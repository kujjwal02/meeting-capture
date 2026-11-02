//! `mc debug replay`: runs the app's detectors over a bundle's lines and prints what they raise,
//! in IST, beside what the app logged live.

use std::collections::HashMap;
use std::io::Write;

use detectors::{
    CallNotice, CallSnapshot, Change, Edge, Host, Notice, Platform, Problem, TraceKey, Verdict,
    verdict,
};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use telemetry::events;
use telemetry::parsed::{Entry, Line};

use super::bundle::Bundle;
use super::timeline::ist;

/// A live line and a replayed firing are the same when this close.
const SAME_FIRING_WITHIN: SignedDuration = SignedDuration::from_secs(2);
/// The field on the warning that a debug folder stopped taking metric rows.
const FOLDER_CAP_FIELD: &str = "mc.debug.folder_max_bytes";

/// What the bundle's app logged about a call's problems, to compare with the replay.
#[derive(Debug, Clone)]
struct LoggedFiring {
    kind: FiringKind,
    /// `None` on a `call.recovered` saved before it named its problem.
    code: Option<String>,
    at: Timestamp,
    is_matched: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FiringKind {
    Raised,
    Noticed,
    Cleared,
}

impl FiringKind {
    fn word(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::Noticed => "notice",
            Self::Cleared => "cleared",
        }
    }

    fn of_event(event: &str) -> Option<Self> {
        match event {
            events::CALL_PROBLEM => Some(Self::Raised),
            events::CALL_PROBLEM_NOTICED => Some(Self::Noticed),
            events::CALL_RECOVERED => Some(Self::Cleared),
            _other => None,
        }
    }
}

/// One replayed firing of a call.
struct Firing {
    at: Timestamp,
    kind: FiringKind,
    problem: Problem,
    evidence: Option<(String, Timestamp)>,
    is_logged: bool,
}

pub(crate) struct ReplayOptions<'a> {
    /// A trace id or a call folder name.
    pub call: Option<&'a str>,
    pub is_json: bool,
}

/// Feeds the bundle to a fresh host, then prints each call's firings and verdict.
pub(crate) fn print_replay(
    bundle: &Bundle,
    options: &ReplayOptions<'_>,
    mut out: impl Write,
) -> std::io::Result<()> {
    let mut host = Host::new();
    let mut logged: HashMap<TraceKey, Vec<LoggedFiring>> = HashMap::new();
    let mut metrics_capped_at: Option<Timestamp> = None;
    // The live board learns a tick's sleeps first; the bundle is one batch.
    host.learn_suspended(bundle.lines.iter().filter(|line| is_detector_input(line)));
    for line in &bundle.lines {
        if line.service == "mc-meet" {
            continue;
        }
        if let Entry::Log(entry) = &line.entry {
            if entry.attributes.contains_key(FOLDER_CAP_FIELD) {
                metrics_capped_at.get_or_insert(line.at);
            }
            if let Some(kind) = entry.event_name().and_then(FiringKind::of_event)
                && let Some(trace) = &entry.trace_id
            {
                logged
                    .entry(TraceKey(trace.clone()))
                    .or_default()
                    .push(LoggedFiring {
                        kind,
                        code: entry
                            .attributes
                            .get(events::ERROR_TYPE)
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        at: line.at,
                        is_matched: false,
                    });
            }
        }
        if is_detector_input(line) {
            host.take_line(line);
        }
    }
    // No time is invented past the bundle.
    if let Some(last) = bundle.lines.iter().map(|line| line.at).max() {
        host.advance_to(last);
    }
    let output = host.drain_output();
    let bundle_end = host.clock().unwrap_or_default();
    let time_zone = ist();
    let calls = host.calls();
    if calls.is_empty() && !options.is_json {
        writeln!(
            out,
            "no call lines: the bundle holds no detector line or call metric row with a trace id"
        )?;
    }
    for call in calls {
        let Some(snapshot) = host.snapshot(&call) else {
            continue;
        };
        if !is_chosen(&call, &snapshot, options.call) {
            continue;
        }
        let calls_logged = logged.entry(call.clone()).or_default();
        let firings = firings_of(&call, &output.edges, &output.notices, calls_logged);
        let call_end = snapshot.ended_at.unwrap_or(bundle_end);
        let call_verdict = verdict(&snapshot.history, call_end);
        if options.is_json {
            print_json(
                &mut out,
                &call,
                &firings,
                calls_logged,
                call_verdict.as_ref(),
            )?;
        } else {
            print_text(
                &mut out,
                &CallReport {
                    call: &call,
                    snapshot: &snapshot,
                    firings: &firings,
                    logged: calls_logged,
                    verdict: call_verdict.as_ref(),
                    bundle_end,
                    metrics_capped_at,
                },
                &time_zone,
            )?;
        }
    }
    Ok(())
}

/// The lines the live tap hands the detectors: their events, and call metric rows.
fn is_detector_input(line: &Line) -> bool {
    match &line.entry {
        Entry::Log(entry) => entry
            .event_name()
            .is_some_and(|name| events::DETECTOR_EVENTS.contains(&name)),
        Entry::Metric(row) => row.trace_id.is_some(),
        Entry::Resource(_) | Entry::Dump { .. } | Entry::Span(_) | Entry::Env(_) => false,
    }
}

/// A call's folder name: its call log's file name without the extension.
fn call_folder(snapshot: &CallSnapshot) -> Option<&str> {
    let call_id = snapshot.call_id.as_deref()?;
    let file_name = call_id.rsplit(['/', '\\']).next()?;
    Some(file_name.strip_suffix(".jsonl").unwrap_or(file_name))
}

fn is_chosen(call: &TraceKey, snapshot: &CallSnapshot, wanted: Option<&str>) -> bool {
    wanted.is_none_or(|wanted| call.0 == wanted || call_folder(snapshot) == Some(wanted))
}

fn firings_of(
    call: &TraceKey,
    edges: &[Edge],
    notices: &[CallNotice],
    logged: &mut [LoggedFiring],
) -> Vec<Firing> {
    let mut firings = Vec::new();
    for edge in edges.iter().filter(|edge| edge.call == *call) {
        let evidence = Some((edge.evidence.event.clone(), edge.evidence.at));
        let mut push = |kind, problem| {
            firings.push(Firing {
                at: edge.at,
                kind,
                problem,
                evidence: evidence.clone(),
                is_logged: false,
            });
        };
        match edge.change {
            Change::Raised(problem) => push(FiringKind::Raised, problem),
            Change::Changed { from, to } => {
                push(FiringKind::Cleared, from);
                push(FiringKind::Raised, to);
            }
            Change::Cleared(problem) => push(FiringKind::Cleared, problem),
        }
    }
    for notice in notices.iter().filter(|notice| notice.call == *call) {
        let problem = match notice.notice {
            Notice::NotRecording(problem) | Notice::CaptureTrouble(problem) => problem,
            Notice::Recording
            | Notice::RecordingAgain
            | Notice::NoTranscript
            | Notice::CaptureBack(_) => continue,
        };
        firings.push(Firing {
            at: notice.at,
            kind: FiringKind::Noticed,
            problem,
            evidence: None,
            is_logged: false,
        });
    }
    firings.sort_by_key(|firing| firing.at);
    for firing in &mut firings {
        let twin = logged.iter_mut().find(|line| {
            !line.is_matched
                && line.kind == firing.kind
                && line
                    .code
                    .as_deref()
                    .is_none_or(|code| code == firing.problem.code())
                && line.at.duration_since(firing.at).abs() <= SAME_FIRING_WITHIN
        });
        if let Some(twin) = twin {
            twin.is_matched = true;
            firing.is_logged = true;
        }
    }
    firings
}

struct CallReport<'a> {
    call: &'a TraceKey,
    snapshot: &'a CallSnapshot,
    firings: &'a [Firing],
    logged: &'a [LoggedFiring],
    verdict: Option<&'a Verdict>,
    bundle_end: Timestamp,
    metrics_capped_at: Option<Timestamp>,
}

fn print_text(
    out: &mut impl Write,
    report: &CallReport<'_>,
    time_zone: &TimeZone,
) -> std::io::Result<()> {
    let clock = |at: Timestamp| {
        at.to_zoned(time_zone.clone())
            .strftime("%H:%M:%S IST")
            .to_string()
    };
    let snapshot = report.snapshot;
    let platform = match snapshot.platform {
        Some(Platform::Meet) => "Meet",
        Some(Platform::SlackHuddle) => "Slack huddle",
        None => "platform unknown",
    };
    writeln!(
        out,
        "call {}  trace {}  {platform}",
        call_folder(snapshot).unwrap_or("-"),
        report.call.0
    )?;
    let end = snapshot.ended_at.map_or_else(
        || format!("{} (bundle end)", clock(report.bundle_end)),
        clock,
    );
    writeln!(
        out,
        "  {} to {end}, {} captions",
        clock(snapshot.opened_at),
        snapshot.caption_count
    )?;
    let mut rows: Vec<(Timestamp, String)> = report
        .firings
        .iter()
        .map(|firing| {
            let layer = match firing.kind {
                FiringKind::Raised => firing.problem.layer().code(),
                FiringKind::Noticed | FiringKind::Cleared => "",
            };
            let evidence = firing
                .evidence
                .as_ref()
                .map_or_else(String::new, |(event, at)| {
                    format!("{event} {}", clock(*at).trim_end_matches(" IST"))
                });
            let logged = if firing.is_logged {
                "logged"
            } else {
                "not logged"
            };
            (
                firing.at,
                format!(
                    "  {}  {:<8} {:<28} {:<15} {:<40} {logged}",
                    clock(firing.at),
                    firing.kind.word(),
                    firing.problem.code(),
                    layer,
                    evidence
                ),
            )
        })
        .collect();
    rows.extend(
        report
            .logged
            .iter()
            .filter(|line| !line.is_matched)
            .map(|line| {
                (
                    line.at,
                    format!(
                        "  {}  {:<8} {:<28} {:<15} {:<40} logged only",
                        clock(line.at),
                        line.kind.word(),
                        line.code.as_deref().unwrap_or("-"),
                        "",
                        ""
                    ),
                )
            }),
    );
    rows.sort_by_key(|(at, _)| *at);
    for (_, row) in rows {
        writeln!(out, "{}", row.trim_end())?;
    }
    print_verdict(out, report.verdict, &clock)?;
    if let Some(at) = report.metrics_capped_at {
        writeln!(out, "  metrics capped at {}", clock(at))?;
    }
    writeln!(out)
}

fn print_verdict(
    out: &mut impl Write,
    verdict: Option<&Verdict>,
    clock: &impl Fn(Timestamp) -> String,
) -> std::io::Result<()> {
    match verdict {
        Some(verdict) => {
            writeln!(
                out,
                "  failing layer: {} ({} at {}, {}{})",
                verdict.layer.code(),
                verdict.problem.code(),
                clock(verdict.raised_at),
                spoken_duration(verdict.duration),
                if verdict.is_noticed { ", noticed" } else { "" }
            )?;
            for part in &verdict.others {
                writeln!(
                    out,
                    "  also: {} {} {}",
                    part.layer.code(),
                    part.problem.code(),
                    spoken_duration(part.duration)
                )?;
            }
        }
        None => writeln!(out, "  failing layer: none")?,
    }
    Ok(())
}

/// "1 min 19 s", "3 h 2 min", "4 s".
fn spoken_duration(duration: SignedDuration) -> String {
    let seconds = duration.as_secs().max(0);
    match (seconds / 3600, seconds % 3600 / 60, seconds % 60) {
        (0, 0, seconds) => format!("{seconds} s"),
        (0, minutes, seconds) => format!("{minutes} min {seconds} s"),
        (hours, minutes, _) => format!("{hours} h {minutes} min"),
    }
}

fn print_json(
    out: &mut impl Write,
    call: &TraceKey,
    firings: &[Firing],
    logged: &[LoggedFiring],
    verdict: Option<&Verdict>,
) -> std::io::Result<()> {
    for firing in firings {
        let object = json!({
            "trace_id": call.0,
            "at": firing.at,
            "kind": firing.kind.word(),
            "problem": firing.problem.code(),
            "layer": firing.problem.layer().code(),
            "evidence": firing.evidence.as_ref().map(|(event, _)| event),
            "evidence_at": firing.evidence.as_ref().map(|(_, at)| at),
            "is_logged": firing.is_logged,
        });
        writeln!(out, "{object}")?;
    }
    for line in logged.iter().filter(|line| !line.is_matched) {
        let object = json!({
            "trace_id": call.0,
            "at": line.at,
            "kind": line.kind.word(),
            "problem": line.code,
            "is_logged_only": true,
        });
        writeln!(out, "{object}")?;
    }
    if let Some(verdict) = verdict {
        let object = json!({
            "trace_id": call.0,
            "verdict": {
                "layer": verdict.layer.code(),
                "problem": verdict.problem.code(),
                "raised_at": verdict.raised_at,
                "duration_s": verdict.duration.as_secs(),
                "is_noticed": verdict.is_noticed,
            },
        });
        writeln!(out, "{object}")?;
    }
    Ok(())
}
