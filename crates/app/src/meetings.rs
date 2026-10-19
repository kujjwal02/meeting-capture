//! Runs the pipeline for meetings, off the async runtime, and sweeps up unfinished ones.

use std::sync::Arc;
use std::time::Duration;

use jobs::{FailureCause, FailureKind, JobState, Meeting, MeetingId, RunOutcome};
use pipeline::{Pipeline, PipelineError};
use tauri::AppHandle;

// Picks up meetings a crash, a quit or a store error left unfinished. A dead process's lease
// expires after the jobs lease expiry, so a sweep soon after launch can still find one held.
const SWEEP_INTERVAL: Duration = Duration::from_mins(5);

pub(crate) fn start(app: AppHandle, pipeline: Arc<Pipeline>) {
    tauri::async_runtime::spawn(sweep_unfinished(app, pipeline));
}

// Blocks until the run ends, so callers keep it off the async runtime.
pub(crate) fn run(app: &AppHandle, pipeline: &Pipeline, id: MeetingId) {
    report_run(app, pipeline, id, &pipeline.run(id));
}

fn report_run(
    app: &AppHandle,
    pipeline: &Pipeline,
    id: MeetingId,
    outcome: &Result<RunOutcome, PipelineError>,
) {
    match outcome {
        Ok(RunOutcome::Finished(meeting)) => {
            tracing::info!(meeting.id = %id, meeting.state = meeting.state().label(), "run finished");
            notify_outcome(app, pipeline, meeting);
        }
        Ok(RunOutcome::AlreadyRunning) => {}
        // Step failures are logged and recorded by the pipeline; this covers store errors too.
        Err(error) => tracing::warn!(meeting.id = %id, exception.message = %error, "run stopped"),
    }
    crate::tray::refresh(app);
}

/// Runs the meeting again from its stored input and overwrites the note, edits included.
pub(crate) fn overwrite_edits(app: &AppHandle, pipeline: &Pipeline, id: MeetingId) {
    if let Err(error) = pipeline.reprocess(id, true) {
        tracing::warn!(meeting.id = %id, exception.message = %error, "run stopped");
    }
    crate::tray::refresh(app);
}

// One notification when the note is written, and one when its notes fail for good. A retry that
// writes the note again without notes stays quiet.
fn notify_outcome(app: &AppHandle, pipeline: &Pipeline, meeting: &Meeting) {
    let title = meeting.title.as_deref().unwrap_or("Untitled call");
    let (heading, body) = match meeting.state() {
        JobState::Written { has_notes: true } => ("Note written", format!("{title}, with notes")),
        JobState::Written { has_notes: false } => {
            ("Note written", format!("{title}, transcript only"))
        }
        JobState::ExtractionPending { retry: Some(retry) } if retry.number() == 1 => {
            let retry_at = retry.clock_time(&pipeline.settings().time_zone);
            (
                "Note written",
                format!("{title}, transcript only: the notes failed; next try at {retry_at}"),
            )
        }
        JobState::Failed {
            cause:
                FailureCause {
                    kind: FailureKind::Extraction,
                    ..
                },
        } => (
            "Notes failed",
            format!("{title} keeps the transcript only. The tray shows why."),
        ),
        JobState::Captured
        | JobState::Matched
        | JobState::Extracted
        | JobState::ExtractionPending { .. }
        | JobState::Failed {
            cause:
                FailureCause {
                    kind: FailureKind::Step,
                    ..
                },
        } => return,
    };
    crate::notify(app, heading, &body);
}

pub(crate) fn retry_failed(app: &AppHandle, pipeline: &Pipeline) {
    if let Err(error) = pipeline.retry_failed(|id, outcome| report_run(app, pipeline, id, outcome))
    {
        tracing::error!(exception.message = %error, "could not list failed meetings");
    }
}

/// Starts a sweep in the background, unless one is still running.
pub(crate) fn run_unfinished(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
    tauri::async_runtime::spawn_blocking(move || sweep(&app, &pipeline));
}

async fn sweep_unfinished(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        interval.tick().await;
        run_unfinished(&app, &pipeline);
    }
}

fn sweep(app: &AppHandle, pipeline: &Pipeline) {
    if let Err(error) = pipeline.sweep(|id, outcome| report_run(app, pipeline, id, outcome)) {
        tracing::error!(exception.message = %error, "could not list unfinished meetings");
    }
}
