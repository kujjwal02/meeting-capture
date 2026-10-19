//! Appends a browser call's events to its session log as they arrive, so a crash loses at most
//! the line being written.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Serialize;
use sources::browser_call::{self, EndReason, LogSummary};

const SYNC_INTERVAL: Duration = Duration::from_secs(5);

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
    /// Creates a new, empty log at `path`. The caller syncs its folder.
    pub(super) fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::options().create_new(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: 0,
            synced_at: Instant::now(),
            seqs: HashSet::new(),
        })
    }

    /// Opens an existing log for more lines, with the summary of the lines it has. A torn last
    /// line is cut first.
    pub(super) fn reopen(path: &Path) -> std::io::Result<(Self, LogSummary)> {
        let contents = std::fs::read(path)?;
        let lines = browser_call::read_lines(&contents).map_err(std::io::Error::other)?;
        let file = File::options().append(true).open(path)?;
        let len = whole_lines_len(&contents)?;
        file.set_len(len)?;
        let log = Self {
            path: path.to_path_buf(),
            file,
            len,
            synced_at: Instant::now(),
            seqs: lines.iter().filter_map(|line| line.seq).collect(),
        };
        Ok((log, browser_call::summarize(&lines)))
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
