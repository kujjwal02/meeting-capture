//! Receives mc-meet's call events through host mode: appends each to its session log, acks it,
//! ends calls, feeds the call status board, and turns each finished log into a meeting. It also
//! takes mc-meet's debug material and sends mc-meet the status it shows.

mod call_book;
mod call_log;
mod log_folder;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use native_host::frames::{LengthOrder, read_frame, write_frame};
use native_host::socket::{AppEnd, CallListener};
use pipeline::Pipeline;
use pipeline::settings::MeetSettings;
use serde::{Deserialize, Serialize};
use sources::browser_call::HealthSource;
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncWrite, ReadHalf};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::Instrument as _;

pub(crate) use self::call_book::InvalidSessionId;
use self::call_book::{CallBook, EventError, Logged, Now, OpenedCall, check_session_id};
use self::log_folder::LogFolder;
use crate::AppState;
use crate::call_debug::{FailedFrame, FrameChannel};
use crate::call_status::{
    BoardView, CallBoard, CallState, Moment, Observation, ProblemReason, clock,
};
use crate::calls::{CallEndReason, CallPlatform};
use crate::finished_logs::{self, LogSource};

/// The protocol this app speaks. An extension that says an older one gets a tray warning.
const PROTOCOL: u32 = 1;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Snapshot requests a slow port may fall behind by before it misses some.
const SNAPSHOT_REQUEST_BACKLOG: usize = 16;

/// The call book, shared by every connection.
pub(crate) struct BrowserCalls {
    book: Mutex<CallBook>,
    next_port_id: AtomicU64,
    /// Every welcomed port sends mc-meet the new value.
    meet_settings: watch::Sender<MeetSettings>,
    /// The Meet calls' status, which every welcomed port sends mc-meet when it changes.
    call_status: watch::Sender<CallStatusMessage>,
    /// Session ids whose page snapshot the app wants. Every welcomed port forwards them.
    snapshot_requests: broadcast::Sender<Vec<String>>,
}

/// What the tray shows for mc-meet.
#[derive(Debug, Clone)]
pub(crate) struct BrowserCallStatus {
    pub is_connected: bool,
    pub is_extension_outdated: bool,
}

impl BrowserCalls {
    /// The calls whose logs go in `log_dir`.
    pub fn new(log_dir: PathBuf, meet_settings: MeetSettings) -> Self {
        Self {
            book: Mutex::new(CallBook::new(LogFolder::new(log_dir))),
            next_port_id: AtomicU64::new(0),
            meet_settings: watch::Sender::new(meet_settings),
            call_status: watch::Sender::new(CallStatusMessage::default()),
            snapshot_requests: broadcast::channel(SNAPSHOT_REQUEST_BACKLOG).0,
        }
    }

    /// Sends mc-meet the Meet calls' status when it changed. Times in labels use `time_zone`.
    pub fn publish_call_status(&self, view: &BoardView, time_zone: &TimeZone) {
        let message = {
            let book = self.lock();
            CallStatusMessage {
                calls: view
                    .calls
                    .iter()
                    .filter(|call| call.platform == CallPlatform::Meet)
                    .map(|call| MeetCallStatus::new(call, book.sessions_of(&call.id), time_zone))
                    .collect(),
            }
        };
        self.call_status.send_if_modified(|current| {
            let is_changed = *current != message;
            *current = message;
            is_changed
        });
    }

    /// Takes the user's stop or resume onto the board. Logs it first in an open Meet call's
    /// log, so a reopened call keeps it. Both happen under the book lock, so an event between
    /// them cannot see the book and the board disagree.
    pub fn set_recording_stopped(&self, board: &CallBoard, call_id: &Path, is_stopped: bool) {
        let mut book = self.lock();
        if let Err(error) = book.set_recording_stopped(call_id, is_stopped, Now::read()) {
            tracing::error!(
                call.id = %call_id.display(),
                exception.message = %error,
                "could not log a recording stop or resume"
            );
        }
        board.observe(
            call_id,
            Observation::RecordingStopped(is_stopped),
            Moment::now(),
        );
    }

    /// Asks mc-meet for a page snapshot of each tab in the call.
    pub fn request_page_snapshot(&self, call_id: &Path) {
        let session_ids = self.lock().sessions_of(call_id);
        if session_ids.is_empty() {
            return;
        }
        // An error only means no port is open; the snapshot is then lost, as the page is.
        let _unheard = self.snapshot_requests.send(session_ids);
    }

    /// The log a session writes to, open or ended. `session_id` is checked: it names files.
    pub fn call_of_session(&self, session_id: &str) -> Result<Option<PathBuf>, InvalidSessionId> {
        check_session_id("session_id", session_id)?;
        let book = self.lock();
        if let Some(log_path) = book.log_path_of(session_id) {
            return Ok(Some(log_path));
        }
        Ok(book.logs().find(session_id).ok().flatten())
    }

    /// The sessions of a call: those writing to it now, and the one that created its log.
    pub fn sessions_of_call(&self, call_id: &Path) -> Vec<String> {
        let mut sessions = self.lock().sessions_of(call_id);
        if let Some(first) = log_folder::first_session_id(call_id)
            && !sessions.contains(&first)
        {
            sessions.push(first);
        }
        sessions
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
        /// The sessions the worker has open. Older extensions leave it out.
        #[serde(default)]
        sessions: Vec<String>,
    },
    Event {
        seq: u64,
        session_id: String,
        event: serde_json::Value,
    },
    /// A data channel frame that did not decode. Not queued: one lost on a disconnect is fine.
    FrameFailed {
        session_id: String,
        channel: FrameChannel,
        at: Timestamp,
        reason: String,
        skeleton: String,
        byte_length: u64,
        #[serde(default)]
        bytes_base64: Option<String>,
    },
    /// The page's state, in reply to `snapshot_request`.
    PageSnapshot {
        session_id: String,
        snapshot: serde_json::Value,
    },
    /// The sessions whose tab went away, every one each time the list changes, and after
    /// `welcome`. mc-meet ends each in 5 minutes unless its tab comes back.
    PagesAway {
        session_ids: Vec<String>,
    },
    /// The popup's "Save debug report" button.
    SaveDebugReport {
        /// The popup's tab session, if the tab is in a call.
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default, rename = "with_text")]
        is_with_text: bool,
    },
    /// The popup's "Stop recording this call" for its tab's session.
    StopRecording {
        session_id: String,
    },
    /// The popup's "Resume recording" for its tab's session.
    ResumeRecording {
        session_id: String,
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
        /// The version in the folder the app installs mc-meet into, when it has one.
        folder_version: Option<String>,
        open_sessions: Vec<String>,
        settings: MeetSettingsMessage,
    },
    Ack {
        seq: u64,
    },
    Settings {
        settings: MeetSettingsMessage,
    },
    CallStatus(CallStatusMessage),
    SnapshotRequest {
        session_ids: Vec<String>,
    },
    DebugReportSaved {
        path: String,
    },
    DebugReportFailed {
        error: String,
    },
}

/// The open Meet calls, as mc-meet shows them on its icon and in its popup.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct CallStatusMessage {
    calls: Vec<MeetCallStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MeetCallStatus {
    /// The tabs' sessions writing to the call. mc-meet finds each tab by its session.
    session_ids: Vec<String>,
    title: Option<String>,
    state: StatusWord,
    reason: Option<ProblemReason>,
    reason_text: Option<&'static str>,
    started_at: Timestamp,
    started_at_label: String,
    recording_since_label: Option<String>,
    caption_count: u64,
    last_caption_at: Option<Timestamp>,
    caption_source: Option<CaptionSourceWord>,
    /// The last problems, oldest first.
    problems: Vec<ProblemStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusWord {
    Waiting,
    Recording,
    Problem,
    /// mc-meet stops reading captions and chat for the call's sessions while this holds.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptionSourceWord {
    Channel,
    Dom,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ProblemStatus {
    reason: ProblemReason,
    reason_text: &'static str,
    at: Timestamp,
    at_label: String,
}

impl MeetCallStatus {
    fn new(
        call: &crate::call_status::CallView,
        session_ids: Vec<String>,
        time_zone: &TimeZone,
    ) -> Self {
        let (state, reason) = match call.state {
            CallState::Waiting => (StatusWord::Waiting, None),
            CallState::Recording => (StatusWord::Recording, None),
            CallState::Problem(reason) => (StatusWord::Problem, Some(reason)),
            CallState::Stopped => (StatusWord::Stopped, None),
        };
        let caption_source = call.caption_source.map(|source| match source {
            HealthSource::Channel => CaptionSourceWord::Channel,
            HealthSource::Dom => CaptionSourceWord::Dom,
            HealthSource::None => CaptionSourceWord::None,
        });
        Self {
            session_ids,
            title: call.title.clone(),
            state,
            reason,
            reason_text: reason.map(ProblemReason::describe),
            started_at: call.started_at,
            started_at_label: clock(call.started_at, time_zone),
            recording_since_label: call.recording_since.map(|at| clock(at, time_zone)),
            caption_count: call.caption_count,
            last_caption_at: call.last_caption_at,
            caption_source,
            problems: call
                .problems
                .iter()
                .map(|problem| ProblemStatus {
                    reason: problem.reason,
                    reason_text: problem.reason.describe(),
                    at: problem.at,
                    at_label: clock(problem.at, time_zone),
                })
                .collect(),
        }
    }
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
        let (unheld, is_extension_connected) = {
            let mut book = self.calls().lock();
            (book.close_port(self.id), book.is_extension_connected())
        };
        let state = self.app.state::<AppState>();
        state
            .calls
            .status()
            .set_extension_connected(is_extension_connected);
        for call_id in &unheld {
            state
                .calls
                .status()
                .observe(call_id, Observation::MeetPortHeld(false), Moment::now());
            state
                .call_debug
                .remember(call_id, serde_json::json!({ "type": "port_closed" }));
        }
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
        // Debug reports are written off the runtime and answered through here.
        let (reply_sender, replies) = mpsc::unbounded_channel::<AppMessage>();
        let mut feeds = PortFeeds::new(self.calls(), replies);
        let mut is_welcomed = false;
        // A frame read dropped halfway would lose its bytes, so one read lives across the
        // other sends and owns the read half until it finishes.
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
                message = feeds.next(), if is_welcomed => {
                    send(to_host, &message).await?;
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
                ExtensionMessage::Hello { .. }
                | ExtensionMessage::Event { .. }
                | ExtensionMessage::FrameFailed { .. }
                | ExtensionMessage::PageSnapshot { .. }
                | ExtensionMessage::PagesAway { .. }
                | ExtensionMessage::SaveDebugReport { .. }
                | ExtensionMessage::StopRecording { .. }
                | ExtensionMessage::ResumeRecording { .. }
                    if !is_host_known =>
                {
                    tracing::warn!("refused a connection that skipped the host hello");
                    return Ok(());
                }
                ExtensionMessage::Hello {
                    protocol,
                    extension_version,
                    sessions,
                } => {
                    let open_sessions =
                        self.greet(protocol, extension_version.as_deref(), &sessions);
                    let welcome = AppMessage::Welcome {
                        protocol: PROTOCOL,
                        app_version: env!("CARGO_PKG_VERSION"),
                        folder_version: crate::meet_extension::folder_version(&self.app),
                        open_sessions,
                        settings: (*feeds.meet_settings.borrow_and_update()).into(),
                    };
                    send(to_host, &welcome).await?;
                    let status = feeds.call_status.borrow_and_update().clone();
                    send(to_host, &AppMessage::CallStatus(status)).await?;
                    is_welcomed = true;
                }
                ExtensionMessage::Event {
                    seq,
                    session_id,
                    event,
                } => {
                    if self.take_event(seq, &session_id, &event) {
                        send(to_host, &AppMessage::Ack { seq }).await?;
                    }
                }
                ExtensionMessage::PagesAway { session_ids } => self.take_away_sessions(session_ids),
                ExtensionMessage::StopRecording { session_id } => {
                    self.set_recording_stopped(&session_id, true);
                }
                ExtensionMessage::ResumeRecording { session_id } => {
                    self.set_recording_stopped(&session_id, false);
                }
                message @ (ExtensionMessage::FrameFailed { .. }
                | ExtensionMessage::PageSnapshot { .. }
                | ExtensionMessage::SaveDebugReport { .. }) => {
                    self.take_debug_message(message, &reply_sender);
                }
                ExtensionMessage::Unknown => {}
            }
        }
        Ok(())
    }

    /// Records the hello and returns the open sessions for `welcome`.
    fn greet(
        &self,
        protocol: u32,
        extension_version: Option<&str>,
        worker_sessions: &[String],
    ) -> Vec<String> {
        tracing::info!(
            browser.extension.version = extension_version,
            browser.extension.protocol = protocol,
            "mc-meet connected"
        );
        let (open_sessions, held_calls) =
            self.calls()
                .lock()
                .greet(self.id, protocol, worker_sessions);
        let board = self.app.state::<AppState>().inner().calls.status();
        board.set_extension_connected(true);
        for call_id in &held_calls {
            board.observe(call_id, Observation::MeetPortHeld(true), Moment::now());
        }
        crate::tray::refresh(&self.app);
        open_sessions
    }

    fn take_away_sessions(&self, session_ids: Vec<String>) {
        if let Some(invalid) = session_ids
            .iter()
            .find_map(|session_id| check_session_id("session_ids", session_id).err())
        {
            tracing::warn!(exception.message = %invalid, "ignored a pages_away message that is not valid");
            return;
        }
        let calls = self
            .calls()
            .lock()
            .note_away_sessions(self.id, session_ids.into_iter().collect::<HashSet<_>>());
        let (board, now) = (
            self.app.state::<AppState>().inner().calls.status(),
            Moment::now(),
        );
        for (call_id, is_away) in &calls {
            board.observe(call_id, Observation::MeetPagesAway(*is_away), now);
        }
    }

    /// The popup's stop or resume, for the call its tab's session writes to.
    fn set_recording_stopped(&self, session_id: &str, is_stopped: bool) {
        let call_id = match self.calls().call_of_session(session_id) {
            Ok(Some(call_id)) => call_id,
            Ok(None) => {
                tracing::warn!(
                    browser_call.session_id = session_id,
                    "a recording stop or resume names a session with no call"
                );
                return;
            }
            Err(invalid) => {
                tracing::warn!(exception.message = %invalid, "refused a recording stop or resume");
                return;
            }
        };
        if is_stopped {
            crate::call_capture::stop_call(&self.app, &call_id);
        } else {
            crate::call_capture::resume_call(&self.app, &call_id);
        }
    }

    // Debug material and the popup's report button. None of it is acked or logged as an event.
    fn take_debug_message(
        &self,
        message: ExtensionMessage,
        reply_sender: &mpsc::UnboundedSender<AppMessage>,
    ) {
        match message {
            ExtensionMessage::FrameFailed {
                session_id,
                channel,
                at,
                reason,
                skeleton,
                byte_length,
                bytes_base64,
            } => {
                let frame = FailedFrame {
                    channel,
                    at,
                    reason,
                    skeleton,
                    byte_length,
                    bytes_base64,
                };
                self.take_failed_frame(&session_id, &frame);
            }
            ExtensionMessage::PageSnapshot {
                session_id,
                snapshot,
            } => {
                let Some(call_id) = self.calls().lock().log_path_of(&session_id) else {
                    return;
                };
                self.app
                    .state::<AppState>()
                    .call_debug
                    .record_page_snapshot(&call_id, &session_id, &snapshot);
            }
            ExtensionMessage::SaveDebugReport {
                session_id,
                is_with_text,
            } => {
                if let Some(Err(invalid)) = session_id
                    .as_deref()
                    .map(|session_id| check_session_id("session_id", session_id))
                {
                    tracing::warn!(exception.message = %invalid, "refused a debug report request");
                    let _unheard = reply_sender.send(AppMessage::DebugReportFailed {
                        error: invalid.to_string(),
                    });
                    return;
                }
                let (app, reply_sender) = (self.app.clone(), reply_sender.clone());
                tauri::async_runtime::spawn_blocking(move || {
                    let reply = match crate::call_debug::save_debug_report(
                        &app,
                        session_id.as_deref(),
                        is_with_text,
                    ) {
                        Ok(folder) => AppMessage::DebugReportSaved {
                            path: folder.display().to_string(),
                        },
                        Err(error) => AppMessage::DebugReportFailed {
                            error: error.to_string(),
                        },
                    };
                    // An error only means the port closed before the report was written.
                    let _unheard = reply_sender.send(reply);
                });
            }
            ExtensionMessage::HostHello { .. }
            | ExtensionMessage::Hello { .. }
            | ExtensionMessage::Event { .. }
            | ExtensionMessage::PagesAway { .. }
            | ExtensionMessage::StopRecording { .. }
            | ExtensionMessage::ResumeRecording { .. }
            | ExtensionMessage::Unknown => {}
        }
    }

    /// Logs one event and reports whether to ack it.
    fn take_event(&self, seq: u64, session_id: &str, event: &serde_json::Value) -> bool {
        let span = tracing::info_span!(
            "browser_call.event",
            browser_call.seq = seq,
            browser_call.session_id = session_id
        );
        let _entered = span.enter();
        let state = self.app.state::<AppState>();
        let lead_up_line = serde_json::json!({
            "seq": seq,
            "session_id": session_id,
            "event": event,
        });
        let logged = {
            let mut book = self.calls().lock();
            let logged = book.take_event(self.id, seq, session_id, event, Now::read());
            // Under the book lock, so the call stream sees opens and ends in the book's order.
            match &logged {
                Ok(
                    Logged::Opened { opened, .. }
                    | Logged::Withheld {
                        opened: Some(opened),
                    },
                ) => open_on_stream(&state, opened),
                Ok(Logged::Ended { log_path, reason }) => {
                    state.calls.end(log_path, CallEndReason::from(*reason));
                }
                Ok(
                    Logged::Appended { .. } | Logged::Duplicate | Logged::Withheld { opened: None },
                )
                | Err(_) => {}
            }
            logged
        };
        let board = state.calls.status();
        match &logged {
            Ok(
                Logged::Appended {
                    log_path,
                    observation,
                }
                | Logged::Opened {
                    opened: OpenedCall { log_path, .. },
                    observation,
                },
            ) => {
                let now = Moment::now();
                board.observe(log_path, Observation::MeetPortHeld(true), now);
                if let Some(observation) = observation {
                    board.observe(log_path, observation.clone(), now);
                }
                state.call_debug.remember(log_path, lead_up_line);
            }
            // Its caption text stays out of the lead-up: the user stopped recording.
            Ok(Logged::Withheld {
                opened: Some(OpenedCall { log_path, .. }),
            }) => board.observe(log_path, Observation::MeetPortHeld(true), Moment::now()),
            Ok(Logged::Ended { log_path, .. }) => state.call_debug.forget_lead_up(log_path),
            Ok(Logged::Duplicate | Logged::Withheld { opened: None }) | Err(_) => {}
        }
        // Other events reach the tray with the status tick. A rebuild closes an open menu.
        match &logged {
            Ok(
                Logged::Opened { .. } | Logged::Withheld { opened: Some(_) } | Logged::Ended { .. },
            ) => crate::tray::refresh(&self.app),
            Ok(Logged::Appended { .. } | Logged::Duplicate | Logged::Withheld { opened: None })
            | Err(_) => {}
        }
        match &logged {
            Ok(
                Logged::Appended { .. }
                | Logged::Opened { .. }
                | Logged::Duplicate
                | Logged::Withheld { .. },
            ) => {}
            Ok(Logged::Ended { log_path, .. }) => {
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                let log_path = log_path.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
            }
            Err(
                error @ (EventError::InvalidSessionId(_)
                | EventError::InvalidEvent(_)
                | EventError::AppOnlyEvent),
            ) => {
                tracing::warn!(exception.message = %error, "dropped an event that is not valid");
            }
            Err(error @ EventError::Write(_)) => {
                tracing::error!(exception.message = %error, "could not log an event");
            }
        }
        is_acked(&logged)
    }

    // A frame of a session the app has no open call for is dropped: its call ended, or its
    // first event has not arrived yet.
    fn take_failed_frame(&self, session_id: &str, frame: &FailedFrame) {
        let Some(call_id) = self.calls().lock().log_path_of(session_id) else {
            return;
        };
        tracing::warn!(
            call.id = %call_id.display(),
            browser_call.session_id = session_id,
            browser_call.channel = frame.channel.label(),
            error.type = frame.reason,
            "mc-meet could not decode a frame"
        );
        self.app
            .state::<AppState>()
            .call_debug
            .record_failed_frame(&call_id, frame);
    }
}

// An event that cannot be written is not acked, so the extension resends it after the next
// `welcome`. One that is not valid is acked: resending cannot fix it, and the ack drops it from
// the extension's queue.
fn is_acked(logged: &Result<Logged, EventError>) -> bool {
    match logged {
        Ok(
            Logged::Appended { .. }
            | Logged::Opened { .. }
            | Logged::Duplicate
            | Logged::Withheld { .. }
            | Logged::Ended { .. },
        )
        | Err(
            EventError::InvalidSessionId(_)
            | EventError::InvalidEvent(_)
            | EventError::AppOnlyEvent,
        ) => true,
        Err(EventError::Write(_)) => false,
    }
}

/// What the app sends a welcomed port unasked: settings, call status, snapshot requests and
/// debug report replies.
struct PortFeeds {
    meet_settings: watch::Receiver<MeetSettings>,
    call_status: watch::Receiver<CallStatusMessage>,
    snapshot_requests: broadcast::Receiver<Vec<String>>,
    replies: mpsc::UnboundedReceiver<AppMessage>,
    // A closed sender would end every later wait at once.
    is_settings_open: bool,
    is_call_status_open: bool,
    is_snapshot_open: bool,
}

impl PortFeeds {
    fn new(calls: &BrowserCalls, replies: mpsc::UnboundedReceiver<AppMessage>) -> Self {
        Self {
            meet_settings: calls.meet_settings.subscribe(),
            call_status: calls.call_status.subscribe(),
            snapshot_requests: calls.snapshot_requests.subscribe(),
            replies,
            is_settings_open: true,
            is_call_status_open: true,
            is_snapshot_open: true,
        }
    }

    /// The next message to send. Every wait in it can be dropped without losing a message.
    async fn next(&mut self) -> AppMessage {
        loop {
            tokio::select! {
                changed = self.meet_settings.changed(), if self.is_settings_open => {
                    if changed.is_err() {
                        self.is_settings_open = false;
                        continue;
                    }
                    let settings = (*self.meet_settings.borrow_and_update()).into();
                    return AppMessage::Settings { settings };
                }
                changed = self.call_status.changed(), if self.is_call_status_open => {
                    if changed.is_err() {
                        self.is_call_status_open = false;
                        continue;
                    }
                    return AppMessage::CallStatus(self.call_status.borrow_and_update().clone());
                }
                request = self.snapshot_requests.recv(), if self.is_snapshot_open => {
                    match request {
                        Ok(session_ids) => return AppMessage::SnapshotRequest { session_ids },
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::warn!(
                                browser_call.missed_requests = missed,
                                "dropped page snapshot requests"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => self.is_snapshot_open = false,
                    }
                }
                Some(reply) = self.replies.recv() => return reply,
                else => return std::future::pending().await,
            }
        }
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
                state.call_debug.forget_lead_up(log_path);
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

// Under the book lock, so the call stream sees opens and ends in the book's order.
fn open_on_stream(state: &AppState, opened: &OpenedCall) {
    state.calls.open(
        &opened.log_path,
        CallPlatform::Meet,
        opened.title.clone(),
        None,
        opened.is_recording_stopped,
    );
    if !opened.logged_caption_keys.is_empty() {
        state.calls.status().observe(
            &opened.log_path,
            Observation::MeetLoggedCaptions(opened.logged_caption_keys.clone()),
            Moment::now(),
        );
    }
}
