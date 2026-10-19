//! Receives mc-meet's call events through host mode: appends each to its session log, acks it,
//! ends calls, shows what the extension reports, and turns each finished log into a meeting.

mod call_log;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jobs::InputKind;
use native_host::frames::{LengthOrder, read_frame, write_frame};
use native_host::socket::{AppEnd, CallListener};
use pipeline::{InputError, Intake, IntakeError, Pipeline};
use serde::{Deserialize, Serialize};
use sources::browser_call::{CallEvent, EndReason, HealthSource};
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncWrite, ReadHalf};
use tracing::Instrument as _;

use self::call_log::CallLog;
use crate::AppState;

/// The protocol this app speaks. An extension that says an older one gets a tray warning.
const PROTOCOL: u32 = 1;
/// A call no extension port holds ends this long after its last event.
const IDLE_CALL_END: Duration = Duration::from_mins(5);
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const MAX_SESSION_ID_CHARS: usize = 64;

/// The open calls and the extension ports, shared by every connection.
#[derive(Default)]
pub(crate) struct BrowserCalls {
    registry: Mutex<Registry>,
    next_port_id: AtomicU64,
}

#[derive(Default)]
struct Registry {
    open_calls: HashMap<String, OpenCall>,
    /// Ports that said `hello`, by port id, with the protocol each said.
    greeted_ports: HashMap<u64, u32>,
}

struct OpenCall {
    log: CallLog,
    /// The ports that sent an event for the call since it opened.
    holder_ports: HashSet<u64>,
    last_event_at: Instant,
    /// The latest event time in the call.
    last_at: Option<Timestamp>,
    /// The title Meet showed, else the meeting code.
    title: Option<String>,
    caption_source: Option<HealthSource>,
    health_problem: Option<String>,
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
    pub title: Option<String>,
    pub caption_source: Option<HealthSource>,
    pub health_problem: Option<String>,
}

impl BrowserCalls {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> BrowserCallStatus {
        let registry = self.lock();
        let mut calls: Vec<(&String, &OpenCall)> = registry.open_calls.iter().collect();
        calls.sort_by_key(|(session_id, _)| *session_id);
        BrowserCallStatus {
            is_connected: !registry.greeted_ports.is_empty(),
            is_extension_outdated: registry
                .greeted_ports
                .values()
                .any(|&protocol| protocol < PROTOCOL),
            calls: calls
                .into_iter()
                .map(|(_, call)| CallStatus {
                    title: call.title.clone(),
                    caption_source: call.caption_source,
                    health_problem: call.health_problem.clone(),
                })
                .collect(),
        }
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
    },
    Ack {
        seq: u64,
    },
}

/// What the app did with one event.
enum Logged {
    Appended,
    /// The log already had the event's `seq`.
    Duplicate,
    /// The event ended its call; the log is complete.
    Ended(PathBuf),
}

#[derive(Debug, thiserror::Error)]
enum EventError {
    #[error(
        "field `session_id` must be 1 to {MAX_SESSION_ID_CHARS} letters, digits or hyphens, \
         not {0:?}"
    )]
    InvalidSessionId(String),
    #[error("field `event` is not a valid call event: {0}")]
    InvalidEvent(serde_json::Error),
    #[error("could not write the browser call log: {0}")]
    Write(#[from] std::io::Error),
}

pub(crate) fn start(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let state = app.state::<AppState>();
    let app_dirs = state.settings.app_dirs();
    register_host(app_dirs.host_manifest_dir());
    let log_dir = app_dirs.browser_call_log_dir();
    // Before the listener starts, so a resent event cannot reopen a log this is closing.
    match call_log::finish_open_logs(&log_dir) {
        Ok(logs) => {
            let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
            tauri::async_runtime::spawn_blocking(move || record_left_over(&app, &pipeline, logs));
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not read the browser call folder");
        }
    }
    let socket = app_dirs.browser_call_socket().to_path_buf();
    tauri::async_runtime::spawn(listen(
        app.clone(),
        Arc::clone(pipeline),
        socket,
        log_dir.clone(),
    ));
    tauri::async_runtime::spawn(end_idle_calls(app.clone(), Arc::clone(pipeline)));
}

// Chrome reads the manifest each time the extension connects, so a moved or updated app is
// found again after its next start.
fn register_host(data_dir: &Path) {
    let executable = match std::env::current_exe() {
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

// Logs the last run did not turn into meetings: the app quit or crashed during the call, or
// before its note.
fn record_left_over(app: &AppHandle, pipeline: &Pipeline, logs: Vec<PathBuf>) {
    for log_path in logs {
        match pipeline.store().has_input(&log_path) {
            Ok(true) => {}
            Ok(false) => record_and_run(app, pipeline, &log_path),
            Err(error) => {
                tracing::error!(exception.message = %error, "could not check a browser call log");
            }
        }
    }
}

async fn listen(app: AppHandle, pipeline: Arc<Pipeline>, socket: PathBuf, log_dir: PathBuf) {
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
                    log_dir: log_dir.clone(),
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
    log_dir: PathBuf,
    id: u64,
}

impl Port {
    async fn serve(self, connection: AppEnd) {
        let (mut from_host, mut to_host) = tokio::io::split(connection);
        match self.relay(&mut from_host, &mut to_host).await {
            Ok(()) => tracing::info!("port closed"),
            Err(error) => tracing::warn!(exception.message = %error, "port closed on an error"),
        }
        let mut registry = self.calls().lock();
        registry.greeted_ports.remove(&self.id);
        for call in registry.open_calls.values_mut() {
            call.holder_ports.remove(&self.id);
        }
        drop(registry);
        crate::tray::refresh(&self.app);
    }

    fn calls(&self) -> &BrowserCalls {
        &self.app.state::<AppState>().inner().browser_calls
    }

    async fn relay(
        &self,
        from_host: &mut ReadHalf<AppEnd>,
        to_host: &mut (impl AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        let mut is_host_known = false;
        while let Some(frame) = read_frame(from_host, LengthOrder::Little).await? {
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
                    let open_sessions = {
                        let mut registry = self.calls().lock();
                        registry.greeted_ports.insert(self.id, protocol);
                        registry.open_calls.keys().cloned().collect()
                    };
                    crate::tray::refresh(&self.app);
                    let welcome = AppMessage::Welcome {
                        protocol: PROTOCOL,
                        app_version: env!("CARGO_PKG_VERSION"),
                        open_sessions,
                    };
                    send(to_host, &welcome).await?;
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

    /// Logs one event and reports whether to ack it. An event that cannot be written is not
    /// acked, so the extension resends it after the next `welcome`.
    fn take_event(&self, seq: u64, session_id: String, event: &serde_json::Value) -> bool {
        let span = tracing::info_span!(
            "browser_call.event",
            browser_call.seq = seq,
            browser_call.session_id = session_id
        );
        let _entered = span.enter();
        let logged = self.log_event(seq, session_id, event);
        crate::tray::refresh(&self.app);
        match logged {
            Ok(Logged::Appended | Logged::Duplicate) => true,
            Ok(Logged::Ended(log_path)) => {
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
                true
            }
            // Resending cannot fix it, so the ack drops it from the extension's queue.
            Err(error @ (EventError::InvalidSessionId(_) | EventError::InvalidEvent(_))) => {
                tracing::warn!(exception.message = %error, "dropped an event that is not valid");
                true
            }
            Err(error @ EventError::Write(_)) => {
                tracing::error!(exception.message = %error, "could not log an event");
                false
            }
        }
    }

    fn log_event(
        &self,
        seq: u64,
        session_id: String,
        raw_event: &serde_json::Value,
    ) -> Result<Logged, EventError> {
        if !is_valid_session_id(&session_id) {
            return Err(EventError::InvalidSessionId(session_id));
        }
        let event = CallEvent::deserialize(raw_event).map_err(EventError::InvalidEvent)?;
        let is_end = call_log::is_end(&event);
        let mut registry = self.calls().lock();
        let call = match registry.open_calls.entry(session_id.clone()) {
            Entry::Occupied(open) => {
                let call = open.into_mut();
                if call.log.has_seq(seq) {
                    return Ok(Logged::Duplicate);
                }
                call
            }
            // A call this run has not seen, or one that already ended: a resend after a quit,
            // or late events after an idle end.
            Entry::Vacant(unseen) => {
                let log = if let Some(log_path) = call_log::find(&self.log_dir, unseen.key())? {
                    let log = CallLog::reopen(&log_path)?;
                    if log.has_seq(seq) {
                        return Ok(Logged::Duplicate);
                    }
                    tracing::info!(file.path = %log_path.display(), "browser call reopened");
                    log
                } else {
                    let started_at = event.at().unwrap_or_else(Timestamp::now);
                    let log = CallLog::create(&self.log_dir, started_at, unseen.key())?;
                    tracing::info!(file.path = %log.path().display(), "browser call started");
                    log
                };
                let title = read_title(log.path());
                unseen.insert(OpenCall {
                    log,
                    holder_ports: HashSet::new(),
                    last_event_at: Instant::now(),
                    last_at: None,
                    title,
                    caption_source: None,
                    health_problem: None,
                })
            }
        };
        call.log.append(Some(seq), raw_event, is_end)?;
        call.holder_ports.insert(self.id);
        call.last_event_at = Instant::now();
        call.last_at = call.last_at.max(event.at());
        note_event(call, &event);
        if is_end {
            let ended = registry.open_calls.remove(&session_id);
            let log_path = ended.map(|call| call.log.path().to_path_buf());
            tracing::info!("browser call ended");
            return Ok(log_path.map_or(Logged::Appended, Logged::Ended));
        }
        Ok(Logged::Appended)
    }
}

async fn send(
    to_host: &mut (impl AsyncWrite + Unpin),
    message: &AppMessage,
) -> std::io::Result<()> {
    let frame = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    write_frame(to_host, LengthOrder::Little, &frame).await
}

// The session id names the log file, so it must not hold a path separator.
fn is_valid_session_id(session_id: &str) -> bool {
    (1..=MAX_SESSION_ID_CHARS).contains(&session_id.len())
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

// The title of a reopened call, from its `call_started` line.
fn read_title(log_path: &Path) -> Option<String> {
    let contents = std::fs::read(log_path).ok()?;
    let lines = sources::browser_call::read_lines(&contents).ok()?;
    lines.iter().find_map(|line| call_title(&line.event))
}

fn call_title(event: &CallEvent) -> Option<String> {
    match event {
        CallEvent::CallStarted {
            title,
            meeting_code,
            ..
        } => Some(
            title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| meeting_code.clone()),
        ),
        CallEvent::Caption { .. }
        | CallEvent::Chat { .. }
        | CallEvent::Roster { .. }
        | CallEvent::Health { .. }
        | CallEvent::CallEnded { .. }
        | CallEvent::Unknown => None,
    }
}

// Keeps what the tray shows about the call current.
fn note_event(call: &mut OpenCall, event: &CallEvent) {
    match event {
        CallEvent::CallStarted { .. } => {
            if let Some(title) = call_title(event) {
                call.title = Some(title);
            }
        }
        CallEvent::Health {
            source,
            captions_on,
            parse_failures,
            detail,
            ..
        } => {
            call.caption_source = Some(*source);
            let problem = match source {
                HealthSource::None => Some("no captions found"),
                HealthSource::Channel | HealthSource::Dom if !captions_on => {
                    Some("captions are off")
                }
                HealthSource::Channel | HealthSource::Dom if *parse_failures > 0 => {
                    Some("some captions could not be read")
                }
                HealthSource::Channel | HealthSource::Dom => None,
            };
            call.health_problem = problem.map(|problem| match detail {
                Some(detail) if !detail.trim().is_empty() => format!("{problem}: {detail}"),
                Some(_) | None => problem.to_owned(),
            });
            if let Some(problem) = &call.health_problem {
                tracing::warn!(exception.message = problem, "mc-meet health problem");
            }
        }
        CallEvent::Caption { .. }
        | CallEvent::Chat { .. }
        | CallEvent::Roster { .. }
        | CallEvent::CallEnded { .. }
        | CallEvent::Unknown => {}
    }
}

// Ends each call no port has held for `IDLE_CALL_END` since its last event: the browser quit or
// crashed without a `call_ended`.
async fn end_idle_calls(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(IDLE_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        let ended = {
            let calls = &app.state::<AppState>().browser_calls;
            let mut registry = calls.lock();
            let idle_sessions: Vec<String> = registry
                .open_calls
                .iter()
                .filter(|(_, call)| {
                    call.holder_ports.is_empty() && call.last_event_at.elapsed() >= IDLE_CALL_END
                })
                .map(|(session_id, _)| session_id.clone())
                .collect();
            idle_sessions
                .into_iter()
                .filter_map(|session_id| {
                    let mut call = registry.open_calls.remove(&session_id)?;
                    let at = call.last_at.unwrap_or_else(Timestamp::now);
                    if let Err(error) = call.log.end(at, EndReason::Idle) {
                        // Left open, so the next start ends it.
                        tracing::error!(
                            file.path = %call.log.path().display(),
                            exception.message = %error,
                            "could not end an idle browser call"
                        );
                        return None;
                    }
                    tracing::info!(
                        file.path = %call.log.path().display(),
                        "idle browser call ended"
                    );
                    Some(call.log.path().to_path_buf())
                })
                .collect::<Vec<_>>()
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

fn record_and_run(app: &AppHandle, pipeline: &Pipeline, log_path: &Path) {
    let _span =
        tracing::info_span!("browser_call.record", file.path = %log_path.display()).entered();
    // No minimum length: the caption count alone tells a real call from a misclick.
    let taken_in = pipeline.take_in_unless_short(InputKind::BrowserCallLog, log_path, 0);
    match taken_in {
        Ok(Intake::Recorded(id)) => {
            crate::tray::refresh(app);
            crate::meetings::run(app, pipeline, id);
        }
        // Its log stays, and `mc ingest` can still write its note.
        Ok(Intake::TooShort) => {}
        Err(IntakeError::Input(error)) => {
            tracing::error!(exception.message = %error, "could not read a browser call log");
            // The banner has little room, and the span already names the file.
            let cause = match &error {
                InputError::Read { source, .. } => source.to_string(),
                InputError::ParseTranscriptonic(_)
                | InputError::ParseSlackHuddle(_)
                | InputError::ParseBrowserCall(_) => error.to_string(),
            };
            crate::huddles::notify(app, "Meet call not recorded", &cause);
            crate::huddles::set_aside(log_path);
        }
        Err(error @ (IntakeError::Copy { .. } | IntakeError::Store(_))) => {
            tracing::error!(exception.message = %error, "could not record a browser call");
            crate::huddles::notify(app, "Meet call not recorded", &error.to_string());
        }
    }
}
