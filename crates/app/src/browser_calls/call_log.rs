//! Appends a browser call's events to its session log as they arrive, so a crash loses at most
//! the line being written.

use std::collections::HashSet;
use std::path::Path;

use jiff::Timestamp;
use serde::Serialize;
use sources::append_log::AppendLog;
use sources::browser_call::{self, CallEvent, EndReason, LogSummary};

use super::call_book::caption_line_key;

pub(super) struct CallLog {
    log: AppendLog,
    /// Every extension `seq` in the log, so a resent event is not written twice.
    seqs: HashSet<u64>,
}

/// An existing log, opened for more lines.
pub(super) struct Reopened {
    pub(super) log: CallLog,
    pub(super) summary: LogSummary,
    /// The caption lines the log holds, by `caption_line_key`.
    pub(super) caption_line_keys: Vec<String>,
}

/// One line as written. The app's own lines have no `seq`.
#[derive(Serialize)]
pub(super) struct LoggedLine<E> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) seq: Option<u64>,
    pub(super) received_at: Timestamp,
    pub(super) event: E,
}

impl CallLog {
    /// Creates the new, empty log `file_name` in `log_dir`.
    pub(super) fn create(log_dir: &Path, file_name: &str) -> std::io::Result<Self> {
        Ok(Self {
            log: AppendLog::create(log_dir, file_name)?,
            seqs: HashSet::new(),
        })
    }

    /// Opens an existing log for more lines, with what the lines it has say. A torn last line
    /// is cut first.
    pub(super) fn reopen(path: &Path) -> std::io::Result<Reopened> {
        let contents = std::fs::read(path)?;
        let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
        let caption_line_keys: HashSet<String> = lines
            .iter()
            .filter_map(|line| match &line.event {
                CallEvent::Caption {
                    device_id,
                    utterance_id,
                    ..
                } => Some(caption_line_key(device_id, utterance_id)),
                CallEvent::CallStarted { .. }
                | CallEvent::Chat { .. }
                | CallEvent::Roster { .. }
                | CallEvent::Health { .. }
                | CallEvent::RecordingStopped { .. }
                | CallEvent::RecordingResumed { .. }
                | CallEvent::CallEnded { .. }
                | CallEvent::Unknown => None,
            })
            .collect();
        let log = Self {
            log: AppendLog::reopen(path, &contents)?,
            seqs: lines.iter().filter_map(|line| line.seq).collect(),
        };
        Ok(Reopened {
            log,
            summary: browser_call::summarize(&lines),
            caption_line_keys: caption_line_keys.into_iter().collect(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.log.path()
    }

    pub(super) fn has_seq(&self, seq: u64) -> bool {
        self.seqs.contains(&seq)
    }

    /// Appends one event and flushes it to the OS, so it survives an app crash. `seq` is `None`
    /// for a line the app writes itself.
    pub(super) fn append(
        &mut self,
        seq: Option<u64>,
        event: &serde_json::Value,
        is_last: bool,
        received_at: Timestamp,
    ) -> std::io::Result<()> {
        self.log.write_line(&LoggedLine {
            seq,
            received_at,
            event,
        })?;
        // Before the sync: a line that is in the log counts as written even when its sync fails,
        // so a resend is a duplicate.
        if let Some(seq) = seq {
            self.seqs.insert(seq);
        }
        self.log.sync_if_due(is_last)
    }

    /// Appends the app's own `call_ended` line, at the time of the last event.
    pub(super) fn end(
        &mut self,
        at: Timestamp,
        reason: EndReason,
        received_at: Timestamp,
    ) -> std::io::Result<()> {
        self.append(None, &ended_event(at, reason), true, received_at)
    }
}

/// The `recording_stopped` or `recording_resumed` event the app writes itself.
pub(super) fn recording_event(is_stopped: bool, at: Timestamp) -> serde_json::Value {
    let kind = if is_stopped {
        "recording_stopped"
    } else {
        "recording_resumed"
    };
    serde_json::json!({ "kind": kind, "at": at })
}

/// The `call_ended` event the app writes itself.
pub(super) fn ended_event(at: Timestamp, reason: EndReason) -> serde_json::Value {
    serde_json::json!({ "kind": "call_ended", "at": at, "reason": reason })
}
