//! The telemetry line format read back: one JSON line becomes a [`Line`].
//!
//! [`parse_line`] reads the format the `record` field marks. Formats written before it stay with
//! their readers.

use jiff::Timestamp;
use serde_json::{Map, Number, Value};

/// One telemetry line.
#[derive(Debug, Clone)]
pub struct Line {
    pub at: Timestamp,
    pub service: String,
    pub instance_id: Option<String>,
    pub entry: Entry,
}

impl Line {
    /// The trace of a log line or a metric row.
    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        match &self.entry {
            Entry::Log(log) => log.trace_id.as_deref(),
            Entry::Metric(row) => row.trace_id.as_deref(),
            Entry::Resource(_) | Entry::Dump { .. } | Entry::Span(_) | Entry::Env(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Entry {
    /// A process's resource line, with its resource attributes.
    Resource(Map<String, Value>),
    /// A flight recorder dump, with the dumping process's resource attributes.
    Dump {
        reason: Option<String>,
        resource: Map<String, Value>,
    },
    Log(LogEntry),
    Span(SpanEntry),
    Metric(MetricRow),
    /// An environment line's fields, without `record` and `timestamp`.
    Env(Map<String, Value>),
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub severity: Severity,
    pub body: String,
    pub scope: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub attributes: Map<String, Value>,
}

impl LogEntry {
    /// The `event.name` attribute, when it is a string.
    #[must_use]
    pub fn event_name(&self) -> Option<&str> {
        self.attributes.get("event.name")?.as_str()
    }
}

#[derive(Debug, Clone)]
pub struct SpanEntry {
    pub name: String,
    pub scope: Option<String>,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub started_at: Timestamp,
    pub attributes: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct MetricRow {
    /// Set on the rows of a call.
    pub trace_id: Option<String>,
    pub interval_ms: Option<u64>,
    pub points: Vec<MetricPoint>,
}

#[derive(Debug, Clone)]
pub struct MetricPoint {
    /// The OpenTelemetry metric name, without the attribute suffix of the row's key.
    pub name: String,
    pub value: Number,
    pub unit: String,
    pub attributes: Map<String, Value>,
    /// Set on a cumulative counter: when it started counting.
    pub start: Option<Timestamp>,
}

impl MetricPoint {
    #[must_use]
    pub fn value_f64(&self) -> f64 {
        self.value.as_f64().unwrap_or_default()
    }

    /// The attribute's text: a string as is, any other value in JSON.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<String> {
        self.attributes.get(key).map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    }
}

/// A log line's severity, as `tracing` names its levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
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

    #[must_use]
    pub fn text(self) -> &'static str {
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
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::Trace => 1,
            Self::Debug => 5,
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
            Self::Fatal => 21,
        }
    }

    /// Reads a severity in any letter case from `field`.
    ///
    /// # Errors
    ///
    /// [`LineError::UnknownSeverity`] when `text` names no level.
    pub fn parse(field: &'static str, text: &str) -> Result<Self, LineError> {
        Self::ALL
            .into_iter()
            .find(|severity| severity.text().eq_ignore_ascii_case(text))
            .ok_or_else(|| LineError::UnknownSeverity {
                field,
                text: text.to_owned(),
                options: Self::ALL.map(Self::text).join(", "),
            })
    }
}

/// Why a line does not follow the telemetry line format.
#[derive(Debug, thiserror::Error)]
pub enum LineError {
    #[error("the line is not JSON")]
    NotJson(#[source] serde_json::Error),
    #[error("the line is {json_type}, not a JSON object")]
    NotObject { json_type: &'static str },
    #[error("field `record` is missing; valid options: {options}")]
    MissingRecord { options: String },
    #[error("field `record` is {json_type}, not a string")]
    RecordNotText { json_type: &'static str },
    #[error("field `record` is {text:?}; valid options: {options}")]
    UnknownRecord { text: String, options: String },
    #[error("field `{field}` is {text:?}; valid options: {options}")]
    UnknownSeverity {
        field: &'static str,
        text: String,
        options: String,
    },
    #[error("field `{field}` is missing or not a string")]
    MissingText { field: &'static str },
    #[error("field `{field}` is missing or not an object")]
    MissingObject { field: &'static str },
    #[error("field `{field}` is {text:?}, not an RFC 3339 time")]
    BadTime {
        field: &'static str,
        text: String,
        #[source]
        source: jiff::Error,
    },
    #[error("metric point `{key}` is not an object")]
    PointNotObject { key: String },
    #[error("metric point `{key}`: field `value` is missing or not a number")]
    PointWithoutValue { key: String },
}

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

    fn options() -> String {
        Self::ALL.map(Self::name).join(", ")
    }

    /// The kind a `record` field names. A value that is not a string is named by its JSON type.
    fn parse(record: &Value) -> Result<Self, LineError> {
        let Value::String(text) = record else {
            return Err(LineError::RecordNotText {
                json_type: json_type(record),
            });
        };
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == text)
            .ok_or_else(|| LineError::UnknownRecord {
                text: text.clone(),
                options: Self::options(),
            })
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Parses one line in the telemetry format.
///
/// # Errors
///
/// A [`LineError`] that names the field the line gets wrong.
pub fn parse_line(text: &[u8]) -> Result<Line, LineError> {
    match serde_json::from_slice(text).map_err(LineError::NotJson)? {
        Value::Object(fields) => parse_record(fields),
        other => Err(LineError::NotObject {
            json_type: json_type(&other),
        }),
    }
}

/// Parses the fields of one line in the telemetry format, for a reader that holds them already.
///
/// # Errors
///
/// A [`LineError`] that names the field the line gets wrong.
pub fn parse_record(mut fields: Map<String, Value>) -> Result<Line, LineError> {
    let kind = match fields.get("record") {
        Some(record) => RecordKind::parse(record)?,
        None => {
            return Err(LineError::MissingRecord {
                options: RecordKind::options(),
            });
        }
    };
    let at = required_time(&fields, "timestamp")?;
    let (service, instance_id, entry) = match kind {
        RecordKind::Resource | RecordKind::Dump => {
            let Some(Value::Object(resource)) = fields.remove("resource") else {
                return Err(LineError::MissingObject { field: "resource" });
            };
            let service = required_text(&resource, "service.name")?;
            let instance_id = optional_text(&resource, "service.instance.id");
            let entry = match kind {
                RecordKind::Dump => Entry::Dump {
                    reason: fields
                        .get("attributes")
                        .and_then(|attributes| attributes.get("mc.recorder.reason")?.as_str())
                        .map(str::to_owned),
                    resource,
                },
                RecordKind::Resource
                | RecordKind::Log
                | RecordKind::Span
                | RecordKind::Metric
                | RecordKind::Env => Entry::Resource(resource),
            };
            (service, instance_id, entry)
        }
        RecordKind::Log => {
            let entry = Entry::Log(LogEntry {
                severity: Severity::parse(
                    "severity_text",
                    &required_text(&fields, "severity_text")?,
                )?,
                body: optional_text(&fields, "body").unwrap_or_default(),
                scope: optional_text(&fields, "scope"),
                trace_id: optional_text(&fields, "trace_id"),
                span_id: optional_text(&fields, "span_id"),
                attributes: take_object(&mut fields, "attributes"),
            });
            (
                required_text(&fields, "service.name")?,
                optional_text(&fields, "service.instance.id"),
                entry,
            )
        }
        RecordKind::Span => {
            let Some(Value::Object(span)) = fields.get("span") else {
                return Err(LineError::MissingObject { field: "span" });
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
                .map(|(key, point)| metric_point(key, point))
                .collect::<Result<Vec<_>, _>>()?;
            let entry = Entry::Metric(MetricRow {
                trace_id: optional_text(&fields, "trace_id"),
                interval_ms: fields.get("interval_ms").and_then(Value::as_u64),
                points,
            });
            (
                required_text(&fields, "service.name")?,
                optional_text(&fields, "service.instance.id"),
                entry,
            )
        }
        RecordKind::Env => {
            fields.remove("record");
            fields.remove("timestamp");
            // The app writes the environment lines: one that names no process is the app's.
            let service =
                optional_text(&fields, "service.name").unwrap_or_else(|| "app".to_owned());
            let instance_id = optional_text(&fields, "service.instance.id");
            (service, instance_id, Entry::Env(fields))
        }
    };
    Ok(Line {
        at,
        service,
        instance_id,
        entry,
    })
}

/// One entry of a metric row's `points`. The key is the name, then `|` and the attributes when
/// one name has several series.
fn metric_point(key: String, point: Value) -> Result<MetricPoint, LineError> {
    let Value::Object(mut point) = point else {
        return Err(LineError::PointNotObject { key });
    };
    let Some(Value::Number(value)) = point.remove("value") else {
        return Err(LineError::PointWithoutValue { key });
    };
    let name = key.split_once('|').map_or(key.as_str(), |(name, _)| name);
    Ok(MetricPoint {
        name: name.to_owned(),
        value,
        unit: optional_text(&point, "unit").unwrap_or_default(),
        start: point
            .get("start_timestamp")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok()),
        attributes: take_object(&mut point, "attributes"),
    })
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

fn required_text(fields: &Map<String, Value>, field: &'static str) -> Result<String, LineError> {
    optional_text(fields, field).ok_or(LineError::MissingText { field })
}

fn required_time(fields: &Map<String, Value>, field: &'static str) -> Result<Timestamp, LineError> {
    let text = required_text(fields, field)?;
    text.parse().map_err(|source| LineError::BadTime {
        field,
        text,
        source,
    })
}
