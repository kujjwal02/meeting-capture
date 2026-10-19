//! A log of one JSON object a line, appended as things happen, so a crash loses at most the line
//! being written. Browser call logs and huddle logs are both this shape.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

const LOG_EXTENSION: &str = "jsonl";
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// A log line that is not valid JSON for its line type.
#[derive(Debug, thiserror::Error)]
#[error("line {line} is not valid: {source}")]
pub struct InvalidLine {
    pub line: usize,
    pub source: serde_json::Error,
}

/// Reads every line of a log. A torn last line, from a crash mid-write, is skipped.
///
/// # Errors
///
/// Returns `InvalidLine` naming the first bad line other than a torn last one.
pub fn read_lines<T: DeserializeOwned>(log: &[u8]) -> Result<Vec<T>, InvalidLine> {
    let text = String::from_utf8_lossy(log);
    let is_torn = !text.is_empty() && !text.ends_with('\n');
    let raw_lines: Vec<&str> = text.lines().collect();
    let mut lines = Vec::with_capacity(raw_lines.len());
    for (index, raw) in raw_lines.iter().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(raw) {
            Ok(line) => lines.push(line),
            Err(_) if is_torn && index + 1 == raw_lines.len() => {}
            Err(source) => {
                return Err(InvalidLine {
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(lines)
}

/// Every log in `log_dir`, sorted by file name. A missing folder has none.
///
/// # Errors
///
/// Returns the I/O error that stopped the folder read.
pub fn log_paths(log_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(log_dir) {
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

/// An open log that takes more lines.
#[derive(Debug)]
pub struct AppendLog {
    path: PathBuf,
    file: File,
    /// The length of the whole lines written so far.
    len: u64,
    synced_at: Instant,
}

impl AppendLog {
    /// Creates the new, empty log `file_name` in `log_dir`, and the folder if it is missing.
    ///
    /// # Errors
    ///
    /// Returns the I/O error, also when the log already exists.
    pub fn create(log_dir: &Path, file_name: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(log_dir)?;
        let path = log_dir.join(file_name);
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

    /// Opens the log at `path` for more lines. `contents` is what the caller read from it, so it
    /// can check the lines before anything changes. A torn last line is cut first, so the log
    /// stays one JSON object a line.
    ///
    /// # Errors
    ///
    /// Returns the I/O error.
    pub fn reopen(path: &Path, contents: &[u8]) -> std::io::Result<Self> {
        let file = File::options().append(true).open(path)?;
        let len = whole_lines_len(contents)?;
        file.set_len(len)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len,
            synced_at: Instant::now(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one line and flushes it to the OS, so it survives an app crash. It reaches the
    /// disk at least every 5 seconds, and at once when `is_last`.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the write or the sync. A failed write leaves no part line.
    pub fn append(&mut self, line: &impl Serialize, is_last: bool) -> std::io::Result<()> {
        self.write_line(line)?;
        self.sync_if_due(is_last)
    }

    /// The write half of `append`, for a caller that must know the line is in the log even when
    /// the sync after it fails.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the write. A failed write leaves no part line.
    pub fn write_line(&mut self, line: &impl Serialize) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
        text.push('\n');
        if let Err(error) = self.file.write_all(text.as_bytes()) {
            // A part-written line in the middle would make the whole log unreadable.
            let _ = self.file.set_len(self.len);
            return Err(error);
        }
        self.len += u64::try_from(text.len()).map_err(std::io::Error::other)?;
        Ok(())
    }

    /// The sync half of `append`.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the sync.
    pub fn sync_if_due(&mut self, is_last: bool) -> std::io::Result<()> {
        if is_last || self.synced_at.elapsed() >= SYNC_INTERVAL {
            self.file.sync_data()?;
            self.synced_at = Instant::now();
        }
        Ok(())
    }
}

/// Appends the line `end_line` makes from a log's lines, when it makes one. For a log the app
/// quit or crashed during. A torn last line is cut first.
///
/// # Errors
///
/// Returns the error that stopped the read, the parse or the write.
pub fn finish<T: DeserializeOwned, L: Serialize>(
    log_path: &Path,
    end_line: impl FnOnce(&[T]) -> Option<L>,
) -> std::io::Result<()> {
    let contents = std::fs::read(log_path)?;
    let lines = read_lines(&contents).map_err(std::io::Error::other)?;
    let Some(end) = end_line(&lines) else {
        return Ok(());
    };
    AppendLog::reopen(log_path, &contents)?.append(&end, true)
}

// Up to and with the last newline.
fn whole_lines_len(contents: &[u8]) -> std::io::Result<u64> {
    let len = contents
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    u64::try_from(len).map_err(std::io::Error::other)
}
