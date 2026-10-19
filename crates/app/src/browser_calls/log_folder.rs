//! The folder of browser call logs: how a log is named and found, the rejoin index, and logs set
//! aside as unreadable. Only this module knows the folder's layout.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use sources::browser_call::{self, EndReason, LogSummary};

use super::call_log::CallLog;

const LOG_EXTENSION: &str = "jsonl";
/// A rejoin file names the log its session joined.
const REJOIN_EXTENSION: &str = "rejoin";
/// A log's name starts with its start time in this form, which sorts as the time does. No
/// colons: Windows rejects them in file names.
const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S%.3fZ";
/// How far back to look for the log a new session rejoins. No call runs longer.
const REJOIN_LOOKBACK: SignedDuration = SignedDuration::from_hours(24);

#[derive(Debug, Clone)]
pub(super) struct LogFolder {
    dir: PathBuf,
}

impl LogFolder {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Creates the log of the session `session_id`, which started at `started_at`.
    pub(super) fn create(
        &self,
        started_at: Timestamp,
        session_id: &str,
    ) -> std::io::Result<CallLog> {
        std::fs::create_dir_all(&self.dir)?;
        let log = CallLog::create(&self.dir.join(log_file_name(started_at, session_id)))?;
        // The new folder entry must reach the disk, or a crash can lose the whole log.
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(log)
    }

    /// The log of `session_id`, if there is one: the log it rejoined, else its own.
    pub(super) fn find(&self, session_id: &str) -> std::io::Result<Option<PathBuf>> {
        if let Some(log_path) = self.rejoined_log(session_id)? {
            return Ok(Some(log_path));
        }
        let suffix = format!("-{session_id}.{LOG_EXTENSION}");
        Ok(self.log_paths()?.into_iter().find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(&suffix))
        }))
    }

    // A rejoined log that was set aside as unreadable no longer counts.
    fn rejoined_log(&self, session_id: &str) -> std::io::Result<Option<PathBuf>> {
        let file_name = match std::fs::read_to_string(self.rejoin_path(session_id)) {
            Ok(file_name) => file_name,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let file_name = Path::new(file_name.trim());
        // Only a bare file name, so a damaged rejoin file cannot point outside the folder.
        if file_name.file_name() != Some(file_name.as_os_str()) {
            return Ok(None);
        }
        let log_path = self.dir.join(file_name);
        Ok(log_path.is_file().then_some(log_path))
    }

    fn rejoin_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.{REJOIN_EXTENSION}"))
    }

    /// Records that the events of `session_id` go to `log_path`, the log of an earlier session
    /// of the same meeting.
    pub(super) fn record_rejoin(&self, session_id: &str, log_path: &Path) -> std::io::Result<()> {
        let file_name = log_path
            .file_name()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .open(self.rejoin_path(session_id))?;
        file.write_all(file_name.to_string_lossy().as_bytes())?;
        file.sync_all()?;
        // Without the folder entry on disk, a crash would leave the session's events in two logs.
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// The latest log of the meeting `meeting_code` with an event at or after `since`. Reads
    /// only logs that started within `REJOIN_LOOKBACK` before `since`.
    pub(super) fn find_rejoinable(
        &self,
        meeting_code: &str,
        since: Timestamp,
    ) -> std::io::Result<Option<PathBuf>> {
        let earliest = since
            .checked_sub(REJOIN_LOOKBACK)
            .map_err(std::io::Error::other)?;
        for path in self.logs_started_since(earliest)? {
            match read_summary(&path) {
                Ok(LogSummary {
                    start: Some(start),
                    last_at: Some(last_at),
                    ..
                }) if start.meeting_code.as_deref() == Some(meeting_code) && last_at >= since => {
                    return Ok(Some(path));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    file.path = %path.display(),
                    exception.message = %error,
                    "could not read a browser call log"
                ),
            }
        }
        Ok(None)
    }

    // Newest first. A log copied in from elsewhere keeps a name with no start time, and is
    // skipped.
    fn logs_started_since(&self, earliest: Timestamp) -> std::io::Result<Vec<PathBuf>> {
        let mut logs = Vec::new();
        for path in self.log_paths()?.into_iter().rev() {
            let Some(started_at) = name_started_at(&path) else {
                continue;
            };
            if started_at < earliest {
                break;
            }
            logs.push(path);
        }
        Ok(logs)
    }

    /// Ends each log whose last event is not a `call_ended`, at the time of its last event. The
    /// app quit or crashed during those calls. Returns every log path, oldest first.
    pub(super) fn finish_open_logs(&self) -> std::io::Result<Vec<PathBuf>> {
        let paths = self.log_paths()?;
        for path in &paths {
            if let Err(error) = finish_log(path) {
                tracing::warn!(
                    file.path = %path.display(),
                    exception.message = %error,
                    "could not finish an open browser call log"
                );
            }
        }
        Ok(paths)
    }

    // Sorted by name, which sorts the app's logs by start time.
    fn log_paths(&self) -> std::io::Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|found| found == LOG_EXTENSION))
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// Renames a log intake could not read, so nothing lists, rejoins or reopens it, and it stays
/// for a look.
pub(super) fn set_aside(log_path: &Path) {
    let unreadable = log_path.with_extension(format!("{LOG_EXTENSION}.unreadable"));
    if let Err(error) = std::fs::rename(log_path, &unreadable) {
        tracing::warn!(
            file.path = %log_path.display(),
            exception.message = %error,
            "could not set aside an unreadable log"
        );
    }
}

// No meeting code or title, since those can name a client.
fn log_file_name(started_at: Timestamp, session_id: &str) -> String {
    format!(
        "{}-{session_id}.{LOG_EXTENSION}",
        started_at.strftime(STAMP_FORMAT)
    )
}

// The start time that begins a log's name. The stamp holds no hyphen, and the session id may.
fn name_started_at(log_path: &Path) -> Option<Timestamp> {
    let file_name = log_path.file_name()?.to_str()?;
    let (stamp, _) = file_name.split_once('-')?;
    let started_at = DateTime::strptime(STAMP_FORMAT, stamp).ok()?;
    TimeZone::UTC.to_timestamp(started_at).ok()
}

fn read_summary(log_path: &Path) -> std::io::Result<LogSummary> {
    let contents = std::fs::read(log_path)?;
    let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
    Ok(browser_call::summarize(&lines))
}

fn finish_log(log_path: &Path) -> std::io::Result<()> {
    let contents = std::fs::read(log_path)?;
    let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
    let Some(last) = lines.last() else {
        return Ok(());
    };
    let summary = browser_call::summarize(&lines);
    if summary.has_ended {
        return Ok(());
    }
    let last_at = summary.last_at.unwrap_or(last.received_at);
    let (mut log, _) = CallLog::reopen(log_path)?;
    log.end(last_at, EndReason::AppRestarted, Timestamp::now())
}
