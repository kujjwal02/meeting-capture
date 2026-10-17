//! Appends huddle log lines to disk as they happen, so a crash loses at most the last line.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use sources::slack_huddle::{self, LogLine};

const SYNC_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) struct HuddleLog {
    path: PathBuf,
    file: File,
    /// The length of the whole lines written so far.
    len: u64,
    synced_at: Instant,
}

impl HuddleLog {
    /// Creates the log for a huddle that started at `started_at`, in `log_dir`.
    pub(crate) fn create(log_dir: &Path, started_at: Timestamp) -> std::io::Result<Self> {
        std::fs::create_dir_all(log_dir)?;
        let path = log_dir.join(slack_huddle::log_file_name(started_at));
        let file = File::options().create_new(true).append(true).open(&path)?;
        // The new folder entry must reach the disk, or a crash can lose the whole log.
        #[cfg(unix)]
        File::open(log_dir)?.sync_all()?;
        Ok(Self {
            path,
            file,
            len: 0,
            synced_at: Instant::now(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn append(&mut self, line: &LogLine) -> std::io::Result<()> {
        let text = line_text(line)?;
        if let Err(error) = self.file.write_all(text.as_bytes()) {
            // A part-written line in the middle would make the whole log unreadable.
            let _ = self.file.set_len(self.len);
            return Err(error);
        }
        self.len += u64::try_from(text.len()).map_err(std::io::Error::other)?;
        if self.synced_at.elapsed() >= SYNC_INTERVAL || matches!(line, LogLine::Ended { .. }) {
            self.file.sync_data()?;
            self.synced_at = Instant::now();
        }
        Ok(())
    }
}

/// Adds an `ended` line to each log in `log_dir` that has none, at the time of its last line.
/// The app quit or crashed during those huddles. Returns every log path, oldest first.
///
/// # Errors
///
/// Returns the I/O error that stopped the folder read. A log that cannot be read or finished is
/// logged and skipped.
pub fn finish_open_logs(log_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let paths = crate::files_with_extension(log_dir, "jsonl")?;
    for path in &paths {
        if let Err(error) = finish_log(path) {
            tracing::warn!(
                file.path = %path.display(),
                exception.message = %error,
                "could not finish an open huddle log"
            );
        }
    }
    Ok(paths)
}

fn finish_log(path: &Path) -> std::io::Result<()> {
    let contents = std::fs::read(path)?;
    let lines = slack_huddle::read_lines(&contents).map_err(std::io::Error::other)?;
    let Some(last) = lines.last() else {
        return Ok(());
    };
    if matches!(last, LogLine::Ended { .. }) {
        return Ok(());
    }
    let ended = LogLine::Ended {
        seen_at: lines
            .iter()
            .map(LogLine::seen_at)
            .max()
            .unwrap_or(last.seen_at()),
    };
    let mut file = File::options().append(true).open(path)?;
    // A crash mid-write leaves a torn last line; cut it, so the log stays one JSON object a line.
    if let Some(torn_at) = contents
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map(|index| index + 1)
        && torn_at < contents.len()
    {
        file.set_len(u64::try_from(torn_at).map_err(std::io::Error::other)?)?;
    }
    file.write_all(line_text(&ended)?.as_bytes())?;
    file.sync_data()
}

fn line_text(line: &LogLine) -> std::io::Result<String> {
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    Ok(text)
}
