//! The flight recorder: the process's last DEBUG lines in memory, written out when something
//! goes wrong. The evidence then exists even for a bug nobody expected.
//!
//! The ring keeps 10 minutes or 8 MB of lines, whichever is less. A dump appends to its file: a
//! second dump into the same file adds only the lines the first did not hold, so two problems
//! in one call keep both windows. A dump file stops growing at 32 MB, with one line that says so.

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::fields;
use crate::line::{LineSink, RecordKind, ResourceLine, encode};
use crate::resource::Resource;

const MAX_AGE: Duration = Duration::from_mins(10);
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// A call with a problem every few minutes would otherwise grow its dump file without end.
const MAX_DUMP_FILE_BYTES: u64 = 32 * 1024 * 1024;
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// Why a dump was written. It heads the dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpReason {
    /// A call problem started.
    CallProblem,
    /// The process panicked.
    Panic,
    /// A pipeline run failed.
    JobFailed,
    /// A capture helper exited with an error, hung, or would not start.
    HelperFailed,
    /// The user saved a debug report.
    DebugReport,
    /// The app asked a helper to dump, for one of the reasons above.
    AppRequest,
}

impl DumpReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallProblem => "call_problem",
            Self::Panic => "panic",
            Self::JobFailed => "job_failed",
            Self::HelperFailed => "helper_failed",
            Self::DebugReport => "debug_report",
            Self::AppRequest => "app_request",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    #[error("the flight recorder is not running: telemetry::init was not called")]
    NotStarted,
    #[error("could not write the recorder dump {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub(crate) struct FlightRecorder {
    resource: Arc<Resource>,
    ring: Mutex<Ring>,
    /// Per dump file, the last line it holds. Held through a whole dump, so two dumps into one
    /// file never race. A dump drops the entries older than the ring's oldest line.
    dumped_through: Mutex<HashMap<PathBuf, u64>>,
}

#[derive(Default)]
struct Ring {
    lines: VecDeque<RingLine>,
    total_bytes: usize,
    next_seq: u64,
}

struct RingLine {
    kept_at: Instant,
    seq: u64,
    /// Exactly the line's bytes, so the byte cap counts what the heap holds. A dump shares them.
    text: Arc<[u8]>,
}

impl Ring {
    fn drop_expired(&mut self, now: Instant) {
        while let Some(oldest) = self.lines.front() {
            let is_too_old = now.duration_since(oldest.kept_at) > MAX_AGE;
            if !is_too_old && self.total_bytes <= MAX_BYTES {
                break;
            }
            self.total_bytes -= oldest.text.len();
            self.lines.pop_front();
        }
    }
}

impl FlightRecorder {
    pub(crate) fn new(resource: Arc<Resource>) -> Self {
        Self {
            resource,
            ring: Mutex::new(Ring::default()),
            dumped_through: Mutex::new(HashMap::new()),
        }
    }

    /// Appends the lines since the last dump into `path` to it, after a line with the resource
    /// and `reason`. Returns how many lines it added.
    pub(crate) fn dump(&self, path: &Path, reason: DumpReason) -> Result<usize, DumpError> {
        let mut dumped_through = self
            .dumped_through
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let after_seq = dumped_through.get(path).copied();
        let (lines, last_seq) = {
            let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
            ring.drop_expired(Instant::now());
            let lines: Vec<Arc<[u8]>> = ring
                .lines
                .iter()
                .filter(|line| after_seq.is_none_or(|after| line.seq > after))
                .map(|line| Arc::clone(&line.text))
                .collect();
            // An entry older than every line in the ring changes nothing: the next dump into
            // its file takes the whole ring either way.
            if let Some(oldest) = ring.lines.front() {
                let oldest_seq = oldest.seq;
                dumped_through.retain(|_, through| *through >= oldest_seq);
            }
            (lines, ring.next_seq.checked_sub(1))
        };
        let write_error = |source| DumpError::Write {
            path: path.to_path_buf(),
            source,
        };
        let added = self
            .append_dump(path, reason, &lines)
            .map_err(write_error)?;
        if let Some(last_seq) = last_seq {
            dumped_through.insert(path.to_path_buf(), last_seq);
        }
        Ok(added)
    }

    // Appends the header and the lines up to the file's cap, then the full line once the cap is
    // reached. Returns how many lines it added.
    fn append_dump(
        &self,
        path: &Path,
        reason: DumpReason,
        lines: &[Arc<[u8]>],
    ) -> std::io::Result<usize> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut file_bytes = file.metadata()?.len();
        if file_bytes >= MAX_DUMP_FILE_BYTES {
            return Ok(0);
        }
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
        let header = self.dump_line(reason, false);
        writer.write_all(&header)?;
        file_bytes += header.len() as u64;
        let mut added = 0;
        for line in lines {
            if file_bytes >= MAX_DUMP_FILE_BYTES {
                writer.write_all(&self.dump_line(reason, true))?;
                break;
            }
            writer.write_all(line)?;
            file_bytes += line.len() as u64;
            added += 1;
        }
        writer.flush()?;
        Ok(added)
    }

    // The `dump` line that heads a dump, or that ends a full file when `is_file_full`.
    fn dump_line(&self, reason: DumpReason, is_file_full: bool) -> Vec<u8> {
        let mut attributes = Map::new();
        attributes.insert(
            fields::RECORDER_REASON.to_owned(),
            Value::String(reason.as_str().to_owned()),
        );
        if is_file_full {
            attributes.insert(fields::RECORDER_IS_FILE_FULL.to_owned(), Value::Bool(true));
        }
        let line = ResourceLine {
            record: RecordKind::Dump,
            timestamp: Timestamp::now(),
            resource: &self.resource,
            attributes,
        };
        encode(&line).unwrap_or_default()
    }
}

impl LineSink for FlightRecorder {
    fn take_line(&self, _at: Timestamp, line: Vec<u8>) {
        let now = Instant::now();
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = ring.next_seq;
        ring.next_seq += 1;
        let text: Arc<[u8]> = Arc::from(line);
        ring.total_bytes += text.len();
        ring.lines.push_back(RingLine {
            kept_at: now,
            seq,
            text,
        });
        ring.drop_expired(now);
    }
}
