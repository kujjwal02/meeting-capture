//! `mc debug export-otlp`: a bundle as OTLP/JSON, one export request per line, the shape an
//! OpenTelemetry Collector's `otlpjsonfile` receiver reads.
//!
//! Each process's lines become one resource. Per resource the file gets up to three lines: an
//! `ExportLogsServiceRequest`, an `ExportTraceServiceRequest` and an
//! `ExportMetricsServiceRequest`. Ids stay in hex, as OTLP/JSON wants, and 64-bit numbers and
//! times are strings.

use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};
use std::path::Path;

use anyhow::Context as _;
use jiff::Timestamp;
use serde_json::{Map, Value, json};

use super::bundle::{Bundle, BundleLine, Entry, LogEntry, MetricPoint, Severity, SpanEntry};

const TRACE_ID_BYTES: usize = 16;
const SPAN_ID_BYTES: usize = 8;
/// `SPAN_KIND_INTERNAL`: every span is work inside one process.
const SPAN_KIND_INTERNAL: u8 = 1;
/// `AGGREGATION_TEMPORALITY_CUMULATIVE`: counters count from their start time.
const CUMULATIVE: u8 = 2;
const METRICS_SCOPE: &str = "mc.metrics";
const RECORDER_SCOPE: &str = "telemetry.recorder";
const ENVIRONMENT_SCOPE: &str = "mc.env";

pub(crate) struct Written {
    pub requests: usize,
    pub log_records: usize,
    pub spans: usize,
    pub metric_points: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MetricKind {
    Gauge,
    /// A counter: a point with a start time.
    CumulativeSum,
}

struct MetricSeries {
    unit: String,
    data_points: Vec<Value>,
}

#[derive(Default)]
struct ResourceSignals {
    logs: BTreeMap<String, Vec<Value>>,
    spans: BTreeMap<String, Vec<Value>>,
    metrics: BTreeMap<(String, MetricKind), MetricSeries>,
}

pub(crate) fn export(bundle: &Bundle, out: &Path) -> anyhow::Result<Written> {
    let mut resources: BTreeMap<(String, Option<String>), ResourceSignals> = BTreeMap::new();
    let mut dropped_ids = 0_usize;
    for line in &bundle.lines {
        let signals = resources
            .entry((line.service.clone(), line.instance_id.clone()))
            .or_default();
        match &line.entry {
            Entry::Resource => {}
            Entry::Dump { reason } => {
                let mut attributes = Map::new();
                if let Some(reason) = reason {
                    attributes.insert(
                        "mc.recorder.reason".to_owned(),
                        Value::from(reason.as_str()),
                    );
                }
                let record = log_record(
                    line.at,
                    Severity::Info,
                    "recorder dump",
                    &attributes,
                    None,
                    None,
                );
                push_log(signals, RECORDER_SCOPE, record);
            }
            Entry::Log(log) => {
                let (record, is_dropped) = log_line_record(line, log);
                dropped_ids += usize::from(is_dropped);
                push_log(signals, log.scope.as_deref().unwrap_or_default(), record);
            }
            Entry::Span(span) => match span_record(line, span) {
                Some(record) => signals
                    .spans
                    .entry(span.scope.clone().unwrap_or_default())
                    .or_default()
                    .push(record),
                None => dropped_ids += 1,
            },
            Entry::Metric(row) => {
                for point in &row.points {
                    add_data_point(signals, line.at, point);
                }
            }
            Entry::Env(fields) => {
                let mut attributes = fields.clone();
                attributes.insert("event.name".to_owned(), Value::from(ENVIRONMENT_SCOPE));
                let record = log_record(
                    line.at,
                    Severity::Info,
                    "environment",
                    &attributes,
                    None,
                    None,
                );
                push_log(signals, ENVIRONMENT_SCOPE, record);
            }
        }
    }
    if dropped_ids > 0 {
        eprintln!(
            "{dropped_ids} lines had a trace or span id that is not {TRACE_ID_BYTES} or \
             {SPAN_ID_BYTES} bytes of hex: logs went out without it, spans were left out"
        );
    }

    write_requests(bundle, resources, out)
}

/// Writes up to three export requests per resource: logs, spans, metrics.
fn write_requests(
    bundle: &Bundle,
    resources: BTreeMap<(String, Option<String>), ResourceSignals>,
    out: &Path,
) -> anyhow::Result<Written> {
    let file = std::fs::File::create(out)
        .with_context(|| format!("could not create {}", out.display()))?;
    let mut writer = BufWriter::new(file);
    let mut written = Written {
        requests: 0,
        log_records: 0,
        spans: 0,
        metric_points: 0,
    };
    for ((service, instance_id), signals) in resources {
        let resource =
            json!({ "attributes": resource_attributes(bundle, &service, instance_id.as_deref()) });
        let mut requests = Vec::new();
        if !signals.logs.is_empty() {
            let scope_logs: Vec<Value> = signals
                .logs
                .into_iter()
                .map(|(scope, records)| {
                    written.log_records += records.len();
                    json!({ "scope": { "name": scope }, "logRecords": records })
                })
                .collect();
            requests.push(
                json!({ "resourceLogs": [{ "resource": resource, "scopeLogs": scope_logs }] }),
            );
        }
        if !signals.spans.is_empty() {
            let scope_spans: Vec<Value> = signals
                .spans
                .into_iter()
                .map(|(scope, spans)| {
                    written.spans += spans.len();
                    json!({ "scope": { "name": scope }, "spans": spans })
                })
                .collect();
            requests.push(
                json!({ "resourceSpans": [{ "resource": resource, "scopeSpans": scope_spans }] }),
            );
        }
        if !signals.metrics.is_empty() {
            let metrics: Vec<Value> = signals
                .metrics
                .into_iter()
                .map(|((name, kind), series)| {
                    written.metric_points += series.data_points.len();
                    metric(&name, kind, series)
                })
                .collect();
            requests.push(json!({
                "resourceMetrics": [{
                    "resource": resource,
                    "scopeMetrics": [{ "scope": { "name": METRICS_SCOPE }, "metrics": metrics }],
                }]
            }));
        }
        for request in requests {
            serde_json::to_writer(&mut writer, &request)?;
            writer.write_all(b"\n")?;
            written.requests += 1;
        }
    }
    writer.flush()?;
    Ok(written)
}

fn push_log(signals: &mut ResourceSignals, scope: &str, record: Value) {
    signals
        .logs
        .entry(scope.to_owned())
        .or_default()
        .push(record);
}

/// The record and whether an id was dropped for not being W3C hex.
fn log_line_record(line: &BundleLine, log: &LogEntry) -> (Value, bool) {
    let trace_id = log
        .trace_id
        .as_deref()
        .filter(|id| is_hex_id(id, TRACE_ID_BYTES));
    let span_id = log
        .span_id
        .as_deref()
        .filter(|id| is_hex_id(id, SPAN_ID_BYTES));
    let is_dropped =
        trace_id.is_none() && log.trace_id.is_some() || span_id.is_none() && log.span_id.is_some();
    let record = log_record(
        line.at,
        log.severity,
        &log.body,
        &log.attributes,
        trace_id,
        span_id,
    );
    (record, is_dropped)
}

fn log_record(
    at: Timestamp,
    severity: Severity,
    body: &str,
    attributes: &Map<String, Value>,
    trace_id: Option<&str>,
    span_id: Option<&str>,
) -> Value {
    let mut record = json!({
        "timeUnixNano": unix_nanos(at),
        "observedTimeUnixNano": unix_nanos(at),
        "severityNumber": severity.number(),
        "severityText": severity.text(),
        "body": { "stringValue": body },
        "attributes": key_values(attributes),
    });
    if let Some(fields) = record.as_object_mut() {
        if let Some(trace_id) = trace_id {
            fields.insert("traceId".to_owned(), Value::from(trace_id));
        }
        if let Some(span_id) = span_id {
            fields.insert("spanId".to_owned(), Value::from(span_id));
        }
    }
    record
}

/// `None` for a span whose ids are not W3C hex: a collector rejects it.
fn span_record(line: &BundleLine, span: &SpanEntry) -> Option<Value> {
    if !is_hex_id(&span.trace_id, TRACE_ID_BYTES) || !is_hex_id(&span.span_id, SPAN_ID_BYTES) {
        return None;
    }
    let mut record = json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": span.name,
        "kind": SPAN_KIND_INTERNAL,
        "startTimeUnixNano": unix_nanos(span.started_at),
        "endTimeUnixNano": unix_nanos(line.at),
        "attributes": key_values(&span.attributes),
    });
    if let Some(parent) = span
        .parent_span_id
        .as_deref()
        .filter(|id| is_hex_id(id, SPAN_ID_BYTES))
        && let Some(fields) = record.as_object_mut()
    {
        fields.insert("parentSpanId".to_owned(), Value::from(parent));
    }
    Some(record)
}

fn add_data_point(signals: &mut ResourceSignals, at: Timestamp, point: &MetricPoint) {
    let kind = match point.start {
        Some(_) => MetricKind::CumulativeSum,
        None => MetricKind::Gauge,
    };
    let mut data_point = json!({
        "attributes": key_values(&point.attributes),
        "timeUnixNano": unix_nanos(at),
    });
    if let Some(fields) = data_point.as_object_mut() {
        if let Some(start) = point.start {
            fields.insert(
                "startTimeUnixNano".to_owned(),
                Value::from(unix_nanos(start)),
            );
        }
        match (kind, point.value.as_i64()) {
            (MetricKind::CumulativeSum, Some(count)) => {
                fields.insert("asInt".to_owned(), Value::from(count.to_string()));
            }
            (MetricKind::CumulativeSum, None) | (MetricKind::Gauge, _) => {
                fields.insert("asDouble".to_owned(), Value::from(point.value_f64()));
            }
        }
    }
    signals
        .metrics
        .entry((point.name.clone(), kind))
        .or_insert_with(|| MetricSeries {
            unit: point.unit.clone(),
            data_points: Vec::new(),
        })
        .data_points
        .push(data_point);
}

fn metric(name: &str, kind: MetricKind, series: MetricSeries) -> Value {
    match kind {
        MetricKind::Gauge => json!({
            "name": name,
            "unit": series.unit,
            "gauge": { "dataPoints": Value::Array(series.data_points) },
        }),
        MetricKind::CumulativeSum => json!({
            "name": name,
            "unit": series.unit,
            "sum": {
                "dataPoints": Value::Array(series.data_points),
                "aggregationTemporality": CUMULATIVE,
                "isMonotonic": true,
            },
        }),
    }
}

/// The process's resource line, else what the lines themselves say. A resource line counts only
/// for its own service: mc-meet's rows, which the app writes, are not the app's process.
fn resource_attributes(bundle: &Bundle, service: &str, instance_id: Option<&str>) -> Vec<Value> {
    let resource = instance_id
        .and_then(|instance_id| bundle.resources.get(instance_id))
        .filter(|resource| resource.get("service.name").and_then(Value::as_str) == Some(service));
    if let Some(resource) = resource {
        return key_values(resource);
    }
    let mut attributes = Map::new();
    attributes.insert("service.name".to_owned(), Value::from(service));
    if let Some(instance_id) = instance_id {
        attributes.insert("service.instance.id".to_owned(), Value::from(instance_id));
    }
    key_values(&attributes)
}

/// OTLP `KeyValue`s. A null says nothing, so it is left out.
fn key_values(fields: &Map<String, Value>) -> Vec<Value> {
    fields
        .iter()
        .filter_map(|(key, value)| Some(json!({ "key": key, "value": any_value(value)? })))
        .collect()
}

/// An OTLP `AnyValue`. `None` for null.
fn any_value(value: &Value) -> Option<Value> {
    Some(match value {
        Value::Null => return None,
        Value::Bool(flag) => json!({ "boolValue": flag }),
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(integer), _) => json!({ "intValue": integer.to_string() }),
            // OTLP's int is 64-bit signed; a double would round the count, so it goes as text.
            (None, Some(count)) => json!({ "stringValue": count.to_string() }),
            (None, None) => json!({ "doubleValue": number.as_f64().unwrap_or_default() }),
        },
        Value::String(text) => json!({ "stringValue": text }),
        Value::Array(values) => {
            let values: Vec<Value> = values.iter().filter_map(any_value).collect();
            json!({ "arrayValue": { "values": values } })
        }
        Value::Object(fields) => json!({ "kvlistValue": { "values": key_values(fields) } }),
    })
}

fn unix_nanos(at: Timestamp) -> String {
    at.as_nanosecond().to_string()
}

/// W3C ids: lowercase or uppercase hex of the exact length, not all zero.
fn is_hex_id(id: &str, bytes: usize) -> bool {
    id.len() == bytes * 2
        && id.chars().all(|character| character.is_ascii_hexdigit())
        && id.chars().any(|character| character != '0')
}
