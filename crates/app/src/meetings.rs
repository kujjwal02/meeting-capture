//! Turns received captions into jobs and runs the pipeline for them, off the async runtime.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use caption_receiver::ReceivedCaptions;
use jobs::{FailureCause, FailureKind, InputKind, JobState, Meeting, MeetingId, RunOutcome};
use pipeline::{Pipeline, PipelineError};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt as _;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::{Listener, set_listener};

// Picks up meetings a crash, a quit or a store error left unfinished. A dead process's lease
// expires after the jobs lease expiry, so a sweep soon after launch can still find one held.
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
    app.state::<crate::AppState>()
        .settings
        .app_dirs()
        .payload_dir()
}

fn record_and_run(app: &AppHandle, pipeline: &Pipeline, captions: ReceivedCaptions) {
    // A read error on the saved payload also replies 500. TranscripTonic's retry posts it again,
    // and the receiver saves it under a new name.
    let recorded = pipeline.take_in(InputKind::TranscriptonicPayload, &captions.payload_path);
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
    report_run(app, pipeline, id, &pipeline.run(id));
}

fn report_run(
    app: &AppHandle,
    pipeline: &Pipeline,
    id: MeetingId,
    outcome: &Result<RunOutcome, PipelineError>,
) {
    match outcome {
        Ok(RunOutcome::Finished(meeting)) => {
            tracing::info!(meeting.id = %id, meeting.state = meeting.state().label(), "run finished");
            notify_outcome(app, pipeline, meeting);
        }
        Ok(RunOutcome::AlreadyRunning) => {}
        // Step failures are logged and recorded by the pipeline; this covers store errors too.
        Err(error) => tracing::warn!(meeting.id = %id, exception.message = %error, "run stopped"),
    }
    crate::tray::refresh(app);
}

/// Runs the meeting again from its stored input and overwrites the note, edits included.
pub(crate) fn overwrite_edits(app: &AppHandle, pipeline: &Pipeline, id: MeetingId) {
    if let Err(error) = pipeline.reprocess(id, true) {
        tracing::warn!(meeting.id = %id, exception.message = %error, "run stopped");
    }
    crate::tray::refresh(app);
}

// One notification when the note is written, and one when its notes fail for good. A retry that
// writes the note again without notes stays quiet.
fn notify_outcome(app: &AppHandle, pipeline: &Pipeline, meeting: &Meeting) {
    let title = meeting.title.as_deref().unwrap_or("Untitled call");
    let (heading, body) = match meeting.state() {
        JobState::Written { has_notes: true } => ("Note written", format!("{title}, with notes")),
        JobState::Written { has_notes: false } => {
            ("Note written", format!("{title}, transcript only"))
        }
        JobState::ExtractionPending { retry: Some(retry) } if retry.number() == 1 => {
            let retry_at = retry.clock_time(&pipeline.settings().time_zone);
            (
                "Note written",
                format!("{title}, transcript only: the notes failed; next try at {retry_at}"),
            )
        }
        JobState::Failed {
            cause:
                FailureCause {
                    kind: FailureKind::Extraction,
                    ..
                },
        } => (
            "Notes failed",
            format!("{title} keeps the transcript only. The tray shows why."),
        ),
        JobState::Captured
        | JobState::Matched
        | JobState::Extracted
        | JobState::ExtractionPending { .. }
        | JobState::Failed {
            cause:
                FailureCause {
                    kind: FailureKind::Step,
                    ..
                },
        } => return,
    };
    let shown = app
        .notification()
        .builder()
        .title(heading)
        .body(body)
        .show();
    if let Err(error) = shown {
        tracing::warn!(exception.message = %error, "could not show the notification");
    }
}

pub(crate) fn retry_failed(app: &AppHandle, pipeline: &Pipeline) {
    if let Err(error) = pipeline.retry_failed(|id, outcome| report_run(app, pipeline, id, outcome))
    {
        tracing::error!(exception.message = %error, "could not list failed meetings");
    }
}

/// Starts a sweep in the background, unless one is still running.
pub(crate) fn run_unfinished(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
    tauri::async_runtime::spawn_blocking(move || sweep(&app, &pipeline));
}

async fn sweep_unfinished(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        run_unfinished(&app, &pipeline);
    }
}

fn sweep(app: &AppHandle, pipeline: &Pipeline) {
    if let Err(error) = pipeline.sweep(|id, outcome| report_run(app, pipeline, id, outcome)) {
        tracing::error!(exception.message = %error, "could not list unfinished meetings");
    }
}
