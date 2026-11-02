//! The assertions on one finished case: its problem lines, its recorder dumps, and the replay
//! of its lines.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::time::Duration;

use jiff::Timestamp;
use serde_json::Value;
use telemetry::events;
use telemetry::parsed::{Entry, Line};

use crate::cases::{Case, Expect};
use crate::driver::{CasePaths, CaseRun};

/// The binaries and folders the checks read.
pub(crate) struct CheckTools<'a> {
    pub mc_path: &'a Path,
    pub log_dir: &'a Path,
}

/// One `call.problem` or `call.recovered` line of the case.
struct ProblemLine {
    event: String,
    code: String,
    at: Timestamp,
}

/// The first failed assertion, or `None` when the case passes.
pub(crate) fn check_case(
    tools: &CheckTools<'_>,
    case: &Case,
    paths: &CasePaths,
    run: &CaseRun,
) -> anyhow::Result<Option<String>> {
    let (raw_lines, lines) = case_log_lines(tools.log_dir, paths)?;
    let problem_lines: Vec<ProblemLine> = lines.iter().filter_map(problem_line).collect();
    let failure = match case.expect {
        Expect::NoProblem => check_quiet(&problem_lines, paths),
        Expect::Problem { .. } => {
            check_problem(case, &problem_lines, run).or_else(|| check_dumps(case, paths))
        }
    };
    if failure.is_some() {
        return Ok(failure);
    }
    write_replay_log(paths, &raw_lines)?;
    check_replay(tools.mc_path, paths)
}

/// The case's lines from the check's log files, raw and parsed. The first line of each file,
/// its resource, comes along so the bundle knows the service.
fn case_log_lines(log_dir: &Path, paths: &CasePaths) -> anyhow::Result<(Vec<String>, Vec<Line>)> {
    let mut raw_lines = Vec::new();
    let mut lines = Vec::new();
    for file in telemetry::daily_log_files(log_dir, telemetry::Service::CaptureCheck)? {
        let text = std::fs::read_to_string(&file)?;
        for (index, raw) in text.lines().enumerate() {
            let Ok(line) = telemetry::parsed::parse_line(raw.as_bytes()) else {
                continue;
            };
            let is_resource = matches!(line.entry, Entry::Resource(_));
            if line.trace_id() == Some(paths.trace.0.as_str()) {
                raw_lines.push(raw.to_owned());
                lines.push(line);
            } else if index == 0 && is_resource {
                raw_lines.push(raw.to_owned());
            }
        }
    }
    Ok((raw_lines, lines))
}

fn problem_line(line: &Line) -> Option<ProblemLine> {
    let Entry::Log(entry) = &line.entry else {
        return None;
    };
    let event = entry.event_name()?;
    if event != events::CALL_PROBLEM && event != events::CALL_RECOVERED {
        return None;
    }
    Some(ProblemLine {
        event: event.to_owned(),
        code: entry
            .attributes
            .get(events::ERROR_TYPE)?
            .as_str()?
            .to_owned(),
        at: line.at,
    })
}

fn check_quiet(problem_lines: &[ProblemLine], paths: &CasePaths) -> Option<String> {
    if let Some(line) = problem_lines
        .iter()
        .find(|line| line.event == events::CALL_PROBLEM)
    {
        return Some(format!(
            "raised {} where no problem was expected",
            line.code
        ));
    }
    let dumps = recorder_files(&paths.debug_folder);
    (!dumps.is_empty()).then(|| format!("{} recorder dump(s) with no problem", dumps.len()))
}

fn check_problem(case: &Case, problem_lines: &[ProblemLine], run: &CaseRun) -> Option<String> {
    let Expect::Problem {
        code,
        raised_from_s,
        raised_by_s,
        still_raised_at_s,
        cleared_by_s,
    } = case.expect
    else {
        return None;
    };
    let seconds_in = |at: Timestamp| at.duration_since(run.started_at).as_secs_f64();
    let Some(raised) = problem_lines
        .iter()
        .find(|line| line.event == events::CALL_PROBLEM && line.code == code)
    else {
        let others: Vec<&str> = problem_lines
            .iter()
            .map(|line| line.code.as_str())
            .collect();
        return Some(format!(
            "no call.problem {code} (lines: {})",
            others.join(" ")
        ));
    };
    // The edge carries the detector's own time; the line follows at the next tick.
    let raised_at = run
        .raised_at(code)
        .map_or_else(|| seconds_in(raised.at), seconds_in);
    if !(window_start(raised_from_s)..=window_end(raised_by_s)).contains(&raised_at) {
        return Some(format!(
            "{code} raised at {raised_at:.1} s, outside [{raised_from_s}, {raised_by_s}]"
        ));
    }
    let cleared_at = problem_lines
        .iter()
        .find(|line| {
            line.event == events::CALL_RECOVERED && line.code == code && line.at >= raised.at
        })
        .map(|line| seconds_in(line.at));
    if let (Some(still_s), Some(cleared_s)) = (still_raised_at_s, cleared_at)
        && cleared_s < window_start(still_s)
    {
        return Some(format!(
            "{code} cleared at {cleared_s:.1} s, before {still_s} s"
        ));
    }
    match (cleared_by_s, cleared_at) {
        (Some(by_s), None) => Some(format!("{code} not cleared by {by_s} s")),
        (Some(by_s), Some(cleared_s)) if cleared_s > window_end(by_s) => Some(format!(
            "{code} cleared at {cleared_s:.1} s, after {by_s} s"
        )),
        (Some(_) | None, _) => None,
    }
}

/// A window's start, with half a second of grace: the case clock starts before the helper.
fn window_start(seconds: u64) -> f64 {
    Duration::from_secs(seconds)
        .saturating_sub(Duration::from_millis(500))
        .as_secs_f64()
}

/// A window's end, with one tick of grace: the board logs at its next 1 s tick.
fn window_end(seconds: u64) -> f64 {
    Duration::from_secs(seconds + 1).as_secs_f64()
}

fn recorder_files(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("recorder-"))
        })
        .collect()
}

fn check_dumps(case: &Case, paths: &CasePaths) -> Option<String> {
    let helper_file = format!("recorder-mc-capture-{}.jsonl", case.helper.subcommand());
    // The check dumps for its problem. A helper dumps at `dump_recorder`, which carries no
    // reason, so its dump reads `app_request`, as the app's helper dumps do.
    let dumps = [
        (
            "recorder-capture-check.jsonl",
            telemetry::DumpReason::CallProblem,
        ),
        (helper_file.as_str(), telemetry::DumpReason::AppRequest),
    ];
    for (name, wanted_reason) in dumps {
        let path = paths.debug_folder.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Some(format!("no {name}"));
        };
        let first = text
            .lines()
            .next()
            .and_then(|raw| telemetry::parsed::parse_line(raw.as_bytes()).ok());
        let is_wanted_dump = matches!(
            first.map(|line| line.entry),
            Some(Entry::Dump { reason: Some(reason), .. }) if reason == wanted_reason.as_str()
        );
        if !is_wanted_dump {
            return Some(format!(
                "{name} does not start with a {} dump line",
                wanted_reason.as_str()
            ));
        }
        if name == helper_file && !has_inject_line(&text, case) {
            return Some(format!("{name} has no inject line for {}", case.trigger));
        }
    }
    None
}

/// Whether the helper's dump shows the case's trigger: its `inject.event` line, or for a
/// permission fault, which logs no line of its own, the schedule that holds it.
fn has_inject_line(text: &str, case: &Case) -> bool {
    text.lines()
        .filter_map(|raw| telemetry::parsed::parse_line(raw.as_bytes()).ok())
        .any(|line| {
            let Entry::Log(entry) = &line.entry else {
                return false;
            };
            let field = |key: &str| entry.attributes.get(key).and_then(Value::as_str);
            field("inject.event") == Some(case.trigger)
                || field("inject.schedule").is_some_and(|schedule| {
                    schedule.split(',').any(|entry| {
                        entry.split_once(':').map(|(_, event)| event) == Some(case.trigger)
                    })
                })
        })
}

fn write_replay_log(paths: &CasePaths, raw_lines: &[String]) -> anyhow::Result<()> {
    let mut text = raw_lines.join("\n");
    text.push('\n');
    std::fs::write(paths.replay_folder.join("check-log.jsonl"), text)?;
    Ok(())
}

/// `mc debug replay` over the case's lines must fire what the live run logged, within 2 s, and
/// nothing else.
fn check_replay(mc_path: &Path, paths: &CasePaths) -> anyhow::Result<Option<String>> {
    let output = Command::new(mc_path)
        .args(["debug", "replay"])
        .arg(&paths.replay_folder)
        .args(["--json", "--call", &paths.trace.0])
        .output()?;
    if !output.status.success() {
        return Ok(Some(format!(
            "mc debug replay failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for object in text
        .lines()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
    {
        let kind = object.get("kind").and_then(Value::as_str).unwrap_or("-");
        let problem = object.get("problem").and_then(Value::as_str).unwrap_or("-");
        if object.get("is_logged") == Some(&Value::Bool(false)) {
            return Ok(Some(format!("replay {kind} {problem} was not logged live")));
        }
        if object.get("is_logged_only") == Some(&Value::Bool(true)) {
            return Ok(Some(format!("live {kind} {problem} has no replay twin")));
        }
    }
    Ok(None)
}

impl CaseRun {
    /// When the live detectors first raised `code`.
    fn raised_at(&self, code: &str) -> Option<Timestamp> {
        self.edges.iter().find_map(|edge| match edge.change {
            detectors::Change::Raised(problem) | detectors::Change::Changed { to: problem, .. }
                if problem.code() == code =>
            {
                Some(edge.at)
            }
            detectors::Change::Raised(_)
            | detectors::Change::Changed { .. }
            | detectors::Change::Cleared(_) => None,
        })
    }
}
