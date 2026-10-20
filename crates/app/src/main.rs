//! Tray app with no Dock icon and no window. Receives mc-meet's call events and Slack huddle
//! captions, and runs the pipeline for each meeting. Starts at login once "Start at login" is on.
//! Started by a browser with an extension origin as its argument, it runs host mode instead.

// Release builds on Windows would otherwise open a console window next to the tray icon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser_calls;
mod calls;
mod finished_logs;
mod huddles;
mod launch;
mod meetings;
mod settings_window;
mod tray;
mod updates;

use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use pipeline::Pipeline;
use pipeline::settings::{AppDirs, SettingsStore};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt as _;

pub(crate) struct AppState {
    /// `None` when the settings or the job store failed to load. `pipeline_error` says why.
    pub pipeline: Option<Arc<Pipeline>>,
    pub pipeline_error: Option<String>,
    /// Exists without a pipeline, so the settings window can save a fix for settings that failed
    /// to load.
    pub settings: Arc<SettingsStore>,
    pub huddle_status: Mutex<huddles::HuddleStatus>,
    pub browser_calls: browser_calls::BrowserCalls,
    pub calls: calls::Calls,
    pub updates: updates::Updates,
}

impl AppState {
    pub fn huddle_status(&self) -> huddles::HuddleStatus {
        self.huddle_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_huddle_status(&self, status: huddles::HuddleStatus) {
        *self
            .huddle_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status;
    }
}

// The env var `AppDirs::locate` reads. A dev run with its own home is a separate instance.
const HOME_OVERRIDE_VAR: &str = "MEETING_CAPTURE_HOME";

fn main() -> ExitCode {
    // First, before anything can write to stdout: in host mode stdout carries frames to Chrome.
    let first_argument = std::env::args().nth(1);
    if native_host::is_host_mode(first_argument.as_deref()) {
        return run_host_mode(first_argument.as_deref().unwrap_or_default());
    }
    match run_tray_app() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("the tray app stopped: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_host_mode(origin: &str) -> ExitCode {
    let app_dirs = match AppDirs::locate() {
        Ok(app_dirs) => app_dirs,
        Err(error) => {
            eprintln!("host mode: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = init_host_logging(&app_dirs) {
        eprintln!("host mode: could not open its log file: {error}");
    }
    native_host::run(origin, app_dirs.browser_call_socket())
}

fn run_tray_app() -> tauri::Result<()> {
    let mut builder = tauri::Builder::default();
    if std::env::var_os(HOME_OVERRIDE_VAR).is_none() {
        // Registered first, so a second launch exits before it binds the socket.
        builder = builder.plugin(tauri_plugin_single_instance::init(|_, _, _| {
            tracing::info!("a second launch was refused; the tray app is already running");
        }));
    }
    let app = builder
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            settings_window::settings_load,
            settings_window::settings_save,
            settings_window::client_file_read,
            settings_window::google_account_add,
            settings_window::google_account_remove,
            settings_window::claude_code_check,
        ])
        .setup(|app| {
            // Tauri panics on a setup error, and a login item has no terminal, so the log is the
            // only place the cause can show.
            setup(app).inspect_err(|error| {
                tracing::error!(exception.message = %error, "app setup failed");
                eprintln!("app setup failed: {error}");
            })
        })
        .build(tauri::generate_context!())?;

    app.run(|app, event| match event {
        // Closing the last window must not quit a tray app; only the Quit item does.
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        // The Quit item, an AppleScript quit and logout all end here, so a downloaded update
        // installs at any quit.
        RunEvent::Exit => {
            updates::install_downloaded(app);
        }
        _ => {}
    });
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let app_dirs = AppDirs::locate()?;
    app_dirs.create()?;
    init_logging(&app_dirs)?;

    let settings_store = Arc::new(SettingsStore::new(app_dirs));
    let (pipeline, pipeline_error) = match Pipeline::open(Arc::clone(&settings_store)) {
        Ok(pipeline) => (Some(Arc::new(pipeline)), None),
        Err(error) => {
            tracing::error!(exception.message = %error, "could not start the pipeline");
            (None, Some(error.to_string()))
        }
    };
    let browser_calls = browser_calls::BrowserCalls::new(
        settings_store.app_dirs().browser_call_log_dir(),
        settings_store.meet(),
    );
    app.manage(AppState {
        pipeline: pipeline.clone(),
        pipeline_error,
        settings: settings_store,
        huddle_status: Mutex::new(huddles::HuddleStatus::Idle),
        browser_calls,
        calls: calls::Calls::new(),
        updates: updates::Updates::default(),
    });
    tray::create(app.handle())?;
    updates::start(app.handle());
    if let Some(pipeline) = pipeline {
        check_google_accounts(app.handle().clone());
        huddles::start(app.handle(), &pipeline);
        browser_calls::start(app.handle(), &pipeline);
        meetings::start(app.handle().clone(), pipeline);
    }
    Ok(())
}

/// Refreshes each account's token in the background and shows the outcome in the tray. At launch,
/// a revoked account shows "sign in again" before the next call needs it.
pub(crate) fn check_google_accounts(app: AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>().settings.google().check();
        tray::refresh(&app);
    });
}

/// Shows a system notification. A failure is only logged: the tray still shows the state.
pub(crate) fn notify(app: &AppHandle, heading: &str, body: &str) {
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

pub(crate) const LOG_FILE_NAME: &str = "app.log";
const HOST_LOG_FILE_NAME: &str = "native-host.log";

// JSON lines in the app's log folder. A tray app has no terminal to write to.
fn init_logging(app_dirs: &AppDirs) -> std::io::Result<()> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dirs.log_dir().join(LOG_FILE_NAME))?;
    tracing_subscriber::fmt()
        .json()
        .with_writer(Mutex::new(log_file))
        .init();
    // A panic on a worker thread would otherwise reach only stderr, which a login item discards.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(exception.message = %panic, "panic");
        default_hook(panic);
    }));
    Ok(())
}

// JSON lines in their own file: several hosts can run at once, one per Chrome profile, next to
// the tray app.
fn init_host_logging(app_dirs: &AppDirs) -> std::io::Result<()> {
    std::fs::create_dir_all(app_dirs.log_dir())?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dirs.log_dir().join(HOST_LOG_FILE_NAME))?;
    tracing_subscriber::fmt()
        .json()
        .with_writer(Mutex::new(log_file))
        .init();
    Ok(())
}
