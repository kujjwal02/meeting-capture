//! `mc debug timeline`: a bundle's lines on one clock, in IST.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write;

use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};

use super::bundle::{Bundle, BundleLine, Entry, LogEntry, MetricPoint, MetricRow};

const SELF_SPEAKING: &str = "mc.meet.self_speaking";
const LEVEL_RMS: &str = "mc.audio.level.rms";
const CAPTION_FRAMES: &str = "mc.captions.frames";
const CAPTION_ACKS: &str = "mc.captions.acks";
/// The per-minute view counts a channel's acks next to its frame kinds.
const ACKS_KIND: &str = "acks";
/// The order the per-minute view prints a channel's counts in.
const COUNT_KINDS: [&str; 5] = ["raw", "parsed", "control", "rejected", ACKS_KIND];
const CAPTION_PRESS: &str = "mc.caption.press";
const CALL_PROBLEM: &str = "call.problem";
/// A minute with more of the user's speech than this and no parsed caption frame is flagged.
const SPEECH_FLAG_MS: f64 = 20_000.0;
/// What a metric row stands for when it does not say.
const DEFAULT_INTERVAL_MS: u64 = 1_000;

fn ist() -> TimeZone {
    TimeZone::fixed(Offset::constant(5).saturating_add(SignedDuration::from_mins(30)))
}

fn is_shown(line: &BundleLine, service: Option<&str>) -> bool {
    service.is_none_or(|service| line.service == service)
}

/// One row per line: time, service, severity and what happened.
pub(crate) fn print_lines(
    bundle: &Bundle,
    service: Option<&str>,
    mut out: impl Write,
) -> std::io::Result<()> {
    let time_zone = ist();
    for line in bundle.lines.iter().filter(|line| is_shown(line, service)) {
        let severity = match &line.entry {
            Entry::Log(log) => log.severity.text(),
            Entry::Resource
            | Entry::Dump { .. }
            | Entry::Span(_)
            | Entry::Metric(_)
            | Entry::Env(_) => "",
        };
        writeln!(
            out,
            "{}  {:<11}  {severity:<5}  {}",
            line.at
                .to_zoned(time_zone.clone())
                .strftime("%Y-%m-%d %H:%M:%S%.3f IST"),
            line.service,
            describe(bundle, line),
        )?;
    }
    Ok(())
}

fn describe(bundle: &Bundle, line: &BundleLine) -> String {
    match &line.entry {
        Entry::Resource => {
            let resource = line
                .instance_id
                .as_ref()
                .and_then(|instance_id| bundle.resources.get(instance_id));
            let field = |key: &str| {
                resource
                    .and_then(|resource| resource.get(key))
                    .map_or_else(|| "?".to_owned(), value_text)
            };
            format!(
                "process started: version {}, pid {}",
                field("service.version"),
                field("process.pid")
            )
        }
        Entry::Dump { reason } => {
            format!("recorder dump: {}", reason.as_deref().unwrap_or("?"))
        }
        Entry::Log(log) => describe_log(log),
        Entry::Span(span) => {
            let length = line.at.duration_since(span.started_at);
            format!(
                "span {} ended after {:.1} s",
                span.name,
                length.as_secs_f64()
            )
        }
        Entry::Metric(row) => describe_metric_row(row),
        Entry::Env(fields) => describe_environment(fields),
    }
}

fn describe_log(log: &LogEntry) -> String {
    let mut text = log.body.clone();
    let notes: Vec<String> = ["event.name", "error.type"]
        .into_iter()
        .filter_map(|key| {
            let value = log.attributes.get(key)?;
            // The body often is the event name already.
            (value.as_str() != Some(log.body.as_str()))
                .then(|| format!("{key}={}", value_text(value)))
        })
        .collect();
    if !notes.is_empty() {
        let _infallible = write!(text, " ({})", notes.join(", "));
    }
    text
}

fn describe_metric_row(row: &MetricRow) -> String {
    let points: Vec<String> = row
        .points
        .iter()
        .map(|point| {
            let name = point.name.strip_prefix("mc.").unwrap_or(&point.name);
            // Ids add nothing a reader can compare at a glance.
            let series: Vec<String> = point
                .attributes
                .iter()
                .filter(|(key, _)| key.rsplit_once('.').is_none_or(|(_, last)| last != "id"))
                .map(|(_, value)| value_text(value))
                .collect();
            let series = if series.is_empty() {
                String::new()
            } else {
                format!("[{}]", series.join("/"))
            };
            let unit = match point.unit.as_str() {
                "" | "1" => String::new(),
                unit => format!(" {unit}"),
            };
            format!("{name}{series}={}{unit}", number_text(point))
        })
        .collect();
    format!("metrics: {}", points.join(", "))
}

fn describe_environment(fields: &Map<String, Value>) -> String {
    let nested = |outer: &str, inner: &str| {
        fields
            .get(outer)
            .and_then(|object| object.get(inner))
            .map(value_text)
    };
    let mut text = format!(
        "environment: {}",
        fields
            .get("reason")
            .map_or_else(|| "?".to_owned(), value_text)
    );
    let parts = [
        ("input", nested("mc.audio.input.default", "transport")),
        ("output", nested("mc.audio.output.default", "transport")),
        ("mic", nested("mc.audio.mic.chosen", "reason")),
        ("power", nested("mc.power", "source")),
        ("chrome", nested("mc.browser", "chrome.version")),
        ("mc-meet", nested("mc.browser", "mc_meet.version")),
    ];
    for (label, value) in parts {
        if let Some(value) = value {
            let _infallible = write!(text, ", {label} {value}");
        }
    }
    text
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn number_text(point: &MetricPoint) -> String {
    if point.value.is_f64() {
        format!("{:.1}", point.value_f64())
    } else {
        point.value.to_string()
    }
}

/// What one minute of the bundle holds.
#[derive(Default)]
struct Minute {
    first_at: Option<Timestamp>,
    lines: u32,
    speech_ms: Option<f64>,
    mic_levels: Vec<f64>,
    remote_levels: Vec<f64>,
    /// New caption frames and acks per channel, keyed by channel name and then by frame kind
    /// or `acks`.
    channels: BTreeMap<String, BTreeMap<String, f64>>,
    presses: u32,
    problems: Vec<String>,
}

impl Minute {
    fn parsed_frames(&self) -> f64 {
        self.channels
            .values()
            .filter_map(|counts| counts.get("parsed"))
            .sum()
    }
}

/// One row per minute: speech, levels, caption frames, acks, presses and problems. Flags a
/// minute of speech with no parsed caption frame, the sign of captions lost between Meet and
/// mc-meet.
pub(crate) fn print_by_minute(
    bundle: &Bundle,
    service: Option<&str>,
    mut out: impl Write,
) -> std::io::Result<()> {
    let mut minutes: BTreeMap<i64, Minute> = BTreeMap::new();
    let mut counters = Counters {
        // A resource line can be hours older than the rest: a process writes it at its start.
        bundle_start: bundle
            .lines
            .iter()
            .find(|line| !matches!(line.entry, Entry::Resource))
            .map(|line| line.at),
        last_totals: HashMap::new(),
    };
    for line in bundle.lines.iter().filter(|line| is_shown(line, service)) {
        let minute = minutes
            .entry(line.at.as_second().div_euclid(60))
            .or_default();
        minute.first_at.get_or_insert(line.at);
        minute.lines += 1;
        match &line.entry {
            Entry::Metric(row) => add_metric_row(minute, row, line, &mut counters),
            Entry::Log(log) => {
                let event_name = log.attributes.get("event.name").and_then(Value::as_str);
                if log.body == CAPTION_PRESS || event_name == Some(CAPTION_PRESS) {
                    minute.presses += 1;
                }
                if event_name == Some(CALL_PROBLEM) {
                    minute.problems.push(
                        log.attributes
                            .get("error.type")
                            .map_or_else(|| log.body.clone(), value_text),
                    );
                }
            }
            Entry::Resource | Entry::Dump { .. } | Entry::Span(_) | Entry::Env(_) => {}
        }
    }
    let time_zone = ist();
    for minute in minutes.values() {
        let Some(first_at) = minute.first_at else {
            continue;
        };
        let mut row = format!(
            "{}  lines {:>4}",
            first_at
                .to_zoned(time_zone.clone())
                .strftime("%Y-%m-%d %H:%M IST"),
            minute.lines
        );
        let _infallible = write!(
            row,
            "  speech {}  mic {}  remote {} dBFS  captions {}  presses {}  problems {}",
            minute.speech_ms.map_or_else(
                || "-".to_owned(),
                |speech_ms| format!("{:.0} s", speech_ms / 1000.0)
            ),
            mean_text(&minute.mic_levels),
            mean_text(&minute.remote_levels),
            channels_text(&minute.channels),
            minute.presses,
            if minute.problems.is_empty() {
                "-".to_owned()
            } else {
                minute.problems.join(",")
            },
        );
        if minute
            .speech_ms
            .is_some_and(|speech_ms| speech_ms > SPEECH_FLAG_MS)
            && minute.parsed_frames() == 0.0
        {
            row.push_str("  FLAG: speech with no parsed caption frame");
        }
        writeln!(out, "{row}")?;
    }
    Ok(())
}

fn add_metric_row(
    minute: &mut Minute,
    row: &MetricRow,
    line: &BundleLine,
    counters: &mut Counters,
) {
    let interval_ms = row.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS);
    for point in &row.points {
        match point.name.as_str() {
            SELF_SPEAKING => {
                // A 0 or 1 sample stands for the whole interval.
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "an interval is far under 2^52 ms"
                )]
                let spoken_ms = point.value_f64() * interval_ms as f64;
                *minute.speech_ms.get_or_insert(0.0) += spoken_ms;
            }
            LEVEL_RMS => match point.attribute("mc.audio.stem").as_deref() {
                Some("mic") => minute.mic_levels.push(point.value_f64()),
                Some("remote") => minute.remote_levels.push(point.value_f64()),
                _ => {}
            },
            CAPTION_FRAMES => {
                let added = counters.added_since_last(point, line);
                let kind = point
                    .attribute("mc.caption.frame.kind")
                    .unwrap_or_else(|| "?".to_owned());
                *minute
                    .channels
                    .entry(channel_name(point))
                    .or_default()
                    .entry(kind)
                    .or_default() += added;
            }
            CAPTION_ACKS => {
                let added = counters.added_since_last(point, line);
                *minute
                    .channels
                    .entry(channel_name(point))
                    .or_default()
                    .entry(ACKS_KIND.to_owned())
                    .or_default() += added;
            }
            _ => {}
        }
    }
}

/// Turns each cumulative series' totals into what each row added.
struct Counters {
    /// The bundle's first line other than a resource line. A counter started before it counted
    /// things the bundle does not show.
    bundle_start: Option<Timestamp>,
    /// Per series, its start and last total.
    last_totals: HashMap<String, (Option<Timestamp>, f64)>,
}

impl Counters {
    // A series' first row is a baseline, unless the counter started inside the bundle. A new
    // start or a total below the last one is a counter that started again, such as a reloaded
    // Meet tab, so all of it is new.
    fn added_since_last(&mut self, point: &MetricPoint, line: &BundleLine) -> f64 {
        let series = format!(
            "{}|{}|{}|{}",
            line.service,
            line.instance_id.as_deref().unwrap_or_default(),
            point.name,
            Value::Object(point.attributes.clone())
        );
        let total = point.value_f64();
        let last = self.last_totals.insert(series, (point.start, total));
        match last {
            Some((last_start, last_total)) if last_start == point.start && total >= last_total => {
                total - last_total
            }
            Some(_) => total,
            None => {
                let is_started_inside = point
                    .start
                    .zip(self.bundle_start)
                    .is_some_and(|(start, bundle_start)| start >= bundle_start);
                if is_started_inside { total } else { 0.0 }
            }
        }
    }
}

/// The mean level of dBFS samples, taken over their power: an arithmetic mean of decibels
/// would weigh a quiet second like a loud one.
fn mean_text(levels: &[f64]) -> String {
    if levels.is_empty() {
        return "-".to_owned();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a minute holds a few thousand samples"
    )]
    let count = levels.len() as f64;
    let mean_power = levels
        .iter()
        .map(|level| 10_f64.powf(level / 10.0))
        .sum::<f64>()
        / count;
    format!("{:.1}", 10.0 * mean_power.log10())
}

/// A caption channel as the per-minute view names it: whose it is, its label and its id, such
/// as `own captions_v2#6` or `meet captions_v2#125`. mc-meet's channels and Meet's own carry
/// the same labels, so the label alone mixes them.
fn channel_name(point: &MetricPoint) -> String {
    let owner = match point.attributes.get("mc.caption.channel.is_own") {
        Some(Value::Bool(true)) => "own",
        Some(Value::Bool(false)) => "meet",
        _ => "?",
    };
    let label = point
        .attribute("mc.caption.channel.label")
        .unwrap_or_else(|| "?".to_owned());
    let id = point
        .attribute("mc.caption.channel.id")
        .unwrap_or_else(|| "?".to_owned());
    format!("{owner} {label}#{id}")
}

/// Each channel that counted anything in the minute, with its counts in a fixed order. A
/// channel with no new frame and no ack is left out.
fn channels_text(channels: &BTreeMap<String, BTreeMap<String, f64>>) -> String {
    let shown = channels
        .iter()
        .filter(|(_, counts)| counts.values().any(|count| *count > 0.0))
        .map(|(channel, counts)| {
            let ordered = COUNT_KINDS
                .iter()
                .map(|kind| (*kind, counts.get(*kind).copied().unwrap_or_default()));
            let others = counts
                .iter()
                .filter(|(kind, _)| !COUNT_KINDS.contains(&kind.as_str()))
                .map(|(kind, count)| (kind.as_str(), *count));
            let counts_text = ordered
                .chain(others)
                .map(|(kind, count)| format!("{kind} {count:.0}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{channel}: {counts_text}")
        })
        .collect::<Vec<_>>();
    if shown.is_empty() {
        "-".to_owned()
    } else {
        shown.join("; ")
    }
}
