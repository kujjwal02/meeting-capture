//! The lines a judge of calls logs about its verdicts: `call.problem`, `call.recovered` and
//! `call.problem_noticed`. The app's status board and the inject check both log them here, so
//! `mc debug replay` compares the same lines with its firings.

use crate::host::{Change, Edge, Notice};
use crate::problem::Problem;

/// Logs one edge. A change logs the old problem's recovery, then the new problem. `platform`
/// is the call's `call.platform` code; `None` leaves the field out.
pub fn log_edge(edge: &Edge, call_id: &str, platform: Option<&str>) {
    match edge.change {
        Change::Raised(problem) => log_raised(edge, problem, call_id, platform),
        Change::Changed { from, to } => {
            log_recovered(from, call_id, platform);
            log_raised(edge, to, call_id, platform);
        }
        Change::Cleared(problem) => log_recovered(problem, call_id, platform),
    }
}

/// Logs a notice that tells the user of a problem. Other notices log nothing.
pub fn log_notice(notice: Notice, call_id: &str) {
    let problem = match notice {
        Notice::CaptionTrouble(problem) | Notice::CaptureTrouble(problem) => problem,
        Notice::Recording
        | Notice::RecordingAgain
        | Notice::NoTranscript
        | Notice::CaptureBack(_) => return,
    };
    tracing::info!(
        event.name = "call.problem_noticed",
        call.id = call_id,
        error.type = problem.code(),
        mc.problem.layer = problem.layer().code(),
        "call problem notified"
    );
}

fn log_raised(edge: &Edge, problem: Problem, call_id: &str, platform: Option<&str>) {
    let heading = if problem.layer().is_capture() {
        "call capture problem"
    } else {
        "call captions problem"
    };
    tracing::warn!(
        event.name = "call.problem",
        call.id = call_id,
        call.platform = platform,
        error.type = problem.code(),
        mc.problem.layer = problem.layer().code(),
        mc.problem.detector = edge.detector.code(),
        mc.problem.evidence = edge.evidence.event,
        mc.problem.evidence_at = %edge.evidence.at,
        "{heading}: {}",
        problem.describe()
    );
}

fn log_recovered(problem: Problem, call_id: &str, platform: Option<&str>) {
    tracing::info!(
        event.name = "call.recovered",
        call.id = call_id,
        call.platform = platform,
        error.type = problem.code(),
        mc.problem.layer = problem.layer().code(),
        "call problem cleared"
    );
}
