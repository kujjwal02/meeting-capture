//! The JSON line shape both sinks write, and the layers that write it.
//!
//! Four kinds of line, told apart by `record`:
//!
//! - `resource`: the process's resource, first in each log file the process opens.
//! - `dump`: the resource and the reason, first in each recorder dump.
//! - `log`: one event. Its attributes are the fields of every enclosing span, outer first, then
//!   the event's own, so a line inside a call's span carries `call.id`.
//! - `span`: one closed span inside a trace, with its start, parent and own fields.
//!
//! Every `log` and `span` line also carries `service.name` and `service.instance.id`: an audio
//! and a video helper append to the same file, and the app writes mc-meet's lines into its own.

use std::cell::Cell;
use std::sync::Arc;

use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::fields;
use crate::init::FileRule;
use crate::resource::Resource;
use crate::trace::{SpanId, SpanIds, TraceId};

/// Takes finished lines, each ending in a newline.
pub(crate) trait LineSink: Send + Sync + 'static {
    fn take_line(&self, at: Timestamp, line: Vec<u8>);
}

impl<K: LineSink> LineSink for Arc<K> {
    fn take_line(&self, at: Timestamp, line: Vec<u8>) {
        K::take_line(self, at, line);
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordKind {
    Resource,
    Dump,
    Log,
    Span,
}

#[derive(Serialize)]
pub(crate) struct ResourceLine<'a> {
    pub record: RecordKind,
    pub timestamp: Timestamp,
    pub resource: &'a Resource,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub attributes: Map<String, Value>,
}

#[derive(Serialize)]
struct LogLine<'a> {
    record: RecordKind,
    timestamp: Timestamp,
    severity_text: &'static str,
    severity_number: u8,
    body: &'a str,
    #[serde(rename = "service.name")]
    service_name: &'a str,
    #[serde(rename = "service.instance.id")]
    instance_id: &'a str,
    /// The `tracing` target: the module that logged, as an instrumentation scope.
    scope: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<TraceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<SpanId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SpanName<'a>>,
    attributes: &'a Map<String, Value>,
}

#[derive(Serialize)]
struct SpanName<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct SpanLine<'a> {
    record: RecordKind,
    timestamp: Timestamp,
    #[serde(rename = "service.name")]
    service_name: &'a str,
    #[serde(rename = "service.instance.id")]
    instance_id: &'a str,
    scope: &'a str,
    trace_id: TraceId,
    span_id: SpanId,
    span: ClosedSpan<'a>,
    attributes: &'a Map<String, Value>,
}

#[derive(Serialize)]
struct ClosedSpan<'a> {
    name: &'a str,
    start_timestamp: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_span_id: Option<SpanId>,
}

/// What the app keeps per open span, in the span's extensions.
struct SpanRecord {
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    trace_id: Option<TraceId>,
    fields: Map<String, Value>,
    started_at: Timestamp,
}

/// Gives each span its W3C ids and keeps its fields. Runs before the sink layers, which read
/// what it keeps.
pub(crate) struct SpanContextLayer {
    span_ids: SpanIds,
}

impl SpanContextLayer {
    pub(crate) fn new(span_ids: SpanIds) -> Self {
        Self { span_ids }
    }
}

impl<S> tracing_subscriber::Layer<S> for SpanContextLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut span_fields = FieldCollector::default();
        attributes.record(&mut span_fields);
        let parent = span.parent().and_then(|parent| {
            parent
                .extensions()
                .get::<SpanRecord>()
                .map(|record| (record.span_id, record.trace_id))
        });
        // A parent outside any trace is no parent in OpenTelemetry: this span starts its trace.
        let traced_parent = parent.and_then(|(span_id, trace_id)| Some((span_id, trace_id?)));
        let trace_id = traced_parent
            .map(|(_, trace_id)| trace_id)
            .or_else(|| derived_trace_id(&span_fields.attributes));
        span.extensions_mut().insert(SpanRecord {
            span_id: self.span_ids.next(),
            parent_span_id: traced_parent.map(|(span_id, _)| span_id),
            trace_id,
            fields: span_fields.attributes,
            started_at: Timestamp::now(),
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(record) = extensions.get_mut::<SpanRecord>() else {
            return;
        };
        let mut recorded = FieldCollector::default();
        values.record(&mut recorded);
        record.fields.extend(recorded.attributes);
        if record.trace_id.is_none() {
            record.trace_id = derived_trace_id(&record.fields);
        }
    }
}

/// Encodes each event, and each closed span inside a trace, once, and hands the same bytes to
/// the recorder and, at the file's level, to the log file. One encoding means one timestamp: a
/// recorder dump then repeats the log file's lines byte for byte, and a reader keeps each once.
/// The layer runs under the recorder's filter, which keeps everything the file's does.
pub(crate) struct LineLayer<F, R> {
    file: F,
    file_rule: FileRule,
    recorder: R,
    resource: Arc<Resource>,
}

impl<F, R> LineLayer<F, R> {
    pub(crate) fn new(file: F, file_rule: FileRule, recorder: R, resource: Arc<Resource>) -> Self {
        Self {
            file,
            file_rule,
            recorder,
            resource,
        }
    }

    fn take_line(&self, metadata: &Metadata<'_>, at: Timestamp, line: Vec<u8>)
    where
        F: LineSink,
        R: LineSink,
    {
        let _inside = SinkEntry::enter();
        if self.file_rule.keeps(metadata) {
            self.file.take_line(at, line.clone());
        }
        self.recorder.take_line(at, line);
    }
}

impl<S, F, R> tracing_subscriber::Layer<S> for LineLayer<F, R>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    F: LineSink,
    R: LineSink,
{
    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let at = Timestamp::now();
        let mut collector = FieldCollector::default();
        // Outer first: (span id, trace id, name) of each enclosing span.
        let mut enclosing: Vec<(SpanId, Option<TraceId>, &'static str)> = Vec::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(record) = span.extensions().get::<SpanRecord>() {
                    for (name, value) in &record.fields {
                        collector.attributes.insert(name.clone(), value.clone());
                    }
                    enclosing.push((record.span_id, record.trace_id, span.name()));
                }
            }
        }
        let mut own_fields = FieldCollector::default();
        event.record(&mut own_fields);
        // The event's own `call.id` names its call even inside another call's span.
        let own_trace_id = derived_trace_id(&own_fields.attributes);
        collector.attributes.extend(own_fields.attributes);
        collector.body = own_fields.body;
        collector.log_target = own_fields.log_target;
        let trace_id = own_trace_id
            .or_else(|| {
                enclosing
                    .iter()
                    .rev()
                    .find_map(|(_, trace_id, _)| *trace_id)
            })
            .or_else(|| derived_trace_id(&collector.attributes));
        // A span id outside the line's trace would point into another trace.
        let innermost = trace_id.and_then(|trace_id| {
            enclosing
                .iter()
                .rev()
                .find(|(_, span_trace_id, _)| *span_trace_id == Some(trace_id))
                .copied()
        });
        let metadata = event.metadata();
        let level = *metadata.level();
        let line = LogLine {
            record: RecordKind::Log,
            timestamp: at,
            severity_text: level.as_str(),
            severity_number: severity_number(level),
            body: collector.body.as_deref().unwrap_or_default(),
            service_name: self.resource.service_name,
            instance_id: &self.resource.instance_id,
            scope: collector
                .log_target
                .as_deref()
                .unwrap_or_else(|| metadata.target()),
            trace_id,
            span_id: innermost.map(|(span_id, _, _)| span_id),
            span: innermost.map(|(_, _, name)| SpanName { name }),
            attributes: &collector.attributes,
        };
        if let Some(encoded) = encode(&line) {
            self.take_line(metadata, at, encoded);
        }
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        let extensions = span.extensions();
        let Some(record) = extensions.get::<SpanRecord>() else {
            return;
        };
        // Spans outside a call or a run would only repeat what their events say.
        let Some(trace_id) = record.trace_id else {
            return;
        };
        let at = Timestamp::now();
        let line = SpanLine {
            record: RecordKind::Span,
            timestamp: at,
            service_name: self.resource.service_name,
            instance_id: &self.resource.instance_id,
            scope: span.metadata().target(),
            trace_id,
            span_id: record.span_id,
            span: ClosedSpan {
                name: span.name(),
                start_timestamp: record.started_at,
                parent_span_id: record.parent_span_id,
            },
            attributes: &record.fields,
        };
        if let Some(encoded) = encode(&line) {
            self.take_line(span.metadata(), at, encoded);
        }
    }
}

thread_local! {
    static IS_INSIDE_SINK: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is inside a sink, which may hold the log file's or the ring's lock. A
/// panic there must not log: the line would wait on the lock this thread holds.
pub(crate) fn is_inside_sink() -> bool {
    IS_INSIDE_SINK.with(Cell::get)
}

/// Marks the thread inside a sink until dropped. A sink logs too, as the old file removal does,
/// so the drop restores the mark it found.
struct SinkEntry {
    was_inside: bool,
}

impl SinkEntry {
    fn enter() -> Self {
        Self {
            was_inside: IS_INSIDE_SINK.with(|inside| inside.replace(true)),
        }
    }
}

impl Drop for SinkEntry {
    fn drop(&mut self) {
        IS_INSIDE_SINK.with(|inside| inside.set(self.was_inside));
    }
}

/// One line with its newline. `None` only for a value serde cannot write, which the fields
/// below never hold.
pub(crate) fn encode(line: &impl Serialize) -> Option<Vec<u8>> {
    let mut encoded = serde_json::to_vec(line).ok()?;
    encoded.push(b'\n');
    Some(encoded)
}

/// The trace a span or an event starts from its own fields: a call's from `call.id`, a
/// pipeline attempt's from `meeting.id` and `mc.input.revision` together.
fn derived_trace_id(attributes: &Map<String, Value>) -> Option<TraceId> {
    if let Some(call_id) = attributes.get(fields::CALL_ID) {
        return Some(TraceId::for_call(&id_text(call_id)));
    }
    match (
        attributes.get(fields::MEETING_ID),
        attributes.get(fields::INPUT_REVISION),
    ) {
        (Some(meeting_id), Some(revision)) => Some(TraceId::for_pipeline_run(
            &id_text(meeting_id),
            &id_text(revision),
        )),
        (None, _) | (_, None) => None,
    }
}

// The id as the process logged it: a string as is, a number in decimal.
fn id_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

// OpenTelemetry's severity numbers: the first of each range.
fn severity_number(level: Level) -> u8 {
    if level == Level::ERROR {
        17
    } else if level == Level::WARN {
        13
    } else if level == Level::INFO {
        9
    } else if level == Level::DEBUG {
        5
    } else {
        1
    }
}

/// Collects fields into attributes and takes `message` out as the body.
#[derive(Default)]
struct FieldCollector {
    body: Option<String>,
    attributes: Map<String, Value>,
    /// The target of a `log` crate record, which `tracing` hides behind its own.
    log_target: Option<String>,
}

impl FieldCollector {
    fn put(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => {
                self.body = Some(match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                });
            }
            "log.target" => {
                if let Value::String(text) = value {
                    self.log_target = Some(text);
                }
            }
            // Where a `log` record came from in the dependency's source: noise for this app.
            "log.module_path" | "log.file" | "log.line" => {}
            name => {
                self.attributes.insert(name.to_owned(), value);
            }
        }
    }
}

impl Visit for FieldCollector {
    fn record_f64(&mut self, field: &Field, value: f64) {
        let number = serde_json::Number::from_f64(value)
            .map_or_else(|| Value::String(value.to_string()), Value::Number);
        self.put(field, number);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_owned()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }
}
