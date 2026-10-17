//! Runs the Slack huddle watcher, shows what it reports, and turns each finished huddle log into
//! a meeting.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use huddle_captions::{HuddleHealth, Notice, WatchPaths, WatchSettings};
use pipeline::{InputError, Intake, IntakeError, Pipeline};
use tauri::{AppHandle, Manager as _};
use tauri_plugin_notification::NotificationExt as _;

use crate::AppState;

/// What the tray shows for Slack huddles.
#[derive(Debug, Clone)]
pub(crate) enum HuddleStatus {
    Idle,
    PermissionNeeded,
    Reading {
        channel: String,
        health: Option<HuddleHealth>,
    },
}

pub(crate) fn start(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let state = app.state::<AppState>();
    let paths = WatchPaths {
        log_dir: state.settings.app_dirs().huddle_log_dir(),
        layout_override_dir: state.settings.app_dirs().slack_layout_dir(),
    };
    // Before the watcher starts, so it cannot touch a log the watcher is writing.
    match huddle_captions::finish_open_logs(&paths.log_dir) {
        Ok(logs) => {
            let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
            tauri::async_runtime::spawn_blocking(move || record_left_over(&app, &pipeline, logs));
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not read the huddle log folder");
        }
    }
    let settings_app = app.clone();
    let settings = move || {
        let slack = settings_app.state::<AppState>().settings.slack();
        WatchSettings {
            read_captions: slack.read_captions,
            auto_start_captions: slack.auto_start_captions,
        }
    };
    let mut reporter = Reporter {
        app: app.clone(),
        pipeline: Arc::clone(pipeline),
        notified: BTreeSet::new(),
    };
    if let Err(error) =
        huddle_captions::spawn(paths, settings, move |notice| reporter.report(notice))
    {
        tracing::error!(exception.message = %error, "could not start the Slack huddle watcher");
    }
}

// Logs the last run did not turn into meetings: the app quit or crashed during the huddle, or
// before its note.
fn record_left_over(app: &AppHandle, pipeline: &Pipeline, logs: Vec<PathBuf>) {
    for log_path in logs {
        match pipeline.store().has_input(&log_path) {
            Ok(true) => {}
            Ok(false) => record_and_run(app, pipeline, &log_path),
            Err(error) => {
                tracing::error!(exception.message = %error, "could not check a huddle log");
            }
        }
    }
}

struct Reporter {
    app: AppHandle,
    pipeline: Arc<Pipeline>,
    /// One notification per huddle and state, even when the state flips back and forth.
    notified: BTreeSet<(PathBuf, HuddleHealth)>,
}

impl Reporter {
    fn report(&mut self, notice: Notice) {
        match notice {
            Notice::PermissionMissing => {
                self.set_status(HuddleStatus::PermissionNeeded);
                notify(
                    &self.app,
                    "Slack huddles need a permission",
                    "Allow Meeting Capture under Accessibility in System Settings to read huddle captions.",
                );
            }
            Notice::PermissionGranted => self.set_status(HuddleStatus::Idle),
            Notice::HuddleStarted { channel, .. } => {
                self.set_status(HuddleStatus::Reading {
                    channel,
                    health: None,
                });
            }
            Notice::Health {
                log_path,
                channel,
                state,
                reason,
            } => {
                self.set_status(HuddleStatus::Reading {
                    channel: channel.clone(),
                    health: Some(state),
                });
                let heading = match state {
                    HuddleHealth::Healthy | HuddleHealth::Untested => return,
                    HuddleHealth::Degraded => "Slack huddle captions: partly read",
                    HuddleHealth::CaptionsOff => "Slack huddle captions are off",
                    HuddleHealth::Broken => "Can't read Slack huddle captions",
                };
                if self.notified.insert((log_path, state)) {
                    notify(&self.app, heading, &format!("{channel}: {reason}"));
                }
            }
            Notice::HuddleEnded { log_path, .. } => {
                self.set_status(HuddleStatus::Idle);
                self.notified.retain(|(path, _)| *path != log_path);
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
            }
            Notice::Problem { message } => notify(&self.app, "Slack huddle captions", &message),
        }
    }

    fn set_status(&self, status: HuddleStatus) {
        self.app.state::<AppState>().set_huddle_status(status);
        crate::tray::refresh(&self.app);
    }
}

fn record_and_run(app: &AppHandle, pipeline: &Pipeline, log_path: &Path) {
    let _span = tracing::info_span!("huddle.record", file.path = %log_path.display()).entered();
    // Read for each huddle, so a new setting applies to the next one without a restart.
    let min_huddle_seconds = app.state::<AppState>().settings.slack().min_huddle_seconds;
    let taken_in = pipeline.take_in_huddle_log_unless_short(log_path, min_huddle_seconds);
    match taken_in {
        Ok(Intake::Recorded(id)) => {
            crate::tray::refresh(app);
            crate::meetings::run(app, pipeline, id);
        }
        // Its log stays, and `mc ingest` can still write its note.
        Ok(Intake::TooShort) => {}
        Err(IntakeError::Input(error)) => {
            tracing::error!(exception.message = %error, "could not read a huddle log");
            // The banner has little room, and the span already names the file.
            let cause = match &error {
                InputError::Read { source, .. } => source.to_string(),
                InputError::ParseTranscriptonic(_) | InputError::ParseSlackHuddle(_) => {
                    error.to_string()
                }
            };
            notify(app, "Slack huddle not recorded", &cause);
            set_aside(log_path);
        }
        Err(error @ (IntakeError::Copy { .. } | IntakeError::Store(_))) => {
            tracing::error!(exception.message = %error, "could not record a huddle");
            notify(app, "Slack huddle not recorded", &error.to_string());
        }
    }
}

fn notify(app: &AppHandle, heading: &str, body: &str) {
    if let Err(error) = app
        .notification()
        .builder()
        .title(heading)
        .body(body)
        .show()
    {
        tracing::warn!(exception.message = %error, "could not show the notification");
    }
}

// Renames a log that cannot be read out of the `.jsonl` set, so the next launch does not report it
// again. The file stays for a look by hand.
fn set_aside(log_path: &Path) {
    let unreadable = log_path.with_extension("jsonl.unreadable");
    if let Err(error) = std::fs::rename(log_path, &unreadable) {
        tracing::warn!(
            file.path = %log_path.display(),
            exception.message = %error,
            "could not set aside an unreadable huddle log"
        );
    }
}
