//! Turns a finished call log, from a Meet call or a Slack huddle, into a meeting, and tells the
//! user when it cannot.

use std::path::{Path, PathBuf};

use jobs::InputKind;
use pipeline::{InputError, Intake, IntakeError, Pipeline};
use tauri::AppHandle;

/// What wrote a call log.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LogSource {
    MeetCall,
    SlackHuddle,
}

impl LogSource {
    fn input_kind(self) -> InputKind {
        match self {
            LogSource::MeetCall => InputKind::BrowserCallLog,
            LogSource::SlackHuddle => InputKind::SlackHuddleLog,
        }
    }

    fn not_recorded_heading(self) -> &'static str {
        match self {
            LogSource::MeetCall => "Meet call not recorded",
            LogSource::SlackHuddle => "Slack huddle not recorded",
        }
    }

    // For log messages.
    fn call_noun(self) -> &'static str {
        match self {
            LogSource::MeetCall => "browser call",
            LogSource::SlackHuddle => "huddle",
        }
    }
}

/// Records each log the last run did not turn into a meeting: the app quit or crashed during
/// the call, or before its note. A call shorter than `min_call_seconds` is skipped.
pub(crate) fn record_left_over(
    app: &AppHandle,
    pipeline: &Pipeline,
    source: LogSource,
    log_paths: Vec<PathBuf>,
    min_call_seconds: u32,
) {
    for log_path in log_paths {
        match pipeline.store().has_input(&log_path) {
            Ok(true) => {}
            Ok(false) => record_and_run(app, pipeline, source, &log_path, min_call_seconds),
            Err(error) => {
                tracing::error!(
                    exception.message = %error,
                    "could not check a {} log",
                    source.call_noun()
                );
            }
        }
    }
}

/// Records the finished log at `log_path` as a meeting and runs it. A call shorter than
/// `min_call_seconds` is skipped.
pub(crate) fn record_and_run(
    app: &AppHandle,
    pipeline: &Pipeline,
    source: LogSource,
    log_path: &Path,
    min_call_seconds: u32,
) {
    // Span names are fixed at compile time.
    let span = match source {
        LogSource::MeetCall => {
            tracing::info_span!("browser_call.record", file.path = %log_path.display())
        }
        LogSource::SlackHuddle => {
            tracing::info_span!("huddle.record", file.path = %log_path.display())
        }
    };
    let _entered = span.enter();
    let taken_in = pipeline.take_in_unless_short(source.input_kind(), log_path, min_call_seconds);
    match taken_in {
        Ok(Intake::Recorded(id)) => {
            crate::tray::refresh(app);
            crate::meetings::run(app, pipeline, id);
        }
        // Its log stays, and `mc ingest` can still write its note.
        Ok(Intake::TooShort) => {}
        Err(IntakeError::Input(error)) => {
            tracing::error!(
                exception.message = %error,
                "could not read a {} log",
                source.call_noun()
            );
            // The banner has little room, and the span already names the file.
            let cause = match &error {
                InputError::Read {
                    source: read_error, ..
                } => read_error.to_string(),
                InputError::ParseTranscriptonic(_)
                | InputError::ParseSlackHuddle(_)
                | InputError::ParseBrowserCall(_) => error.to_string(),
            };
            crate::notify(app, source.not_recorded_heading(), &cause);
            set_aside(log_path);
        }
        Err(error @ (IntakeError::Copy { .. } | IntakeError::Store(_))) => {
            tracing::error!(
                exception.message = %error,
                "could not record a {}",
                source.call_noun()
            );
            crate::notify(app, source.not_recorded_heading(), &error.to_string());
        }
    }
}

// Renames a log that cannot be read out of the `.jsonl` set, so the next launch does not report
// it again, and a browser call session cannot rejoin or reopen it. The file stays for a look by
// hand.
fn set_aside(log_path: &Path) {
    let unreadable = log_path.with_extension("jsonl.unreadable");
    if let Err(error) = std::fs::rename(log_path, &unreadable) {
        tracing::warn!(
            file.path = %log_path.display(),
            exception.message = %error,
            "could not set aside an unreadable log"
        );
    }
}
