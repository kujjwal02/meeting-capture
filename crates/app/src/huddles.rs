//! Runs the Slack huddle watcher, feeds what it reports to the call status board, and turns
//! each finished huddle log into a meeting.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use capture_protocol::WindowHint;
use huddle_captions::{HuddleHealth, HuddleWindow, Notice, WatchPaths, WatchSettings};
use pipeline::Pipeline;
use tauri::{AppHandle, Manager as _};

use crate::AppState;
use crate::call_status::{Moment, Observation};
use crate::calls::{CallEndReason, CallPlatform};
use crate::finished_logs::{self, LogSource};
use crate::notify;

const SLACK_BUNDLE_ID: &str = "com.tinyspeck.slackmacgap";

pub(crate) fn start(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let state = app.state::<AppState>();
    let paths = WatchPaths {
        log_dir: state.settings.app_dirs().huddle_log_dir(),
        layout_override_dir: state.settings.app_dirs().slack_layout_dir(),
    };
    // Before the watcher starts, so it cannot touch a log the watcher is writing.
    match huddle_captions::finish_open_logs(&paths.log_dir) {
        Ok(logs) => {
            state
                .calls
                .end_at_launch(&logs.ended_paths, CallPlatform::SlackHuddle);
            let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
            tauri::async_runtime::spawn_blocking(move || {
                let min_call_seconds = min_huddle_seconds(&app);
                finished_logs::record_left_over(
                    &app,
                    &pipeline,
                    LogSource::SlackHuddle,
                    logs.log_paths,
                    min_call_seconds,
                );
            });
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not read the huddle log folder");
        }
    }
    let settings_app = app.clone();
    let settings = move || {
        let state = settings_app.state::<AppState>();
        let slack = state.settings.slack();
        WatchSettings {
            read_captions: slack.read_captions,
            auto_start_captions: slack.auto_start_captions,
            stopped_logs: state.calls.status().stopped_call_ids(),
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

struct Reporter {
    app: AppHandle,
    pipeline: Arc<Pipeline>,
    /// One notification per huddle and state, even when the state flips back and forth.
    notified: BTreeSet<(PathBuf, HuddleHealth)>,
}

/// The hint video records the huddle window by. `None` while the watcher has not seen huddle
/// content in the window, or could not read its id: a title alone could pick a conversation
/// window, which shows private messages. Video starts when `HuddleWindowFound` brings the id.
fn window_hint(window: HuddleWindow) -> Option<WindowHint> {
    let Some(window_id) = window.window_id else {
        tracing::info!("the huddle window is not confirmed yet; video waits for its id");
        return None;
    };
    Some(WindowHint {
        bundle_id: SLACK_BUNDLE_ID.to_owned(),
        title: Some(window.title),
        pid: Some(window.pid),
        window_id,
    })
}

impl Reporter {
    fn report(&mut self, notice: Notice) {
        match notice {
            Notice::PermissionMissing => {
                self.calls().status().set_huddle_permission_missing(true);
                crate::tray::refresh(&self.app);
                notify(
                    &self.app,
                    "Slack huddles need a permission",
                    "Allow Meeting Capture under Accessibility in System Settings to read huddle captions.",
                );
            }
            Notice::PermissionGranted => {
                self.calls().status().set_huddle_permission_missing(false);
                crate::tray::refresh(&self.app);
            }
            Notice::HuddleStarted {
                log_path,
                channel,
                window,
            } => {
                // Each huddle gets a new log, so none starts stopped.
                self.calls().open(
                    &log_path,
                    CallPlatform::SlackHuddle,
                    Some(channel),
                    window_hint(window),
                    false,
                );
                crate::tray::refresh(&self.app);
            }
            Notice::HuddleWindowFound { log_path, window } => {
                if let Some(hint) = window_hint(window) {
                    self.calls().set_window_hint(&log_path, hint);
                }
            }
            Notice::CaptionsRead {
                log_path,
                caption_lines,
            } => {
                self.calls().status().observe(
                    &log_path,
                    Observation::HuddleCaptionLines(caption_lines),
                    Moment::now(),
                );
            }
            Notice::Health {
                log_path,
                channel,
                state,
                reason,
            } => {
                self.calls().status().observe(
                    &log_path,
                    Observation::HuddleHealth(state),
                    Moment::now(),
                );
                crate::tray::refresh(&self.app);
                // Captions off and an unreadable window stop the recording, so the status board
                // announces them with the other "Not recording" notices.
                let heading = match state {
                    HuddleHealth::Healthy
                    | HuddleHealth::Untested
                    | HuddleHealth::CaptionsOff
                    | HuddleHealth::Broken => return,
                    HuddleHealth::Degraded => "Slack huddle captions: partly read",
                };
                if self.notified.insert((log_path, state)) {
                    notify(&self.app, heading, &format!("{channel}: {reason}"));
                }
            }
            Notice::HuddleEnded { log_path, .. } => {
                // The watcher gives no reason; the window closing is the usual one.
                self.calls().end(&log_path, CallEndReason::WindowClosed);
                crate::tray::refresh(&self.app);
                self.notified.retain(|(path, _)| *path != log_path);
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
            }
            Notice::Problem { message } => notify(&self.app, "Slack huddle captions", &message),
        }
    }

    fn calls(&self) -> &crate::calls::Calls {
        &self.app.state::<AppState>().inner().calls
    }
}

fn record_and_run(app: &AppHandle, pipeline: &Pipeline, log_path: &Path) {
    finished_logs::record_and_run(
        app,
        pipeline,
        LogSource::SlackHuddle,
        log_path,
        min_huddle_seconds(app),
    );
}

// Read for each huddle, so a new setting applies to the next one without a restart.
fn min_huddle_seconds(app: &AppHandle) -> u32 {
    app.state::<AppState>().settings.slack().min_huddle_seconds
}
