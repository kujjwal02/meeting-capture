//! The browser call lifecycle: which log each session writes to, which ports hold each call, and
//! when a call ends. The book takes one input at a time, at a time the caller reads, and says
//! what happened. The caller acks, refreshes the tray and starts intake.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize as _;
use sources::browser_call::{CallEvent, CallStart, EndReason};

use super::call_log::{self, CallLog};
use super::log_folder::LogFolder;
use super::{BrowserCallStatus, PROTOCOL};
use crate::call_status::{MeetCaptions, Observation};

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
    /// The sessions whose tab went away, by the port that said so. mc-meet ends each in 5
    /// minutes unless its tab comes back.
    away_sessions: HashMap<u64, HashSet<String>>,
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
    /// Whether the book returned the call in `Opened` or `Withheld`. `Opened` sets it only after
    /// its write, so a resent first event still reports it when the first write failed.
    is_reported: bool,
    /// The caption line keys a reopened log already held, for the call stream.
    logged_caption_keys: Vec<String>,
    /// The user stopped recording the call. Its log takes no caption or chat until a resume.
    is_recording_stopped: bool,
    /// Caption lines, by device and utterance id, seen since the last stop or resume.
    live_utterances: HashSet<(String, String)>,
    /// Caption lines a stop or resume cut. One that grows on would mix words from both sides of
    /// the change, so its later versions are withheld.
    cut_utterances: HashSet<(String, String)>,
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
    Appended {
        log_path: PathBuf,
        observation: Option<Observation>,
    },
    /// The call's first event written since it opened in this run, other than an end.
    Opened {
        opened: OpenedCall,
        observation: Option<Observation>,
    },
    /// The log already had the event's `seq`.
    Duplicate,
    /// A caption or chat message of a call the user stopped recording, or a caption line a stop
    /// or resume cut. Acked, not logged. `opened` is set when it was the call's first event in
    /// this run, as after a restart while the call was stopped.
    Withheld { opened: Option<OpenedCall> },
    /// The event ended its call; the log is complete.
    Ended {
        log_path: PathBuf,
        reason: EndReason,
    },
}

/// A call the book opened in this run, for the call stream.
pub(super) struct OpenedCall {
    pub(super) log_path: PathBuf,
    /// The title Meet showed, else the meeting code.
    pub(super) title: Option<String>,
    /// The caption line keys the log held before this run, when the call was reopened.
    pub(super) logged_caption_keys: Vec<String>,
    /// The log's last stop or resume was a stop, so the call opens stopped.
    pub(super) is_recording_stopped: bool,
}

/// A session id names files, so only a checked one reaches a path.
#[derive(Debug, thiserror::Error)]
#[error(
    "field `{field}` must hold 1 to {MAX_SESSION_ID_CHARS} letters, digits or hyphens, not {id:?}"
)]
pub(crate) struct InvalidSessionId {
    field: &'static str,
    id: String,
}

/// Checks a session id from the extension, which `field` of its message held.
pub(super) fn check_session_id(field: &'static str, id: &str) -> Result<(), InvalidSessionId> {
    if is_valid_session_id(id) {
        Ok(())
    } else {
        Err(InvalidSessionId {
            field,
            id: id.to_owned(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum EventError {
    #[error(transparent)]
    InvalidSessionId(#[from] InvalidSessionId),
    #[error("field `event` is not a valid call event: {0}")]
    InvalidEvent(serde_json::Error),
    #[error("only the app writes recording_stopped and recording_resumed")]
    AppOnlyEvent,
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
            away_sessions: HashMap::new(),
        }
    }

    pub(super) fn logs(&self) -> &LogFolder {
        &self.logs
    }

    /// Records the protocol `port_id` said, and that it holds the calls of `worker_sessions`,
    /// the sessions its worker has open. Returns the open sessions, which the extension resends
    /// events for, and the calls the port holds.
    pub(super) fn greet(
        &mut self,
        port_id: u64,
        protocol: u32,
        worker_sessions: &[String],
    ) -> (Vec<String>, Vec<PathBuf>) {
        self.greeted_ports.insert(port_id, protocol);
        let mut held = Vec::new();
        for session_id in worker_sessions {
            let Some(log_path) = self.open_sessions.get(session_id) else {
                continue;
            };
            if let Some(call) = self.open_calls.get_mut(log_path) {
                call.holder_ports.insert(port_id);
                held.push(log_path.clone());
            }
        }
        (self.open_sessions.keys().cloned().collect(), held)
    }

    /// Forgets `port_id`, and returns the calls no port holds now. A call no port holds can end
    /// as idle.
    pub(super) fn close_port(&mut self, port_id: u64) -> Vec<PathBuf> {
        self.greeted_ports.remove(&port_id);
        self.away_sessions.remove(&port_id);
        self.open_calls
            .iter_mut()
            .filter_map(|(log_path, call)| {
                let is_unheld = call.holder_ports.remove(&port_id) && call.holder_ports.is_empty();
                is_unheld.then(|| log_path.clone())
            })
            .collect()
    }

    /// Records the sessions `port_id` says went away, all of them each time. Returns each open
    /// call with whether all of its sessions went away.
    pub(super) fn note_away_sessions(
        &mut self,
        port_id: u64,
        session_ids: HashSet<String>,
    ) -> Vec<(PathBuf, bool)> {
        self.away_sessions.insert(port_id, session_ids);
        self.open_calls
            .iter()
            .map(|(log_path, call)| {
                let is_away = call.sessions.iter().all(|session_id| {
                    self.away_sessions
                        .values()
                        .any(|away| away.contains(session_id))
                });
                (log_path.clone(), is_away)
            })
            .collect()
    }

    /// The ports that said `hello` and are still open.
    pub(super) fn greeted_port_ids(&self) -> HashSet<u64> {
        self.greeted_ports.keys().copied().collect()
    }

    /// Whether any port said `hello` and is still open.
    pub(super) fn is_extension_connected(&self) -> bool {
        !self.greeted_ports.is_empty()
    }

    /// The log of an open session.
    pub(super) fn log_path_of(&self, session_id: &str) -> Option<PathBuf> {
        self.open_sessions.get(session_id).cloned()
    }

    /// The sessions writing to an open call's log.
    pub(super) fn sessions_of(&self, log_path: &Path) -> Vec<String> {
        let mut sessions: Vec<String> = self
            .open_calls
            .get(log_path)
            .map(|call| call.sessions.iter().cloned().collect())
            .unwrap_or_default();
        sessions.sort();
        sessions
    }

    /// Appends one event from `port_id` to its session's log.
    pub(super) fn take_event(
        &mut self,
        port_id: u64,
        seq: u64,
        session_id: &str,
        raw_event: &serde_json::Value,
        now: Now,
    ) -> Result<Logged, EventError> {
        check_session_id("session_id", session_id)?;
        let event = CallEvent::deserialize(raw_event).map_err(EventError::InvalidEvent)?;
        if matches!(
            event,
            CallEvent::RecordingStopped { .. } | CallEvent::RecordingResumed { .. }
        ) {
            return Err(EventError::AppOnlyEvent);
        }
        let end_reason = event.end_reason();
        let is_end = end_reason.is_some();
        let log_path = if let Some(log_path) = self.open_sessions.get(session_id) {
            log_path.clone()
        } else {
            let Some(log_path) = self.open_session(session_id, seq, &event, now)? else {
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
        let is_withheld = call.is_withheld(&event);
        call.holder_ports.insert(port_id);
        call.last_event_at = now.instant;
        if is_withheld {
            let opened = (!call.is_reported).then(|| call.take_opened(&log_path));
            return Ok(Logged::Withheld { opened });
        }
        call.log
            .append(Some(seq), raw_event, is_end, now.timestamp)?;
        call.last_at = call.last_at.max(event.at());
        call.note_event(&event);
        let Some(reason) = end_reason else {
            let observation = observation_of(&event);
            if !call.is_reported {
                return Ok(Logged::Opened {
                    opened: call.take_opened(&log_path),
                    observation,
                });
            }
            return Ok(Logged::Appended {
                log_path,
                observation,
            });
        };
        call.sessions.remove(session_id);
        self.open_sessions.remove(session_id);
        if !call.sessions.is_empty() {
            tracing::info!("browser call session ended; another session goes on");
            return Ok(Logged::Appended {
                log_path,
                observation: None,
            });
        }
        self.close_call(&log_path);
        tracing::info!("browser call ended");
        Ok(Logged::Ended { log_path, reason })
    }

    /// Logs the user's stop or resume of an open call, when it changes the call.
    pub(super) fn set_recording_stopped(
        &mut self,
        log_path: &Path,
        is_stopped: bool,
        now: Now,
    ) -> std::io::Result<()> {
        let Some(call) = self.open_calls.get_mut(log_path) else {
            return Ok(());
        };
        if call.is_recording_stopped != is_stopped {
            let at = now.timestamp;
            call.log.append(
                None,
                &call_log::recording_event(is_stopped, at),
                false,
                now.timestamp,
            )?;
            call.is_recording_stopped = is_stopped;
            let cut = std::mem::take(&mut call.live_utterances);
            call.cut_utterances.extend(cut);
        }
        Ok(())
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
                .insert(log_path.clone(), OpenCall::new(log, None, Vec::new(), now));
            log_path
        };
        let call = match self.open_calls.entry(log_path.clone()) {
            Entry::Occupied(open) => open.into_mut(),
            // One that already ended: a resend after a quit, late events after an idle end, or
            // a rejoin.
            Entry::Vacant(ended) => {
                let reopened = CallLog::reopen(&log_path)?;
                if reopened.log.has_seq(seq) {
                    return Ok(None);
                }
                tracing::info!(file.path = %log_path.display(), "browser call reopened");
                let mut call = OpenCall::new(
                    reopened.log,
                    reopened.summary.start,
                    reopened.caption_line_keys,
                    now,
                );
                call.is_recording_stopped = reopened.summary.is_recording_stopped;
                ended.insert(call)
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
    fn new(
        log: CallLog,
        start: Option<CallStart>,
        logged_caption_keys: Vec<String>,
        now: Now,
    ) -> Self {
        Self {
            log,
            sessions: HashSet::new(),
            start,
            holder_ports: HashSet::new(),
            last_event_at: now.instant,
            last_at: None,
            is_reported: false,
            logged_caption_keys,
            is_recording_stopped: false,
            live_utterances: HashSet::new(),
            cut_utterances: HashSet::new(),
        }
    }

    /// Whether the event is a caption or chat message the log must not take. Notes each
    /// caption line it lets through.
    fn is_withheld(&mut self, event: &CallEvent) -> bool {
        match event {
            CallEvent::Caption {
                device_id,
                utterance_id,
                ..
            } => {
                let key = (device_id.clone(), utterance_id.clone());
                if self.cut_utterances.contains(&key) {
                    return true;
                }
                self.live_utterances.insert(key);
                self.is_recording_stopped
            }
            CallEvent::Chat { .. } => self.is_recording_stopped,
            CallEvent::CallStarted { .. }
            | CallEvent::Roster { .. }
            | CallEvent::Health { .. }
            | CallEvent::RecordingStopped { .. }
            | CallEvent::RecordingResumed { .. }
            | CallEvent::CallEnded { .. }
            | CallEvent::Unknown => false,
        }
    }

    /// The call as the call stream first hears of it. Marks it reported.
    fn take_opened(&mut self, log_path: &Path) -> OpenedCall {
        self.is_reported = true;
        OpenedCall {
            log_path: log_path.to_path_buf(),
            title: self.title(),
            logged_caption_keys: std::mem::take(&mut self.logged_caption_keys),
            is_recording_stopped: self.is_recording_stopped,
        }
    }

    fn title(&self) -> Option<String> {
        let start = self.start.as_ref()?;
        start.title.clone().or_else(|| start.meeting_code.clone())
    }

    // Keeps the call's title current.
    fn note_event(&mut self, event: &CallEvent) {
        if self.start.is_none() {
            self.start = CallStart::from_event(event);
        }
    }
}

// What the status board learns from one event.
fn observation_of(event: &CallEvent) -> Option<Observation> {
    match event {
        CallEvent::Caption {
            device_id,
            utterance_id,
            ..
        } => Some(Observation::MeetCaption {
            line_key: caption_line_key(device_id, utterance_id),
        }),
        CallEvent::Health {
            source,
            captions_on,
            captions_unavailable,
            caption_frames_failing,
            parse_failures,
            detail,
            ..
        } => {
            if *parse_failures > 0 || *captions_unavailable {
                tracing::warn!(
                    browser_call.parse_failures = parse_failures,
                    browser_call.captions_unavailable = captions_unavailable,
                    exception.message = detail.as_deref().unwrap_or_default(),
                    "mc-meet health report"
                );
            }
            let captions = match (*captions_unavailable, *captions_on) {
                (true, _) => MeetCaptions::Unavailable,
                (false, true) => MeetCaptions::On,
                (false, false) => MeetCaptions::Off,
            };
            Some(Observation::MeetHealth {
                source: *source,
                captions,
                is_caption_frames_failing: *caption_frames_failing,
            })
        }
        CallEvent::CallStarted { .. }
        | CallEvent::Chat { .. }
        | CallEvent::Roster { .. }
        | CallEvent::RecordingStopped { .. }
        | CallEvent::RecordingResumed { .. }
        | CallEvent::CallEnded { .. }
        | CallEvent::Unknown => None,
    }
}

/// Names one caption line: every version of an utterance has the same key.
pub(super) fn caption_line_key(device_id: &str, utterance_id: &str) -> String {
    format!("{device_id}|{utterance_id}")
}

// The session id names the log file, so it must not hold a path separator.
fn is_valid_session_id(session_id: &str) -> bool {
    (1..=MAX_SESSION_ID_CHARS).contains(&session_id.len())
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}
