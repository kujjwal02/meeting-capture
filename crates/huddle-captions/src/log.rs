//! Appends huddle log lines to disk as they happen, so a crash loses at most the last line.

use std::path::Path;

use jiff::Timestamp;
use sources::append_log::{self, AppendLog, FinishedLogs};
use sources::slack_huddle::{self, LogLine};

pub(crate) struct HuddleLog {
    log: AppendLog,
    is_recording_stopped: bool,
}

impl HuddleLog {
    /// Creates the log for a huddle that started at `started_at`, in `log_dir`.
    pub(crate) fn create(log_dir: &Path, started_at: Timestamp) -> std::io::Result<Self> {
        let log = AppendLog::create(log_dir, &slack_huddle::log_file_name(started_at))?;
        Ok(Self {
            log,
            is_recording_stopped: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.log.path()
    }

    /// Appends `line`, except a caption line while the user has recording stopped.
    pub(crate) fn append(&mut self, line: &LogLine) -> std::io::Result<()> {
        let is_caption = matches!(line, LogLine::Row { .. } | LogLine::Overlay { .. });
        if is_caption && self.is_recording_stopped {
            return Ok(());
        }
        self.log.append(line, matches!(line, LogLine::Ended { .. }))
    }

    /// Logs a stop or a resume when the user's choice changes. Returns whether it changed.
    pub(crate) fn set_recording_stopped(
        &mut self,
        is_stopped: bool,
        seen_at: Timestamp,
    ) -> std::io::Result<bool> {
        if self.is_recording_stopped == is_stopped {
            return Ok(false);
        }
        let line = if is_stopped {
            LogLine::RecordingStopped { seen_at }
        } else {
            LogLine::RecordingResumed { seen_at }
        };
        self.log.append(&line, false)?;
        self.is_recording_stopped = is_stopped;
        Ok(true)
    }
}

/// Adds an `ended` line to each log in `log_dir` that has none, at the time of its last line.
/// The app quit or crashed during those huddles. Returns every log path, and the ones it ended.
///
/// # Errors
///
/// Returns the I/O error that stopped the folder read. A log that cannot be read or finished is
/// logged and skipped.
pub fn finish_open_logs(log_dir: &Path) -> std::io::Result<FinishedLogs> {
    let log_paths = append_log::log_paths(log_dir)?;
    let mut ended_paths = Vec::new();
    for path in &log_paths {
        match append_log::finish(path, ended_line) {
            Ok(true) => ended_paths.push(path.clone()),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                file.path = %path.display(),
                exception.message = %error,
                "could not finish an open huddle log"
            ),
        }
    }
    Ok(FinishedLogs {
        log_paths,
        ended_paths,
    })
}

fn ended_line(lines: &[LogLine]) -> Option<LogLine> {
    let last = lines.last()?;
    if matches!(last, LogLine::Ended { .. }) {
        return None;
    }
    Some(LogLine::Ended {
        seen_at: lines
            .iter()
            .map(LogLine::seen_at)
            .max()
            .unwrap_or(last.seen_at()),
    })
}
