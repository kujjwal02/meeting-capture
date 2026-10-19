//! The browser call lifecycle: which log each session writes to, which ports hold each call, and
//! when a call ends. The book takes one input at a time, at a time the caller reads, and says
//! what happened. The caller acks, refreshes the tray and starts intake.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize as _;
use sources::browser_call::{CallEvent, CallStart, EndReason, HealthSource};

use super::call_log::CallLog;
use super::log_folder::LogFolder;
use super::{BrowserCallStatus, CallStatus, PROTOCOL};

/// A call no extension port holds ends this long after its last event.
const IDLE_CALL_END: Duration = Duration::from_mins(5);
/// A new session of a meeting whose call ended this long ago or less joins that call's log.
const REJOIN_WINDOW: SignedDuration = SignedDuration::from_mins(10);
const MAX_SESSION_ID_CHARS: usize = 64;

/// The open calls and the greeted ports.
///
/// An open session is always in its call's `sessions`, and a call with no session left is closed.
pub(super) struct CallBook {
    logs: LogFolder,
    /// Open calls by log path. Sessions that rejoined a meeting share its log.
    open_calls: HashMap<PathBuf, OpenCall>,
    /// The log path of each open session.
    open_sessions: HashMap<String, PathBuf>,
    /// Ports that said `hello`, by port id, with the protocol each said.
    greeted_ports: HashMap<u64, u32>,
}

struct OpenCall {
    log: CallLog,
    /// The sessions writing to the log. The call ends when the last one ends.
    sessions: HashSet<String>,
    /// The first session's start, which names the call.
    start: Option<CallStart>,
    /// The ports that sent an event for the call since it opened.
    holder_ports: HashSet<u64>,
    last_event_at: Instant,
    /// The latest event time in the call.
    last_at: Option<Timestamp>,
    caption_source: Option<HealthSource>,
    health_problem: Option<String>,
}

/// One clock reading, taken by the caller for one input.
#[derive(Debug, Clone, Copy)]
pub(super) struct Now {
    /// For idle timing, which must not jump with the wall clock.
    pub(super) instant: Instant,
    /// For the log's `received_at`, and for an end with no event time to use.
    pub(super) timestamp: Timestamp,
}

impl Now {
    pub(super) fn read() -> Self {
        Self {
            instant: Instant::now(),
            timestamp: Timestamp::now(),
        }
    }
}

/// What the book did with one event.
pub(super) enum Logged {
    Appended,
    /// The log already had the event's `seq`.
    Duplicate,
    /// The event ended its call; the log is complete.
    Ended(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum EventError {
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

impl CallBook {
    pub(super) fn new(logs: LogFolder) -> Self {
        Self {
            logs,
            open_calls: HashMap::new(),
            open_sessions: HashMap::new(),
            greeted_ports: HashMap::new(),
        }
    }

    pub(super) fn logs(&self) -> &LogFolder {
        &self.logs
    }

    /// Records the protocol `port_id` said and returns the open sessions, which the extension
    /// resends events for.
    pub(super) fn greet(&mut self, port_id: u64, protocol: u32) -> Vec<String> {
        self.greeted_ports.insert(port_id, protocol);
        self.open_sessions.keys().cloned().collect()
    }

    /// Forgets `port_id`. A call no port holds can end as idle.
    pub(super) fn close_port(&mut self, port_id: u64) {
        self.greeted_ports.remove(&port_id);
        for call in self.open_calls.values_mut() {
            call.holder_ports.remove(&port_id);
        }
    }

    /// Appends one event from `port_id` to its session's log.
    pub(super) fn take_event(
        &mut self,
        port_id: u64,
        seq: u64,
        session_id: String,
        raw_event: &serde_json::Value,
        now: Now,
    ) -> Result<Logged, EventError> {
        if !is_valid_session_id(&session_id) {
            return Err(EventError::InvalidSessionId(session_id));
        }
        let event = CallEvent::deserialize(raw_event).map_err(EventError::InvalidEvent)?;
        let is_end = event.ends_call();
        let log_path = if let Some(log_path) = self.open_sessions.get(&session_id) {
            log_path.clone()
        } else {
            let Some(log_path) = self.open_session(&session_id, seq, &event, now)? else {
                return Ok(Logged::Duplicate);
            };
            log_path
        };
        let call = self
            .open_calls
            .get_mut(&log_path)
            .ok_or_else(|| std::io::Error::other("an open session has no open call"))?;
        if call.log.has_seq(seq) {
            return Ok(Logged::Duplicate);
        }
        call.log
            .append(Some(seq), raw_event, is_end, now.timestamp)?;
        call.holder_ports.insert(port_id);
        call.last_event_at = now.instant;
        call.last_at = call.last_at.max(event.at());
        call.note_event(&event);
        if !is_end {
            return Ok(Logged::Appended);
        }
        call.sessions.remove(&session_id);
        self.open_sessions.remove(&session_id);
        if !call.sessions.is_empty() {
            tracing::info!("browser call session ended; another session goes on");
            return Ok(Logged::Appended);
        }
        self.close_call(&log_path);
        tracing::info!("browser call ended");
        Ok(Logged::Ended(log_path))
    }

    /// Ends each call no port has held for `IDLE_CALL_END` since its last event: the browser
    /// quit or crashed without a `call_ended`. Returns the logs it ended.
    pub(super) fn end_idle_calls(&mut self, now: Now) -> Vec<PathBuf> {
        let idle_logs: Vec<PathBuf> = self
            .open_calls
            .iter()
            .filter(|(_, call)| {
                call.holder_ports.is_empty()
                    && now.instant.duration_since(call.last_event_at) >= IDLE_CALL_END
            })
            .map(|(log_path, _)| log_path.clone())
            .collect();
        idle_logs
            .into_iter()
            .filter_map(|log_path| {
                let mut call = self.close_call(&log_path)?;
                let at = call.last_at.unwrap_or(now.timestamp);
                if let Err(error) = call.log.end(at, EndReason::Idle, now.timestamp) {
                    // Left open, so the next start ends it.
                    tracing::error!(
                        file.path = %log_path.display(),
                        exception.message = %error,
                        "could not end an idle browser call"
                    );
                    return None;
                }
                tracing::info!(file.path = %log_path.display(), "idle browser call ended");
                Some(log_path)
            })
            .collect()
    }

    pub(super) fn status(&self) -> BrowserCallStatus {
        let mut calls: Vec<(&PathBuf, &OpenCall)> = self.open_calls.iter().collect();
        calls.sort_by_key(|(log_path, _)| *log_path);
        BrowserCallStatus {
            is_connected: !self.greeted_ports.is_empty(),
            is_extension_outdated: self
                .greeted_ports
                .values()
                .any(|&protocol| protocol < PROTOCOL),
            calls: calls
                .into_iter()
                .map(|(_, call)| CallStatus {
                    title: call.title(),
                    caption_source: call.caption_source,
                    health_problem: call.health_problem.clone(),
                })
                .collect(),
        }
    }

    // Removes the call and every session still writing to it.
    fn close_call(&mut self, log_path: &Path) -> Option<OpenCall> {
        let call = self.open_calls.remove(log_path)?;
        for session_id in &call.sessions {
            self.open_sessions.remove(session_id);
        }
        Some(call)
    }

    /// Opens the log of a session this run has not seen and returns its path. That is the log
    /// it wrote before a resend or late events, else the log of the meeting it rejoined, else a
    /// new log. Returns `None` when the log already has `seq`.
    fn open_session(
        &mut self,
        session_id: &str,
        seq: u64,
        event: &CallEvent,
        now: Now,
    ) -> Result<Option<PathBuf>, EventError> {
        let log_path = if let Some(log_path) = self.logs.find(session_id)? {
            log_path
        } else if let Some(log_path) = self.rejoined_log(event)? {
            self.logs.record_rejoin(session_id, &log_path)?;
            tracing::info!(file.path = %log_path.display(), "browser call rejoined");
            log_path
        } else {
            let started_at = event.at().unwrap_or(now.timestamp);
            let log = self.logs.create(started_at, session_id)?;
            tracing::info!(file.path = %log.path().display(), "browser call started");
            let log_path = log.path().to_path_buf();
            self.open_calls
                .insert(log_path.clone(), OpenCall::new(log, None, now));
            log_path
        };
        let call = match self.open_calls.entry(log_path.clone()) {
            Entry::Occupied(open) => open.into_mut(),
            // One that already ended: a resend after a quit, late events after an idle end, or
            // a rejoin.
            Entry::Vacant(ended) => {
                let (log, summary) = CallLog::reopen(&log_path)?;
                if log.has_seq(seq) {
                    return Ok(None);
                }
                tracing::info!(file.path = %log_path.display(), "browser call reopened");
                ended.insert(OpenCall::new(log, summary.start, now))
            }
        };
        if call.log.has_seq(seq) {
            return Ok(None);
        }
        call.sessions.insert(session_id.to_owned());
        self.open_sessions
            .insert(session_id.to_owned(), log_path.clone());
        Ok(Some(log_path))
    }

    // A new session of a meeting with an open call, or one that ended within `REJOIN_WINDOW`,
    // joins its log: the user left and joined again, or joined from a new tab after a crash.
    fn rejoined_log(&self, event: &CallEvent) -> std::io::Result<Option<PathBuf>> {
        let meeting_code = CallStart::from_event(event).and_then(|start| start.meeting_code);
        let (Some(at), Some(meeting_code)) = (event.at(), meeting_code) else {
            return Ok(None);
        };
        let open = self.open_calls.iter().find(|(_, call)| {
            call.start
                .as_ref()
                .is_some_and(|start| start.meeting_code.as_ref() == Some(&meeting_code))
        });
        if let Some((log_path, _)) = open {
            return Ok(Some(log_path.clone()));
        }
        let since = at
            .checked_sub(REJOIN_WINDOW)
            .map_err(std::io::Error::other)?;
        self.logs.find_rejoinable(&meeting_code, since)
    }
}

impl OpenCall {
    fn new(log: CallLog, start: Option<CallStart>, now: Now) -> Self {
        Self {
            log,
            sessions: HashSet::new(),
            start,
            holder_ports: HashSet::new(),
            last_event_at: now.instant,
            last_at: None,
            caption_source: None,
            health_problem: None,
        }
    }

    fn title(&self) -> Option<String> {
        let start = self.start.as_ref()?;
        start.title.clone().or_else(|| start.meeting_code.clone())
    }

    // Keeps what the tray shows about the call current.
    fn note_event(&mut self, event: &CallEvent) {
        match event {
            CallEvent::CallStarted { .. } => {
                if self.start.is_none() {
                    self.start = CallStart::from_event(event);
                }
            }
            CallEvent::Health {
                source,
                captions_on,
                parse_failures,
                detail,
                ..
            } => {
                self.caption_source = Some(*source);
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
                self.health_problem = problem.map(|problem| match detail {
                    Some(detail) if !detail.trim().is_empty() => format!("{problem}: {detail}"),
                    Some(_) | None => problem.to_owned(),
                });
                if let Some(problem) = &self.health_problem {
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
}

// The session id names the log file, so it must not hold a path separator.
fn is_valid_session_id(session_id: &str) -> bool {
    (1..=MAX_SESSION_ID_CHARS).contains(&session_id.len())
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}
