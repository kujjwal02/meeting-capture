//! Metric rows: gauges and counters with OpenTelemetry names and UCUM units, one row per interval
//! while a scope is open.
//!
//! A [`MetricsScope`] owns a sampler thread. Each interval it writes one `metric` row with every
//! gauge set since the last row and every counter. A gauge nobody set in the interval is left out,
//! so a stalled source shows as a hole, not as its last value repeated. Counters are cumulative
//! from their creation, which `start_timestamp` gives.
//!
//! A row's `points` key is the metric name, or `name|k=v,k=v` with the attributes sorted when
//! the point has any. Each row is one `write` on a file opened for append, so several scopes on
//! one file never interleave their rows.
//!
//! A source that brings its own timing writes [`RowPoints`] through [`MetricsScope::write_row`]
//! instead: the audio helper's heartbeat levels, and mc-meet's caption channel counts. A scope
//! from [`MetricsScope::open_rows`] has no sampler and takes only such rows.
//!
//! A scope on a debug folder stops writing once the folder holds 50 MB, and logs that once per
//! folder: every scope on one folder shares its budget.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::line::encode;
use crate::trace::TraceId;

/// The file name in a debug folder.
pub const METRICS_FILE_NAME: &str = "metrics.jsonl";
/// What one debug folder may hold before its metrics stop.
pub const DEBUG_FOLDER_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Other writers grow the folder too, so its size is read again this often.
const FOLDER_MEASURE_INTERVAL: Duration = Duration::from_secs(30);
/// Starts the name of the daily files in the log folder, `metrics-<UTC date>.jsonl`.
pub const DAILY_FILE_PREFIX: &str = "metrics";
/// mc-meet, the one source that brings its own rows. It has no `Service`: it is not a Rust
/// process.
const MC_MEET_SERVICE_NAME: &str = "mc-meet";

/// The budget of each debug folder some scope writes, so two scopes on one folder count each
/// other's rows and warn once.
static FOLDER_BUDGETS: Mutex<Vec<(PathBuf, Weak<Mutex<FolderBudget>>)>> = Mutex::new(Vec::new());

/// One open scope: a call's debug folder, or the day's file outside calls. Dropping it writes a
/// last row and stops the sampler.
pub struct MetricsScope {
    shared: Arc<ScopeShared>,
    stop: Option<mpsc::Sender<()>>,
    sampler: Option<JoinHandle<()>>,
}

/// A value read at each row. Clones share the value.
#[derive(Clone)]
pub struct Gauge(Arc<GaugeCell>);

/// A cumulative count. Clones share the count.
#[derive(Clone)]
pub struct Counter(Arc<CounterCell>);

struct GaugeCell {
    value_bits: AtomicU64,
    is_fresh: AtomicBool,
}

struct CounterCell {
    total: AtomicU64,
}

struct ScopeShared {
    target: RowTarget,
    interval_ms: u32,
    trace_id: Mutex<Option<TraceId>>,
    instruments: Mutex<Vec<Instrument>>,
    /// `None` for the day's file, which has no cap.
    budget: Option<Arc<Mutex<FolderBudget>>>,
    has_write_failed: AtomicBool,
}

enum RowTarget {
    /// `metrics.jsonl` in a debug folder, under the folder's size cap.
    DebugFolder(PathBuf),
    /// `metrics-<UTC date>.jsonl` in the log folder, kept 14 days.
    Daily(PathBuf),
}

#[derive(Default)]
struct FolderBudget {
    measured_bytes: u64,
    measured_at: Option<Instant>,
    written_bytes: u64,
    is_full: bool,
}

struct Instrument {
    key: String,
    unit: &'static str,
    attributes: Map<String, Value>,
    kind: InstrumentKind,
}

enum InstrumentKind {
    Gauge(Arc<GaugeCell>),
    Counter {
        cell: Arc<CounterCell>,
        started_at: Timestamp,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RowRecord {
    Metric,
}

/// The points of one row a source builds itself, keyed and shaped as the sampler's points.
#[derive(Default)]
pub struct RowPoints(Map<String, Value>);

impl RowPoints {
    /// A gauge point. A value that is not finite is left out: JSON has no infinity.
    pub fn gauge(&mut self, name: &str, unit: &str, attributes: &[(&str, Value)], value: f64) {
        let Some(number) = serde_json::Number::from_f64(value) else {
            return;
        };
        self.insert(name, unit, attributes, Value::Number(number), None);
    }

    /// A cumulative counter point, counted from `started_at`.
    pub fn counter(
        &mut self,
        name: &str,
        unit: &str,
        attributes: &[(&str, Value)],
        total: u64,
        started_at: Timestamp,
    ) {
        self.insert(name, unit, attributes, Value::from(total), Some(started_at));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn insert(
        &mut self,
        name: &str,
        unit: &str,
        attributes: &[(&str, Value)],
        value: Value,
        started_at: Option<Timestamp>,
    ) {
        let mut sorted: Vec<&(&str, Value)> = attributes.iter().collect();
        sorted.sort_unstable_by_key(|(attribute, _)| *attribute);
        let key = point_key(
            name,
            sorted
                .iter()
                .map(|(attribute, value)| (*attribute, attribute_text(value))),
        );
        let mut point = Map::new();
        point.insert("value".to_owned(), value);
        if let Some(started_at) = started_at {
            point.insert(
                "start_timestamp".to_owned(),
                Value::String(started_at.to_string()),
            );
        }
        point.insert("unit".to_owned(), Value::from(unit));
        if !sorted.is_empty() {
            let attributes = sorted
                .iter()
                .map(|(attribute, value)| ((*attribute).to_owned(), value.clone()))
                .collect();
            point.insert("attributes".to_owned(), Value::Object(attributes));
        }
        self.0.insert(key, Value::Object(point));
    }
}

/// An attribute value as a point key spells it: text as it is, a number or a flag as JSON.
fn attribute_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            value.to_string()
        }
    }
}

/// `name`, or `name|k=v,k=v` with the attributes in the order given.
fn point_key<'a>(name: &str, sorted: impl Iterator<Item = (&'a str, String)>) -> String {
    let pairs: Vec<String> = sorted
        .map(|(attribute, value)| format!("{attribute}={value}"))
        .collect();
    if pairs.is_empty() {
        name.to_owned()
    } else {
        format!("{name}|{}", pairs.join(","))
    }
}

#[derive(Serialize)]
struct MetricRow<'a, P> {
    record: RowRecord,
    timestamp: Timestamp,
    #[serde(rename = "service.name", skip_serializing_if = "Option::is_none")]
    service_name: Option<&'a str>,
    #[serde(
        rename = "service.instance.id",
        skip_serializing_if = "Option::is_none"
    )]
    instance_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<TraceId>,
    interval_ms: u32,
    points: P,
}

impl MetricsScope {
    /// A scope writing `metrics.jsonl` in `folder`, one row every `interval_ms`.
    #[must_use]
    pub fn open(folder: &Path, interval_ms: u32) -> Self {
        Self::start(RowTarget::DebugFolder(folder.to_path_buf()), interval_ms)
    }

    /// A scope on `metrics.jsonl` in `folder` with no sampler: it writes only the rows given to
    /// [`MetricsScope::write_row`] and [`MetricsScope::write_mc_meet_row`]. `interval_ms` is
    /// the interval those rows cover.
    #[must_use]
    pub fn open_rows(folder: &Path, interval_ms: u32) -> Self {
        Self {
            shared: Arc::new(ScopeShared::new(
                RowTarget::DebugFolder(folder.to_path_buf()),
                interval_ms,
            )),
            stop: None,
            sampler: None,
        }
    }

    /// A scope writing `metrics-<UTC date>.jsonl` in `log_dir`, for metrics outside any call.
    #[must_use]
    pub fn open_daily(log_dir: &Path, interval_ms: u32) -> Self {
        Self::start(RowTarget::Daily(log_dir.to_path_buf()), interval_ms)
    }

    /// Puts `trace_id` on every row, so a call's rows join the call's trace.
    #[must_use]
    pub fn with_trace_id(self, trace_id: TraceId) -> Self {
        *self
            .shared
            .trace_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(trace_id);
        self
    }

    fn start(target: RowTarget, interval_ms: u32) -> Self {
        let shared = Arc::new(ScopeShared::new(target, interval_ms));
        let (stop, stopped) = mpsc::channel();
        let sampler_shared = Arc::clone(&shared);
        let interval = Duration::from_millis(u64::from(interval_ms.max(1)));
        let spawned = std::thread::Builder::new()
            .name("metrics".to_owned())
            .spawn(move || run_sampler(&sampler_shared, &stopped, interval));
        let sampler = match spawned {
            Ok(sampler) => Some(sampler),
            Err(error) => {
                tracing::warn!(exception.message = %error, "could not start the metrics sampler");
                None
            }
        };
        Self {
            shared,
            stop: Some(stop),
            sampler,
        }
    }

    /// A gauge in this scope's rows. `attributes` are names and values, such as
    /// `[("mc.audio.stem", "mic")]`. Dropping every clone removes it after its last value.
    #[must_use]
    pub fn gauge(
        &self,
        name: &'static str,
        unit: &'static str,
        attributes: &[(&str, &str)],
    ) -> Gauge {
        let cell = Arc::new(GaugeCell {
            value_bits: AtomicU64::new(0),
            is_fresh: AtomicBool::new(false),
        });
        self.register(
            name,
            unit,
            attributes,
            InstrumentKind::Gauge(Arc::clone(&cell)),
        );
        Gauge(cell)
    }

    /// A counter in this scope's rows, cumulative from now. Dropping every clone removes it
    /// after its last row.
    #[must_use]
    pub fn counter(
        &self,
        name: &'static str,
        unit: &'static str,
        attributes: &[(&str, &str)],
    ) -> Counter {
        let cell = Arc::new(CounterCell {
            total: AtomicU64::new(0),
        });
        self.register(
            name,
            unit,
            attributes,
            InstrumentKind::Counter {
                cell: Arc::clone(&cell),
                started_at: Timestamp::now(),
            },
        );
        Counter(cell)
    }

    /// Writes a row of this process's points at `at`, with this scope's trace and interval. A
    /// row with no points writes nothing.
    pub fn write_row(&self, at: Timestamp, points: &RowPoints) {
        if !points.is_empty() {
            self.shared.write_own(at, &points.0);
        }
    }

    /// Writes a row of mc-meet's points, with `service.name` `mc-meet` and this scope's trace
    /// and interval. The row carries no `service.instance.id`: mc-meet has no run id the app
    /// knows.
    pub fn write_mc_meet_row(&self, at: Timestamp, points: &RowPoints) {
        if !points.is_empty() {
            self.shared
                .write(at, Some(MC_MEET_SERVICE_NAME), None, &points.0);
        }
    }

    fn register(
        &self,
        name: &'static str,
        unit: &'static str,
        attributes: &[(&str, &str)],
        kind: InstrumentKind,
    ) {
        let mut sorted: Vec<(&str, &str)> = attributes.to_vec();
        sorted.sort_unstable();
        let key = point_key(
            name,
            sorted
                .iter()
                .map(|(attribute, value)| (*attribute, (*value).to_owned())),
        );
        let attributes = sorted
            .iter()
            .map(|(attribute, value)| ((*attribute).to_owned(), Value::from(*value)))
            .collect();
        self.shared
            .instruments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Instrument {
                key,
                unit,
                attributes,
                kind,
            });
    }
}

impl Drop for MetricsScope {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(sampler) = self.sampler.take()
            && sampler.join().is_err()
        {
            tracing::warn!("the metrics sampler panicked");
        }
    }
}

impl Gauge {
    pub fn set(&self, value: f64) {
        self.0.value_bits.store(value.to_bits(), Ordering::Relaxed);
        self.0.is_fresh.store(true, Ordering::Release);
    }
}

impl Counter {
    pub fn add(&self, count: u64) {
        self.0.total.fetch_add(count, Ordering::Relaxed);
    }
}

// Rows land on a fixed grid from the scope's start, so a slow write does not shift later rows.
fn run_sampler(shared: &ScopeShared, stopped: &mpsc::Receiver<()>, interval: Duration) {
    let mut next_row_at = Instant::now() + interval;
    loop {
        let wait = next_row_at.saturating_duration_since(Instant::now());
        match stopped.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {
                shared.write_sampled();
                next_row_at += interval;
                let now = Instant::now();
                // A Mac that slept skips the rows it missed instead of writing them all at once.
                while next_row_at <= now {
                    next_row_at += interval;
                }
            }
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                shared.write_sampled();
                return;
            }
        }
    }
}

impl ScopeShared {
    fn new(target: RowTarget, interval_ms: u32) -> Self {
        let budget = match &target {
            RowTarget::DebugFolder(folder) => Some(folder_budget(folder)),
            RowTarget::Daily(_) => None,
        };
        Self {
            target,
            interval_ms,
            trace_id: Mutex::new(None),
            instruments: Mutex::new(Vec::new()),
            budget,
            has_write_failed: AtomicBool::new(false),
        }
    }

    fn write_sampled(&self) {
        let at = Timestamp::now();
        let mut points = Map::new();
        {
            let mut instruments = self
                .instruments
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            instruments.retain(|instrument| {
                if let Some(point) = instrument.point() {
                    points.insert(instrument.key.clone(), point);
                }
                !instrument.is_dropped()
            });
        }
        if points.is_empty() {
            return;
        }
        self.write_own(at, &points);
    }

    /// A row of this process's points, with its resource's service fields.
    fn write_own(&self, at: Timestamp, points: &Map<String, Value>) {
        let resource = crate::init::installed_resource();
        self.write(
            at,
            resource.as_ref().map(|resource| resource.service_name),
            resource
                .as_ref()
                .map(|resource| resource.instance_id.as_str()),
            points,
        );
    }

    fn write(
        &self,
        at: Timestamp,
        service_name: Option<&str>,
        instance_id: Option<&str>,
        points: impl Serialize,
    ) {
        let trace_id = *self.trace_id.lock().unwrap_or_else(PoisonError::into_inner);
        let row = MetricRow {
            record: RowRecord::Metric,
            timestamp: at,
            service_name,
            instance_id,
            trace_id,
            interval_ms: self.interval_ms,
            points,
        };
        let Some(line) = encode(&row) else {
            return;
        };
        let written = match &self.target {
            RowTarget::DebugFolder(folder) => {
                if !self.has_room(folder, line.len()) {
                    return;
                }
                std::fs::create_dir_all(folder).and_then(|()| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(folder.join(METRICS_FILE_NAME))?
                        .write_all(&line)
                })
            }
            RowTarget::Daily(log_dir) => {
                crate::env::append_daily(log_dir, DAILY_FILE_PREFIX, at, &line)
            }
        };
        // Once per scope: a full disk would otherwise log every second.
        if let Err(error) = written
            && !self.has_write_failed.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(exception.message = %error, "could not write a metrics row");
        }
    }

    fn has_room(&self, folder: &Path, line_bytes: usize) -> bool {
        let Some(budget) = &self.budget else {
            return true;
        };
        let mut budget = budget.lock().unwrap_or_else(PoisonError::into_inner);
        if budget.is_full {
            return false;
        }
        let now = Instant::now();
        if budget
            .measured_at
            .is_none_or(|at| now.duration_since(at) >= FOLDER_MEASURE_INTERVAL)
        {
            budget.measured_bytes = folder_size_bytes(folder);
            budget.measured_at = Some(now);
            budget.written_bytes = 0;
        }
        let line_bytes = u64::try_from(line_bytes).unwrap_or(u64::MAX);
        let total_bytes = budget
            .measured_bytes
            .saturating_add(budget.written_bytes)
            .saturating_add(line_bytes);
        if total_bytes > DEBUG_FOLDER_MAX_BYTES {
            budget.is_full = true;
            tracing::warn!(
                file.directory = %folder.display(),
                mc.debug.folder_max_bytes = DEBUG_FOLDER_MAX_BYTES,
                "the debug folder reached its size cap; its metrics stop"
            );
            return false;
        }
        budget.written_bytes += line_bytes;
        true
    }
}

impl Instrument {
    /// The point for this row, or `None` for a gauge nobody set since the last row.
    fn point(&self) -> Option<Value> {
        let mut point = Map::new();
        match &self.kind {
            InstrumentKind::Gauge(cell) => {
                if !cell.is_fresh.swap(false, Ordering::Acquire) {
                    return None;
                }
                let value = f64::from_bits(cell.value_bits.load(Ordering::Relaxed));
                // JSON has no infinity: a gauge that cannot say a finite value says nothing.
                point.insert(
                    "value".to_owned(),
                    Value::Number(serde_json::Number::from_f64(value)?),
                );
            }
            InstrumentKind::Counter { cell, started_at } => {
                point.insert(
                    "value".to_owned(),
                    Value::from(cell.total.load(Ordering::Relaxed)),
                );
                point.insert(
                    "start_timestamp".to_owned(),
                    Value::String(started_at.to_string()),
                );
            }
        }
        point.insert("unit".to_owned(), Value::from(self.unit));
        if !self.attributes.is_empty() {
            point.insert(
                "attributes".to_owned(),
                Value::Object(self.attributes.clone()),
            );
        }
        Some(Value::Object(point))
    }

    /// Whether every handle is gone, so only the scope holds the value.
    fn is_dropped(&self) -> bool {
        match &self.kind {
            InstrumentKind::Gauge(cell) => Arc::strong_count(cell) == 1,
            InstrumentKind::Counter { cell, .. } => Arc::strong_count(cell) == 1,
        }
    }
}

/// The budget the scopes on `folder` share, made by the first.
fn folder_budget(folder: &Path) -> Arc<Mutex<FolderBudget>> {
    let mut budgets = FOLDER_BUDGETS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    budgets.retain(|(_, budget)| budget.strong_count() > 0);
    if let Some(budget) = budgets
        .iter()
        .filter(|(budget_folder, _)| budget_folder == folder)
        .find_map(|(_, budget)| budget.upgrade())
    {
        return budget;
    }
    let budget = Arc::new(Mutex::new(FolderBudget::default()));
    budgets.push((folder.to_path_buf(), Arc::downgrade(&budget)));
    budget
}

/// The bytes the files under `folder` hold. A folder that cannot be read counts as empty.
#[must_use]
pub fn folder_size_bytes(folder: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => folder_size_bytes(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
            Err(_) => 0,
        })
        .fold(0, u64::saturating_add)
}
