//! Receives mc-meet's call events through host mode: appends each to its session log, acks it,
//! ends calls, shows what the extension reports, and turns each finished log into a meeting.

mod call_book;
mod call_log;
mod log_folder;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use native_host::frames::{LengthOrder, read_frame, write_frame};
use native_host::socket::{AppEnd, CallListener};
use pipeline::Pipeline;
use pipeline::settings::MeetSettings;
use serde::{Deserialize, Serialize};
use sources::browser_call::HealthSource;
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncWrite, ReadHalf};
use tokio::sync::watch;
use tracing::Instrument as _;

use self::call_book::{CallBook, EventError, Logged, Now};
use self::log_folder::LogFolder;
use crate::AppState;
use crate::calls::{CallEndReason, CallPlatform};
use crate::finished_logs::{self, LogSource};

/// The protocol this app speaks. An extension that says an older one gets a tray warning.
const PROTOCOL: u32 = 1;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The call book, shared by every connection.
pub(crate) struct BrowserCalls {
    book: Mutex<CallBook>,
    next_port_id: AtomicU64,
    /// Every welcomed port sends mc-meet the new value.
    meet_settings: watch::Sender<MeetSettings>,
}

/// What the tray shows for mc-meet.
#[derive(Debug, Clone)]
pub(crate) struct BrowserCallStatus {
    pub is_connected: bool,
    pub is_extension_outdated: bool,
    pub calls: Vec<CallStatus>,
}

#[derive(Debug, Clone)]
pub(crate) struct CallStatus {
    /// The title Meet showed, else the meeting code.
    pub title: Option<String>,
    pub caption_source: Option<HealthSource>,
    pub health_problem: Option<String>,
}

impl BrowserCalls {
    /// The calls whose logs go in `log_dir`.
    pub fn new(log_dir: PathBuf, meet_settings: MeetSettings) -> Self {
        Self {
            book: Mutex::new(CallBook::new(LogFolder::new(log_dir))),
            next_port_id: AtomicU64::new(0),
            meet_settings: watch::Sender::new(meet_settings),
        }
    }

    /// Sends the settings to every welcomed mc-meet port. A port stays open through a call, so a
    /// save must reach the ports already open.
    pub fn send_settings(&self, meet_settings: MeetSettings) {
        self.meet_settings.send_if_modified(|current| {
            let is_changed = *current != meet_settings;
            *current = meet_settings;
            is_changed
        });
    }

    fn lock(&self) -> MutexGuard<'_, CallBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> BrowserCallStatus {
        self.lock().status()
    }
}

/// Messages from the extension, and the host's first frame.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ExtensionMessage {
    HostHello {
        origin: String,
    },
    Hello {
        protocol: u32,
        #[serde(default)]
        extension_version: Option<String>,
    },
    Event {
        seq: u64,
        session_id: String,
        event: serde_json::Value,
    },
    /// A type from a newer extension.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AppMessage {
    Welcome {
        protocol: u32,
        app_version: &'static str,
        open_sessions: Vec<String>,
        settings: MeetSettingsMessage,
    },
    Ack {
        seq: u64,
    },
    Settings {
        settings: MeetSettingsMessage,
    },
}

/// The settings mc-meet acts on, as the extension reads them.
#[derive(Serialize)]
struct MeetSettingsMessage {
    hide_captions: bool,
}

impl From<MeetSettings> for MeetSettingsMessage {
    fn from(settings: MeetSettings) -> Self {
        Self {
            hide_captions: settings.hide_captions,
        }
    }
}

pub(crate) fn start(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let state = app.state::<AppState>();
    let app_dirs = state.settings.app_dirs();
    register_host(app_dirs.host_manifest_dir());
    let logs = state.browser_calls.lock().logs().clone();
    // Before the listener starts, so a resent event cannot reopen a log this is closing.
    match logs.finish_open_logs() {
        Ok(logs) => {
            state
                .calls
                .end_at_launch(&logs.ended_paths, CallPlatform::Meet);
            let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
            tauri::async_runtime::spawn_blocking(move || {
                finished_logs::record_left_over(
                    &app,
                    &pipeline,
                    LogSource::MeetCall,
                    logs.log_paths,
                    0,
                );
            });
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not read the browser call folder");
        }
    }
    let socket = app_dirs.browser_call_socket().to_path_buf();
    tauri::async_runtime::spawn(listen(app.clone(), Arc::clone(pipeline), socket));
    tauri::async_runtime::spawn(end_idle_calls(app.clone(), Arc::clone(pipeline)));
}

// Chrome reads the manifest each time the extension connects, so a moved or updated app is
// found again after its next start.
fn register_host(data_dir: &Path) {
    // Canonical, since macOS reports the path the app was started with, `..` parts included,
    // and Chrome does not start a host whose path has them.
    let executable = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(executable) => executable,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not find the app executable");
            return;
        }
    };
    match native_host::manifest::register(&executable, data_dir) {
        Ok(registrations) => {
            for registration in registrations {
                match registration.outcome {
                    Ok(()) => tracing::info!(
                        browser.name = registration.browser,
                        file.path = registration.location,
                        "native host registered"
                    ),
                    Err(error) => tracing::warn!(
                        browser.name = registration.browser,
                        file.path = registration.location,
                        exception.message = %error,
                        "could not register the native host"
                    ),
                }
            }
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not register the native host");
        }
    }
}

async fn listen(app: AppHandle, pipeline: Arc<Pipeline>, socket: PathBuf) {
    let mut listener = match CallListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(
                server.address = %socket.display(),
                exception.message = %error,
                "could not listen for mc-meet"
            );
            return;
        }
    };
    tracing::info!(server.address = %socket.display(), "listening for mc-meet");
    loop {
        match listener.accept().await {
            Ok(connection) => {
                let calls = &app.state::<AppState>().browser_calls;
                let port_id = calls.next_port_id.fetch_add(1, Ordering::Relaxed);
                let span = tracing::info_span!("browser_call.port", browser_call.port_id = port_id);
                let port = Port {
                    app: app.clone(),
                    pipeline: Arc::clone(&pipeline),
                    id: port_id,
                };
                tauri::async_runtime::spawn(port.serve(connection).instrument(span));
            }
            Err(error) => {
                tracing::error!(exception.message = %error, "could not accept a host connection");
                // A failing accept usually fails again at once.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// One host-mode connection: one extension port.
struct Port {
    app: AppHandle,
    pipeline: Arc<Pipeline>,
    id: u64,
}

impl Port {
    async fn serve(self, connection: AppEnd) {
        let (from_host, mut to_host) = tokio::io::split(connection);
        match self.relay(from_host, &mut to_host).await {
            Ok(()) => tracing::info!("port closed"),
            Err(error) => tracing::warn!(exception.message = %error, "port closed on an error"),
        }
        self.calls().lock().close_port(self.id);
        crate::tray::refresh(&self.app);
    }

    fn calls(&self) -> &BrowserCalls {
        &self.app.state::<AppState>().inner().browser_calls
    }

    async fn relay(
        &self,
        from_host: ReadHalf<AppEnd>,
        to_host: &mut (impl AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        let mut is_host_known = false;
        let mut meet_settings = self.calls().meet_settings.subscribe();
        let mut is_sending_settings = false;
        // A frame read dropped halfway would lose its bytes, so one read lives across the
        // settings sends and owns the read half until it finishes.
        let mut next_frame = Box::pin(read_next_frame(from_host));
        loop {
            let frame = tokio::select! {
                (from_host, frame) = &mut next_frame => {
                    next_frame.set(read_next_frame(from_host));
                    match frame? {
                        Some(frame) => frame,
                        None => break,
                    }
                },
                changed = meet_settings.changed(), if is_sending_settings => {
                    // A closed sender would end every later wait at once.
                    if changed.is_err() {
                        is_sending_settings = false;
                        continue;
                    }
                    let settings = (*meet_settings.borrow_and_update()).into();
                    send(to_host, &AppMessage::Settings { settings }).await?;
                    continue;
                }
            };
            let message: ExtensionMessage = match serde_json::from_slice(&frame) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(exception.message = %error, "ignored a message that is not valid");
                    continue;
                }
            };
            match message {
                ExtensionMessage::HostHello { origin } => {
                    if origin != native_host::EXTENSION_ORIGIN {
                        tracing::warn!(browser.origin = origin, "refused an unknown extension");
                        return Ok(());
                    }
                    is_host_known = true;
                }
                // Only host mode connects here, and it always speaks first.
                ExtensionMessage::Hello { .. } | ExtensionMessage::Event { .. }
                    if !is_host_known =>
                {
                    tracing::warn!("refused a connection that skipped the host hello");
                    return Ok(());
                }
                ExtensionMessage::Hello {
                    protocol,
                    extension_version,
                } => {
                    tracing::info!(
                        browser.extension.version = extension_version,
                        browser.extension.protocol = protocol,
                        "mc-meet connected"
                    );
                    let open_sessions = self.calls().lock().greet(self.id, protocol);
                    crate::tray::refresh(&self.app);
                    let welcome = AppMessage::Welcome {
                        protocol: PROTOCOL,
                        app_version: env!("CARGO_PKG_VERSION"),
                        open_sessions,
                        settings: (*meet_settings.borrow_and_update()).into(),
                    };
                    send(to_host, &welcome).await?;
                    is_sending_settings = true;
                }
                ExtensionMessage::Event {
                    seq,
                    session_id,
                    event,
                } => {
                    if self.take_event(seq, session_id, &event) {
                        send(to_host, &AppMessage::Ack { seq }).await?;
                    }
                }
                ExtensionMessage::Unknown => {}
            }
        }
        Ok(())
    }

    /// Logs one event and reports whether to ack it.
    fn take_event(&self, seq: u64, session_id: String, event: &serde_json::Value) -> bool {
        let span = tracing::info_span!(
            "browser_call.event",
            browser_call.seq = seq,
            browser_call.session_id = session_id
        );
        let _entered = span.enter();
        let state = self.app.state::<AppState>();
        let logged = {
            let mut book = self.calls().lock();
            let logged = book.take_event(self.id, seq, session_id, event, Now::read());
            // Under the book lock, so the call stream sees opens and ends in the book's order.
            match &logged {
                Ok(Logged::Opened { log_path, title }) => {
                    state
                        .calls
                        .open(log_path, CallPlatform::Meet, title.clone());
                }
                Ok(Logged::Ended { log_path, reason }) => {
                    state.calls.end(log_path, CallEndReason::from(*reason));
                }
                Ok(Logged::Appended | Logged::Duplicate) | Err(_) => {}
            }
            logged
        };
        crate::tray::refresh(&self.app);
        match &logged {
            Ok(Logged::Appended | Logged::Opened { .. } | Logged::Duplicate) => {}
            Ok(Logged::Ended { log_path, .. }) => {
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                let log_path = log_path.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
            }
            Err(error @ (EventError::InvalidSessionId(_) | EventError::InvalidEvent(_))) => {
                tracing::warn!(exception.message = %error, "dropped an event that is not valid");
            }
            Err(error @ EventError::Write(_)) => {
                tracing::error!(exception.message = %error, "could not log an event");
            }
        }
        is_acked(&logged)
    }
}

// An event that cannot be written is not acked, so the extension resends it after the next
// `welcome`. One that is not valid is acked: resending cannot fix it, and the ack drops it from
// the extension's queue.
fn is_acked(logged: &Result<Logged, EventError>) -> bool {
    match logged {
        Ok(Logged::Appended | Logged::Opened { .. } | Logged::Duplicate | Logged::Ended { .. })
        | Err(EventError::InvalidSessionId(_) | EventError::InvalidEvent(_)) => true,
        Err(EventError::Write(_)) => false,
    }
}

async fn read_next_frame(
    mut from_host: ReadHalf<AppEnd>,
) -> (ReadHalf<AppEnd>, std::io::Result<Option<Vec<u8>>>) {
    let frame = read_frame(&mut from_host, LengthOrder::Little).await;
    (from_host, frame)
}

async fn send(
    to_host: &mut (impl AsyncWrite + Unpin),
    message: &AppMessage,
) -> std::io::Result<()> {
    let frame = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    write_frame(to_host, LengthOrder::Little, &frame).await
}

// A browser that quit or crashed sends no `call_ended`, so a timer ends its calls.
async fn end_idle_calls(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(IDLE_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        let ended = {
            let state = app.state::<AppState>();
            let mut book = state.browser_calls.lock();
            let ended = book.end_idle_calls(Now::read());
            // Under the book lock, so a late event cannot reopen the call before this ends it.
            for log_path in &ended {
                state.calls.end(log_path, CallEndReason::Idle);
            }
            ended
        };
        if ended.is_empty() {
            continue;
        }
        crate::tray::refresh(&app);
        for log_path in ended {
            let (app, pipeline) = (app.clone(), Arc::clone(&pipeline));
            tauri::async_runtime::spawn_blocking(move || {
                record_and_run(&app, &pipeline, &log_path);
            });
        }
    }
}

// No minimum length: the caption count alone tells a real call from a misclick.
fn record_and_run(app: &AppHandle, pipeline: &Pipeline, log_path: &Path) {
    finished_logs::record_and_run(app, pipeline, LogSource::MeetCall, log_path, 0);
}
