//! Reads the fields detectors need from a parsed line. A missing or mistyped field reads as
//! absent: a detector then keeps its state, as it would without the line.

use jiff::Timestamp;
use serde_json::Value;
use telemetry::events;
use telemetry::parsed::{Entry, Line, MetricRow};

/// A log line's `event.name`.
pub(crate) fn event_name(line: &Line) -> Option<&str> {
    match &line.entry {
        Entry::Log(entry) => entry.attributes.get(events::EVENT_NAME)?.as_str(),
        Entry::Resource(_)
        | Entry::Dump { .. }
        | Entry::Span(_)
        | Entry::Metric(_)
        | Entry::Env(_) => None,
    }
}

fn attribute<'a>(line: &'a Line, key: &str) -> Option<&'a Value> {
    match &line.entry {
        Entry::Log(entry) => entry.attributes.get(key),
        Entry::Resource(_)
        | Entry::Dump { .. }
        | Entry::Span(_)
        | Entry::Metric(_)
        | Entry::Env(_) => None,
    }
}

pub(crate) fn text<'a>(line: &'a Line, key: &str) -> Option<&'a str> {
    attribute(line, key)?.as_str()
}

pub(crate) fn flag(line: &Line, key: &str) -> Option<bool> {
    attribute(line, key)?.as_bool()
}

pub(crate) fn count(line: &Line, key: &str) -> Option<u64> {
    attribute(line, key)?.as_u64()
}

pub(crate) fn time(line: &Line, key: &str) -> Option<Timestamp> {
    text(line, key)?.parse().ok()
}

/// The row's points named `name` whose `mc.audio.stem` is `stem`, as numbers.
pub(crate) fn stem_point(row: &MetricRow, name: &str, stem: &str) -> Option<f64> {
    row.points
        .iter()
        .find(|point| {
            point.name == name
                && point
                    .attributes
                    .get(events::AUDIO_STEM)
                    .and_then(Value::as_str)
                    == Some(stem)
        })
        .and_then(|point| point.value.as_f64())
}

pub(crate) fn point(row: &MetricRow, name: &str) -> Option<f64> {
    row.points
        .iter()
        .find(|point| point.name == name)
        .and_then(|point| point.value.as_f64())
}

/// How long a row covers. Level rows carry no interval: one comes per 1 s heartbeat.
pub(crate) fn row_seconds(row: &MetricRow) -> f64 {
    const HEARTBEAT_MS: u64 = 1000;
    std::time::Duration::from_millis(row.interval_ms.unwrap_or(HEARTBEAT_MS)).as_secs_f64()
}
