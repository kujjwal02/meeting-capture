//! Appends a browser call's events to its session log as they arrive, so a crash loses at most
//! the line being written.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use sources::browser_call::{self, CallEvent, EndReason};

const SYNC_INTERVAL: Duration = Duration::from_secs(5);
/// A rejoin file names the log its session joined.
const REJOIN_EXTENSION: &str = "rejoin";
/// How far back to look for the log a new session rejoins. No call runs longer.
const REJOIN_LOOKBACK: SignedDuration = SignedDuration::from_hours(24);

pub(super) struct CallLog {
    path: PathBuf,
    file: File,
    /// The length of the whole lines written so far.
    len: u64,
    synced_at: Instant,
    /// Every extension `seq` in the log, so a resent event is not written twice.
    seqs: HashSet<u64>,
}

#[derive(Serialize)]
struct LoggedLine<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    received_at: Timestamp,
    event: &'a serde_json::Value,
}

impl CallLog {
    /// Creates the log for the session `session_id`, which started at `started_at`, in `log_dir`.
    pub(super) fn create(
        log_dir: &Path,
        started_at: Timestamp,
        session_id: &str,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(log_dir)?;
        let path = log_dir.join(browser_call::log_file_name(started_at, session_id));
        let file = File::options().create_new(true).append(true).open(&path)?;
        // The new folder entry must reach the disk, or a crash can lose the whole log.
        #[cfg(unix)]
        File::open(log_dir)?.sync_all()?;
        Ok(Self {
            path,
            file,
            len: 0,
            synced_at: Instant::now(),
            seqs: HashSet::new(),
        })
    }

    /// Opens an existing log for more lines. A torn last line is cut first.
    pub(super) fn reopen(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read(path)?;
        let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
        let file = File::options().append(true).open(path)?;
        let len = whole_lines_len(&contents)?;
        file.set_len(len)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len,
            synced_at: Instant::now(),
            seqs: lines.iter().filter_map(|line| line.seq).collect(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
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
    ) -> std::io::Result<()> {
        let mut text = serde_json::to_string(&LoggedLine {
            seq,
            received_at: Timestamp::now(),
            event,
        })
        .map_err(std::io::Error::other)?;
        text.push('\n');
        if let Err(error) = self.file.write_all(text.as_bytes()) {
            // A part-written line in the middle would make the whole log unreadable.
            let _ = self.file.set_len(self.len);
            return Err(error);
        }
        self.len += u64::try_from(text.len()).map_err(std::io::Error::other)?;
        if let Some(seq) = seq {
            self.seqs.insert(seq);
        }
        if is_last || self.synced_at.elapsed() >= SYNC_INTERVAL {
            self.file.sync_data()?;
            self.synced_at = Instant::now();
        }
        Ok(())
    }

    /// Appends the app's own `call_ended` line, at the time of the last event.
    pub(super) fn end(&mut self, at: Timestamp, reason: EndReason) -> std::io::Result<()> {
        let ended = serde_json::json!({ "kind": "call_ended", "at": at, "reason": reason });
        self.append(None, &ended, true)
    }
}

// Up to and with the last newline.
fn whole_lines_len(contents: &[u8]) -> std::io::Result<u64> {
    let len = contents
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    u64::try_from(len).map_err(std::io::Error::other)
}

/// The log of `session_id` in `log_dir`, if there is one: the log it rejoined, else its own.
pub(super) fn find(log_dir: &Path, session_id: &str) -> std::io::Result<Option<PathBuf>> {
    if let Some(log_path) = rejoined_log(log_dir, session_id)? {
        return Ok(Some(log_path));
    }
    let suffix = format!("-{session_id}.jsonl");
    Ok(log_paths(log_dir)?.into_iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(&suffix))
    }))
}

// A rejoined log that was set aside as unreadable no longer counts.
fn rejoined_log(log_dir: &Path, session_id: &str) -> std::io::Result<Option<PathBuf>> {
    let file_name = match std::fs::read_to_string(rejoin_path(log_dir, session_id)) {
        Ok(file_name) => file_name,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file_name = Path::new(file_name.trim());
    // Only a bare file name, so a damaged rejoin file cannot point outside the folder.
    if file_name.file_name() != Some(file_name.as_os_str()) {
        return Ok(None);
    }
    let log_path = log_dir.join(file_name);
    Ok(log_path.is_file().then_some(log_path))
}

fn rejoin_path(log_dir: &Path, session_id: &str) -> PathBuf {
    log_dir.join(format!("{session_id}.{REJOIN_EXTENSION}"))
}

/// Records that the events of `session_id` go to `log_path`, the log of an earlier session of
/// the same meeting.
pub(super) fn record_rejoin(
    log_dir: &Path,
    session_id: &str,
    log_path: &Path,
) -> std::io::Result<()> {
    let file_name = log_path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut file = File::options()
        .create_new(true)
        .write(true)
        .open(rejoin_path(log_dir, session_id))?;
    file.write_all(file_name.to_string_lossy().as_bytes())?;
    file.sync_all()?;
    // Without the folder entry on disk, a crash would leave the session's events in two logs.
    #[cfg(unix)]
    File::open(log_dir)?.sync_all()?;
    Ok(())
}

/// The latest log of the meeting `meeting_code` with an event at or after `since`. Reads only
/// logs that started within `REJOIN_LOOKBACK` before `since`.
pub(super) fn find_rejoinable(
    log_dir: &Path,
    meeting_code: &str,
    since: Timestamp,
) -> std::io::Result<Option<PathBuf>> {
    let earliest = since
        .checked_sub(REJOIN_LOOKBACK)
        .map_err(std::io::Error::other)?;
    // A log's file name starts with its start time, in a form that sorts as the time does.
    let earliest_name = browser_call::log_file_name(earliest, "");
    let stamp_len = earliest_name.len() - "-.jsonl".len();
    let earliest_stamp = &earliest_name[..stamp_len];
    for path in log_paths(log_dir)?.into_iter().rev() {
        let Some(file_name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if file_name
            .get(..stamp_len)
            .is_none_or(|stamp| stamp < earliest_stamp)
        {
            break;
        }
        match read_meeting(&path) {
            Ok(Some((code, last_at))) if code == meeting_code && last_at >= since => {
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

// The meeting code of a log and the time of its last event.
fn read_meeting(log_path: &Path) -> std::io::Result<Option<(String, Timestamp)>> {
    let contents = std::fs::read(log_path)?;
    let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
    let meeting_code = lines.iter().find_map(|line| match &line.event {
        CallEvent::CallStarted { meeting_code, .. } => Some(meeting_code.trim().to_owned()),
        CallEvent::Caption { .. }
        | CallEvent::Chat { .. }
        | CallEvent::Roster { .. }
        | CallEvent::Health { .. }
        | CallEvent::CallEnded { .. }
        | CallEvent::Unknown => None,
    });
    let last_at = lines.iter().filter_map(|line| line.event.at()).max();
    Ok(meeting_code.zip(last_at))
}

/// Ends each log in `log_dir` whose last event is not a `call_ended`, at the time of its last
/// event. The app quit or crashed during those calls. Returns every log path, oldest first.
pub(super) fn finish_open_logs(log_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let paths = log_paths(log_dir)?;
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

fn finish_log(path: &Path) -> std::io::Result<()> {
    let contents = std::fs::read(path)?;
    let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
    let Some(last) = lines.last() else {
        return Ok(());
    };
    if is_end(&last.event) {
        return Ok(());
    }
    let last_at = lines
        .iter()
        .filter_map(|line| line.event.at())
        .max()
        .unwrap_or(last.received_at);
    CallLog::reopen(path)?.end(last_at, EndReason::AppRestarted)
}

/// Whether `event` ends its call.
pub(super) fn is_end(event: &CallEvent) -> bool {
    match event {
        CallEvent::CallEnded { .. } => true,
        CallEvent::CallStarted { .. }
        | CallEvent::Caption { .. }
        | CallEvent::Chat { .. }
        | CallEvent::Roster { .. }
        | CallEvent::Health { .. }
        | CallEvent::Unknown => false,
    }
}

fn log_paths(log_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|found| found == "jsonl"))
        .collect();
    paths.sort();
    Ok(paths)
}
