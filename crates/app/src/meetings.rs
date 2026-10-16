//! Turns received captions into jobs and runs the pipeline for them, off the async runtime.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use caption_receiver::ReceivedCaptions;
use jobs::MeetingId;
use pipeline::{Pipeline, RunOutcome};
use tauri::AppHandle;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::{Listener, set_listener};

// Picks up meetings a crash, a quit or a store error left unfinished. A dead process's lease
// expires after `jobs::LEASE_EXPIRY`, so a sweep soon after launch can still find one held.
const SWEEP_INTERVAL: Duration = Duration::from_mins(5);

pub(crate) fn start(app: AppHandle, pipeline: Arc<Pipeline>, port: u16) {
    tauri::async_runtime::spawn(sweep_unfinished(app.clone(), Arc::clone(&pipeline)));
    tauri::async_runtime::spawn(receive(app, pipeline, port));
}

async fn receive(app: AppHandle, pipeline: Arc<Pipeline>, port: u16) {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            let reason = format!("cannot listen on {address}: {error}");
            set_listener(&app, Listener::Stopped { reason });
            return;
        }
    };
    set_listener(&app, Listener::Listening { port });

    let (sender, mut received) = mpsc::channel(8);
    let payload_dir = payload_dir(&app);
    let server =
        tauri::async_runtime::spawn(caption_receiver::serve(listener, payload_dir, sender));
    while let Some(captions) = received.recv().await {
        let app = app.clone();
        let pipeline = Arc::clone(&pipeline);
        tauri::async_runtime::spawn_blocking(move || record_and_run(&app, &pipeline, captions));
    }
    let reason = match server.await {
        Ok(Ok(())) => "the receiver stopped".to_owned(),
        Ok(Err(error)) => format!("the receiver stopped: {error}"),
        Err(error) => format!("the receiver crashed: {error}"),
    };
    set_listener(&app, Listener::Stopped { reason });
}

fn payload_dir(app: &AppHandle) -> std::path::PathBuf {
    use tauri::Manager as _;
    app.state::<crate::AppState>().app_dirs.payload_dir()
}

fn record_and_run(app: &AppHandle, pipeline: &Pipeline, captions: ReceivedCaptions) {
    let recorded = pipeline.record_transcriptonic(&captions.payload_path, &captions.transcript);
    let reply = recorded.as_ref().map(|_| ()).map_err(ToString::to_string);
    if captions.recorded.send(reply).is_err() {
        tracing::warn!("TranscripTonic disconnected before the reply");
    }
    let Ok(id) = recorded else {
        // The receiver logged the error and replied 500, so TranscripTonic offers a retry.
        return;
    };
    crate::tray::refresh(app);
    run(app, pipeline, id);
}

// Blocks until the run ends, so callers keep it off the async runtime.
pub(crate) fn run(app: &AppHandle, pipeline: &Pipeline, id: MeetingId) {
    match pipeline.run(id) {
        Ok(RunOutcome::Finished(meeting)) => {
            tracing::info!(meeting.id = %id, meeting.state = meeting.state().label(), "run finished");
        }
        Ok(RunOutcome::AlreadyRunning) => {}
        // Step failures are logged and recorded by the pipeline; this covers store errors too.
        Err(error) => tracing::warn!(meeting.id = %id, exception.message = %error, "run stopped"),
    }
    crate::tray::refresh(app);
}

pub(crate) fn retry_failed(app: &AppHandle, pipeline: &Pipeline) {
    match pipeline.store().failed() {
        Ok(failed) => {
            for meeting in failed {
                run(app, pipeline, meeting.id);
            }
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not list failed meetings");
        }
    }
}

async fn sweep_unfinished(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        let lister = Arc::clone(&pipeline);
        let unfinished =
            tauri::async_runtime::spawn_blocking(move || lister.store().unfinished()).await;
        let unfinished = match unfinished {
            Ok(Ok(unfinished)) => unfinished,
            Ok(Err(error)) => {
                tracing::error!(exception.message = %error, "could not list unfinished meetings");
                continue;
            }
            Err(error) => {
                tracing::error!(exception.message = %error, "listing unfinished meetings crashed");
                continue;
            }
        };
        // One thread per meeting, so a hung run never holds up the others or the next sweep.
        for meeting in unfinished {
            let (app, pipeline) = (app.clone(), Arc::clone(&pipeline));
            tauri::async_runtime::spawn_blocking(move || run(&app, &pipeline, meeting.id));
        }
    }
}
