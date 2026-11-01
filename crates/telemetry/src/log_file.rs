//! The log file sink: one file per service per UTC day, `<service>-<YYYY-MM-DD>.jsonl`.
//!
//! Several processes of one service append to the same file: an audio and a video helper, or a
//! native host per Chrome profile. Each line goes out in one `write` on a file opened for
//! append, so lines from two processes never interleave.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};

use crate::line::{LineSink, RecordKind, ResourceLine, encode};
use crate::resource::{Resource, Service};

/// Daily files kept, today's included.
const RETENTION_DAYS: i32 = 14;
const EXTENSION: &str = ".jsonl";

/// Where the file layer writes.
pub(crate) enum FileSink {
    Daily(DailyLogFile),
    /// For a helper run by hand, which has no log folder.
    Stderr,
}

impl LineSink for FileSink {
    fn take_line(&self, at: Timestamp, line: Vec<u8>) {
        match self {
            Self::Daily(file) => file.append(at, &line),
            Self::Stderr => {
                // Nowhere is left to report a failed write to stderr.
                let _unreported = std::io::stderr().lock().write_all(&line);
            }
        }
    }
}

pub(crate) struct DailyLogFile {
    dir: PathBuf,
    service: Service,
    resource: Arc<Resource>,
    open_day: Mutex<Option<OpenDay>>,
}

struct OpenDay {
    date: Date,
    file: File,
}

impl DailyLogFile {
    /// Opens today's file now, so a folder the process cannot write fails at start, where the
    /// caller can still say so.
    pub(crate) fn open(
        dir: PathBuf,
        service: Service,
        resource: Arc<Resource>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let log_file = Self {
            dir,
            service,
            resource,
            open_day: Mutex::new(None),
        };
        let now = Timestamp::now();
        let today = utc_date(now);
        let opened = log_file.open_day(today, now)?;
        *log_file
            .open_day
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(opened);
        Ok(log_file)
    }

    fn append(&self, at: Timestamp, line: &[u8]) {
        let date = utc_date(at);
        let mut open_day = self.open_day.lock().unwrap_or_else(PoisonError::into_inner);
        // A line stamped just before midnight can reach the lock after the next day's file
        // opened. It goes into that file, so the files never flip back and forth.
        let is_new_day = open_day.as_ref().is_none_or(|open| date > open.date);
        if is_new_day {
            match self.open_day(date, at) {
                Ok(opened) => *open_day = Some(opened),
                Err(error) => {
                    // The log is the place errors go, so stderr is all that is left. Unlike
                    // `eprintln!`, a failed write to stderr does not panic under this lock.
                    let _unreported = writeln!(
                        std::io::stderr(),
                        "could not open the log file in {}: {error}",
                        self.dir.display()
                    );
                    *open_day = None;
                    return;
                }
            }
        }
        if let Some(open) = open_day.as_mut()
            && let Err(error) = open.file.write_all(line)
        {
            let _unreported = writeln!(std::io::stderr(), "could not write the log file: {error}");
        }
        // After the lock: the removal logs, and its lines come back here.
        drop(open_day);
        if is_new_day {
            remove_expired(&self.dir, self.service);
        }
    }

    // Opens the day's file with the resource line first.
    fn open_day(&self, date: Date, at: Timestamp) -> std::io::Result<OpenDay> {
        let path = self.dir.join(file_name(self.service, date));
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let header = ResourceLine {
            record: RecordKind::Resource,
            timestamp: at,
            resource: &self.resource,
            attributes: serde_json::Map::new(),
        };
        if let Some(encoded) = encode(&header) {
            file.write_all(&encoded)?;
        }
        Ok(OpenDay { date, file })
    }
}

/// The service's daily log files in `dir`, oldest first. A missing folder has none.
///
/// # Errors
///
/// Returns the error from reading the folder.
pub fn daily_log_files(dir: &Path, service: Service) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut dated: Vec<(Date, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let date = file_date(&entry.file_name().to_string_lossy(), service)?;
            Some((date, entry.path()))
        })
        .collect();
    dated.sort();
    Ok(dated.into_iter().map(|(_, path)| path).collect())
}

/// Deletes the service's files outside the retention: it keeps today's and the 13 days before.
pub(crate) fn remove_expired(dir: &Path, service: Service) {
    let today = utc_date(Timestamp::now());
    let Ok(cutoff) = today.checked_sub((RETENTION_DAYS - 1).days()) else {
        return;
    };
    let Ok(files) = daily_log_files(dir, service) else {
        return;
    };
    for path in files {
        let is_expired = path
            .file_name()
            .and_then(|name| file_date(&name.to_string_lossy(), service))
            .is_some_and(|date| date < cutoff);
        if !is_expired {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(file.path = %path.display(), "removed an old log file");
            }
            // Another process of the service removed it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    file.path = %path.display(),
                    exception.message = %error,
                    "could not remove an old log file"
                );
            }
        }
    }
}

fn file_name(service: Service, date: Date) -> String {
    format!("{}-{date}{EXTENSION}", service.name())
}

// The date in a file name of this service. `mc` must not claim `mc-capture-...`, so the rest of
// the name must be a date and nothing else.
fn file_date(file_name: &str, service: Service) -> Option<Date> {
    let date = file_name
        .strip_prefix(service.name())?
        .strip_prefix('-')?
        .strip_suffix(EXTENSION)?;
    date.parse().ok()
}

fn utc_date(at: Timestamp) -> Date {
    at.to_zoned(TimeZone::UTC).date()
}
