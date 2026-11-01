//! Reads every JSON line of a bundle into one list sorted by time.
//!
//! A bundle is a debug report folder, a debug folder or one `.jsonl` file. Lines with a `record`
//! field follow the telemetry line format. Lines without one come from reports saved before it:
//! `tracing` JSON lines of the app log, and the events the app kept from mc-meet in the call log,
//! the lead-up, page snapshots and failed frames.
//!
//! A recorder dump repeats lines of the log file, and the lead-up repeats events of the call
//! log, so each line is kept once. Two telemetry lines are the same line when they hold the
//! same fields and values, in any key order and spacing: the report's redaction writes a line
//! again, and a process stamps an event once for both of its sinks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use jiff::Timestamp;
use serde_json::{Map, Number, Value};

/// The kind of a telemetry line, from its `record` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    Resource,
    Dump,
    Log,
    Span,
    Metric,
    Env,
}

impl RecordKind {
    const ALL: [Self; 6] = [
        Self::Resource,
        Self::Dump,
        Self::Log,
        Self::Span,
        Self::Metric,
        Self::Env,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Resource => "resource",
            Self::Dump => "dump",
            Self::Log => "log",
            Self::Span => "span",
            Self::Metric => "metric",
            Self::Env => "env",
        }
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == text)
            .ok_or_else(|| {
                anyhow!(
                    "field `record` is {text:?}; valid options: {}",
                    option_list(Self::ALL.map(Self::name))
                )
            })
    }
}

/// A log line's severity, as `tracing` names its levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    const ALL: [Self; 6] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
        Self::Fatal,
    ];

    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }

    /// OpenTelemetry's severity number: the first of each level's range.
    pub(crate) fn number(self) -> u8 {
        match self {
            Self::Trace => 1,
            Self::Debug => 5,
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
            Self::Fatal => 21,
        }
    }

    fn parse(field: &str, text: &str) -> anyhow::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|severity| severity.text().eq_ignore_ascii_case(text))
            .ok_or_else(|| {
                anyhow!(
                    "field `{field}` is {text:?}; valid options: {}",
                    option_list(Self::ALL.map(Self::text))
                )
            })
    }
}

/// The kind a `record` field names. A value that is not a string is named by its JSON type.
fn record_kind(record: &Value) -> anyhow::Result<RecordKind> {
    match record {
        Value::String(text) => RecordKind::parse(text),
        Value::Null => bail!("field `record` is null, not a string"),
        Value::Bool(_) => bail!("field `record` is a boolean, not a string"),
        Value::Number(_) => bail!("field `record` is a number, not a string"),
        Value::Array(_) => bail!("field `record` is an array, not a string"),
        Value::Object(_) => bail!("field `record` is an object, not a string"),
    }
}

fn option_list<const N: usize>(names: [&str; N]) -> String {
    names.join(", ")
}

pub(crate) struct Bundle {
    /// Every line, oldest first.
    pub lines: Vec<BundleLine>,
    /// Each process's resource attributes, by `service.instance.id`.
    pub resources: HashMap<String, Map<String, Value>>,
}

pub(crate) struct BundleLine {
    pub at: Timestamp,
    pub service: String,
    pub instance_id: Option<String>,
    pub entry: Entry,
}

pub(crate) enum Entry {
    /// A process's resource line. Its attributes are in [`Bundle::resources`].
    Resource,
    Dump {
        reason: Option<String>,
    },
    Log(LogEntry),
    Span(SpanEntry),
    Metric(MetricRow),
    /// An environment line's fields, without `record` and `timestamp`.
    Env(Map<String, Value>),
}

pub(crate) struct LogEntry {
    pub severity: Severity,
    pub body: String,
    pub scope: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub attributes: Map<String, Value>,
}

pub(crate) struct SpanEntry {
    pub name: String,
    pub scope: Option<String>,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub started_at: Timestamp,
    pub attributes: Map<String, Value>,
}

pub(crate) struct MetricRow {
    pub interval_ms: Option<u64>,
    pub points: Vec<MetricPoint>,
}

pub(crate) struct MetricPoint {
    /// The OpenTelemetry metric name, without the attribute suffix of the row's key.
    pub name: String,
    pub value: Number,
    pub unit: String,
    pub attributes: Map<String, Value>,
    /// Set on a cumulative counter: when it started counting.
    pub start: Option<Timestamp>,
}

impl MetricPoint {
    pub(crate) fn value_f64(&self) -> f64 {
        self.value.as_f64().unwrap_or_default()
    }

    /// The attribute's text: a string as is, any other value in JSON.
    pub(crate) fn attribute(&self, key: &str) -> Option<String> {
        self.attributes.get(key).map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    }
}

impl Bundle {
    pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
        let files = jsonl_files(path)?;
        if files.is_empty() {
            bail!("no .jsonl files in {}", path.display());
        }
        let mut reader = Reader::default();
        for file in &files {
            reader.read_file(file)?;
        }
        if reader.untimed_lines > 0 || reader.unreadable_lines > 0 || reader.bad_lines > 0 {
            eprintln!(
                "skipped {} lines with no time, {} lines that are not JSON objects and {} lines \
                 that do not follow the line format",
                reader.untimed_lines, reader.unreadable_lines, reader.bad_lines
            );
        }
        let mut lines = reader.lines;
        lines.sort_by_key(|line| line.at);
        Ok(Self {
            lines,
            resources: reader.resources,
        })
    }
}

enum Parsed {
    Line(Box<BundleLine>),
    /// A line with no time to place it by.
    Untimed,
    /// A line another file of the bundle already holds.
    Repeat,
}

#[derive(Default)]
struct Reader {
    lines: Vec<BundleLine>,
    resources: HashMap<String, Map<String, Value>>,
    seen: HashSet<String>,
    untimed_lines: usize,
    unreadable_lines: usize,
    /// Lines with a `record` field that do not follow the line format.
    bad_lines: usize,
}

/// The service name of mc-meet's lines and rows.
const MC_MEET_SERVICE: &str = "mc-meet";
/// The `tracing` target the app logs mc-meet's events under.
const MC_MEET_SCOPE: &str = "mc_meet";

fn is_mc_meet_scope(scope: &str) -> bool {
    scope.split("::").next() == Some(MC_MEET_SCOPE)
}

/// The resource fields an environment line repeats.
const ENV_RESOURCE_KEYS: [&str; 5] = [
    "service.name",
    "service.instance.id",
    "service.version",
    "os.type",
    "host.arch",
];

/// Bad lines named one by one on stderr; the rest are only counted.
const BAD_LINES_SHOWN: usize = 5;

impl Reader {
    fn read_file(&mut self, path: &Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(line) else {
                self.unreadable_lines += 1;
                continue;
            };
            let parsed = match fields.get("record") {
                Some(_) if !self.seen.insert(canonical_text(&fields)) => Ok(Parsed::Repeat),
                Some(record) => record_kind(record)
                    .and_then(|kind| self.record_line(kind, fields))
                    .map(|line| {
                        if self.is_repeated_meet_event(&line) {
                            Parsed::Repeat
                        } else {
                            Parsed::Line(Box::new(line))
                        }
                    }),
                None => self.earlier_line(&file_name, fields),
            };
            match parsed {
                Ok(Parsed::Line(line)) => self.lines.push(*line),
                Ok(Parsed::Untimed) => self.untimed_lines += 1,
                Ok(Parsed::Repeat) => {}
                Err(error) => {
                    // One line a newer or broken writer wrote must not hide the rest.
                    if self.bad_lines < BAD_LINES_SHOWN {
                        eprintln!("skipped {} line {}: {error:#}", path.display(), index + 1);
                    }
                    self.bad_lines += 1;
                }
            }
        }
        Ok(())
    }

    /// Whether another file already held this mc-meet event. The app writes each event to its
    /// log and to the call's `events.jsonl`; the two lines differ in time and process fields.
    fn is_repeated_meet_event(&mut self, line: &BundleLine) -> bool {
        let Entry::Log(log) = &line.entry else {
            return false;
        };
        if line.service != MC_MEET_SERVICE {
            return false;
        }
        let Some(sent_at) = log.attributes.get("mc.meet.at").and_then(Value::as_str) else {
            return false;
        };
        let field = |key: &str| {
            log.attributes
                .get(key)
                .map(Value::to_string)
                .unwrap_or_default()
        };
        let key = format!(
            "mc-meet event|{sent_at}|{}|{}|{}",
            field("browser_call.session_id"),
            log.body,
            field("mc.meet.attributes"),
        );
        !self.seen.insert(key)
    }

    fn record_line(
        &mut self,
        kind: RecordKind,
        mut fields: Map<String, Value>,
    ) -> anyhow::Result<BundleLine> {
        let at = required_time(&fields, "timestamp")?;
        let (service, instance_id, entry) = match kind {
            RecordKind::Resource | RecordKind::Dump => {
                let Some(Value::Object(resource)) = fields.remove("resource") else {
                    bail!("field `resource` is missing or not an object");
                };
                let service = required_text(&resource, "service.name")?;
                let instance_id = optional_text(&resource, "service.instance.id");
                // The process's own line wins over what an environment line said of it.
                if let Some(instance_id) = &instance_id {
                    self.resources.insert(instance_id.clone(), resource);
                }
                let entry = match kind {
                    RecordKind::Dump => Entry::Dump {
                        reason: fields.get("attributes").and_then(|attributes| {
                            optional_text_in(attributes, "mc.recorder.reason")
                        }),
                    },
                    RecordKind::Resource
                    | RecordKind::Log
                    | RecordKind::Span
                    | RecordKind::Metric
                    | RecordKind::Env => Entry::Resource,
                };
                (service, instance_id, entry)
            }
            RecordKind::Log => log_entry(fields)?,
            RecordKind::Span => {
                let Some(Value::Object(span)) = fields.get("span") else {
                    bail!("field `span` is missing or not an object");
                };
                let entry = Entry::Span(SpanEntry {
                    name: required_text(span, "name")?,
                    scope: optional_text(&fields, "scope"),
                    trace_id: required_text(&fields, "trace_id")?,
                    span_id: required_text(&fields, "span_id")?,
                    parent_span_id: optional_text(span, "parent_span_id"),
                    started_at: required_time(span, "start_timestamp")?,
                    attributes: take_object(&mut fields, "attributes"),
                });
                (
                    required_text(&fields, "service.name")?,
                    optional_text(&fields, "service.instance.id"),
                    entry,
                )
            }
            RecordKind::Metric => {
                let points = take_object(&mut fields, "points")
                    .into_iter()
                    .map(|(key, point)| metric_point(&key, point))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let entry = Entry::Metric(MetricRow {
                    interval_ms: fields.get("interval_ms").and_then(Value::as_u64),
                    points,
                });
                (
                    required_text(&fields, "service.name")?,
                    optional_text(&fields, "service.instance.id"),
                    entry,
                )
            }
            RecordKind::Env => self.env_entry(fields),
        };
        Ok(BundleLine {
            at,
            service,
            instance_id,
            entry,
        })
    }

    /// An environment line's service, process and fields.
    fn env_entry(&mut self, mut fields: Map<String, Value>) -> (String, Option<String>, Entry) {
        fields.remove("record");
        fields.remove("timestamp");
        // The app writes the environment lines: one that names no process is the app's.
        let service = optional_text(&fields, "service.name").unwrap_or_else(|| "app".to_owned());
        let instance_id = optional_text(&fields, "service.instance.id");
        // The resource fields name the process: they go to its resource, which a
        // bundle without the process's resource line then still has.
        let mut resource = Map::new();
        for key in ENV_RESOURCE_KEYS {
            if let Some(value) = fields.remove(key) {
                resource.insert(key.to_owned(), value);
            }
        }
        if let Some(instance_id) = &instance_id {
            self.resources
                .entry(instance_id.clone())
                .or_insert(resource);
        }
        (service, instance_id, Entry::Env(fields))
    }

    /// A line of a report saved before the telemetry format.
    fn earlier_line(
        &mut self,
        file_name: &str,
        mut fields: Map<String, Value>,
    ) -> anyhow::Result<Parsed> {
        if fields.contains_key("level") && fields.contains_key("fields") {
            return tracing_line(file_name, fields).map(|line| Parsed::Line(Box::new(line)));
        }
        if let Some(seq) = fields.get("seq").and_then(Value::as_u64)
            && !self.seen.insert(format!("seq {seq}"))
        {
            return Ok(Parsed::Repeat);
        }
        let at = ["received_at", "timestamp", "at"]
            .into_iter()
            .find_map(|key| optional_time(&fields, key))
            .or_else(|| {
                fields
                    .get("event")
                    .and_then(|event| optional_time_in(event, "at"))
            });
        let Some(at) = at else {
            return Ok(Parsed::Untimed);
        };
        let kind = fields
            .get("event")
            .and_then(|event| optional_text_in(event, "kind"))
            .or_else(|| optional_text(&fields, "type"))
            .or_else(|| optional_text(&fields, "kind"))
            .unwrap_or_else(|| "line".to_owned());
        fields.remove("received_at");
        Ok(Parsed::Line(Box::new(BundleLine {
            at,
            // The app keeps these events as mc-meet sent them.
            service: MC_MEET_SERVICE.to_owned(),
            instance_id: None,
            entry: Entry::Log(LogEntry {
                severity: Severity::Info,
                body: format!("{}: {kind}", earlier_file_label(file_name)),
                scope: Some(earlier_file_label(file_name).to_owned()),
                trace_id: None,
                span_id: None,
                attributes: fields,
            }),
        })))
    }
}

/// A log line's service, process and entry.
fn log_entry(mut fields: Map<String, Value>) -> anyhow::Result<(String, Option<String>, Entry)> {
    let severity = Severity::parse("severity_text", &required_text(&fields, "severity_text")?)?;
    let scope = optional_text(&fields, "scope");
    let service = required_text(&fields, "service.name")?;
    // The app logs mc-meet's events under scope `mc_meet`. They are mc-meet's, as
    // its metric rows are, which name no process.
    let (service, instance_id) = if scope.as_deref().is_some_and(is_mc_meet_scope) {
        (MC_MEET_SERVICE.to_owned(), None)
    } else {
        (service, optional_text(&fields, "service.instance.id"))
    };
    let entry = Entry::Log(LogEntry {
        severity,
        body: optional_text(&fields, "body").unwrap_or_default(),
        scope,
        trace_id: optional_text(&fields, "trace_id"),
        span_id: optional_text(&fields, "span_id"),
        attributes: take_object(&mut fields, "attributes"),
    });
    Ok((service, instance_id, entry))
}

/// A `tracing_subscriber` JSON line: `timestamp`, `level`, `fields` with `message`, `target`, and
/// the enclosing spans.
fn tracing_line(file_name: &str, mut fields: Map<String, Value>) -> anyhow::Result<BundleLine> {
    let at = required_time(&fields, "timestamp")?;
    let severity = Severity::parse("level", &required_text(&fields, "level")?)?;
    let mut attributes = Map::new();
    if let Some(Value::Array(spans)) = fields.remove("spans") {
        for span in spans {
            if let Value::Object(span_fields) = span {
                attributes.extend(span_fields.into_iter().filter(|(key, _)| key != "name"));
            }
        }
    }
    let mut event_fields = take_object(&mut fields, "fields");
    let body = match event_fields.remove("message") {
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
        None => String::new(),
    };
    attributes.extend(event_fields);
    Ok(BundleLine {
        at,
        service: earlier_log_service(file_name).to_owned(),
        instance_id: None,
        entry: Entry::Log(LogEntry {
            severity,
            body,
            scope: optional_text(&fields, "target"),
            trace_id: None,
            span_id: None,
            attributes,
        }),
    })
}

// Before the telemetry format a line named no service; the report's file name does.
fn earlier_log_service(file_name: &str) -> &'static str {
    ["native-host", "mc-capture"]
        .into_iter()
        .find(|service| file_name.contains(service))
        .unwrap_or("app")
}

fn earlier_file_label(file_name: &str) -> &'static str {
    ["lead-up", "page-snapshots", "frames"]
        .into_iter()
        .find(|label| file_name.starts_with(label))
        .unwrap_or("call-log")
}

/// One entry of a metric row's `points`. The key is the name, then `|` and the attributes when
/// one name has several series.
fn metric_point(key: &str, point: Value) -> anyhow::Result<MetricPoint> {
    let name = key.split_once('|').map_or(key, |(name, _)| name).to_owned();
    let Value::Object(mut point) = point else {
        bail!("metric point `{key}` is not an object");
    };
    let Some(Value::Number(value)) = point.remove("value") else {
        bail!("metric point `{key}`: field `value` is missing or not a number");
    };
    Ok(MetricPoint {
        name,
        value,
        unit: optional_text(&point, "unit").unwrap_or_default(),
        start: optional_time(&point, "start_timestamp"),
        attributes: take_object(&mut point, "attributes"),
    })
}

/// Every `.jsonl` file under `path`, in name order, or `path` itself when it is a file.
fn jsonl_files(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files = Vec::new();
    let mut folders = vec![path.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let entries = std::fs::read_dir(&folder)
            .with_context(|| format!("could not list {}", folder.display()))?;
        for entry in entries {
            let entry_path = entry?.path();
            if entry_path.is_dir() {
                folders.push(entry_path);
            } else if entry_path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                files.push(entry_path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// The line's fields with every object's keys sorted, so equal lines have equal text.
fn canonical_text(fields: &Map<String, Value>) -> String {
    let mut text = String::new();
    write_canonical_object(&mut text, fields);
    text
}

fn write_canonical_object(text: &mut String, fields: &Map<String, Value>) {
    let mut sorted: Vec<(&String, &Value)> = fields.iter().collect();
    sorted.sort_unstable_by_key(|(key, _)| *key);
    text.push('{');
    for (index, (key, field)) in sorted.into_iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push_str(&Value::String(key.clone()).to_string());
        text.push(':');
        write_canonical(text, field);
    }
    text.push('}');
}

fn write_canonical(text: &mut String, value: &Value) {
    match value {
        Value::Object(fields) => write_canonical_object(text, fields),
        Value::Array(values) => {
            text.push('[');
            for (index, element) in values.iter().enumerate() {
                if index > 0 {
                    text.push(',');
                }
                write_canonical(text, element);
            }
            text.push(']');
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            text.push_str(&value.to_string());
        }
    }
}

fn take_object(fields: &mut Map<String, Value>, key: &str) -> Map<String, Value> {
    match fields.remove(key) {
        Some(Value::Object(object)) => object,
        _ => Map::new(),
    }
}

fn optional_text(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key)?.as_str().map(str::to_owned)
}

fn optional_text_in(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn required_text(fields: &Map<String, Value>, key: &str) -> anyhow::Result<String> {
    optional_text(fields, key).ok_or_else(|| anyhow!("field `{key}` is missing or not a string"))
}

fn optional_time(fields: &Map<String, Value>, key: &str) -> Option<Timestamp> {
    fields.get(key)?.as_str()?.parse().ok()
}

fn optional_time_in(value: &Value, key: &str) -> Option<Timestamp> {
    value.get(key)?.as_str()?.parse().ok()
}

fn required_time(fields: &Map<String, Value>, key: &str) -> anyhow::Result<Timestamp> {
    let text = required_text(fields, key)?;
    text.parse()
        .with_context(|| format!("field `{key}` is {text:?}, not an RFC 3339 time"))
}
