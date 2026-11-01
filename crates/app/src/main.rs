//! Tray app with no Dock icon and no window. Receives mc-meet's call events and Slack huddle
//! captions, records each call's audio through the `mc-capture` sidecar, and runs the pipeline
//! for each meeting. Starts at login once "Start at login" is on.
//! Started by a browser with an extension origin as its argument, it runs host mode instead.

// Release builds on Windows would otherwise open a console window next to the tray icon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser_calls;
mod call_capture;
mod call_debug;
mod call_metrics;
mod call_status;
mod calls;
mod debug_mode;
mod finished_logs;
mod host_metrics;
mod huddles;
mod launch;
mod meet_extension;
mod meet_telemetry;
mod meetings;
mod settings_window;
mod tray;
mod updates;

use std::process::ExitCode;
use std::sync::Arc;

use pipeline::Pipeline;
use pipeline::settings::{AppDirs, SettingsStore};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt as _;
use telemetry::debug_mode::{DebugMode, DebugModeFollower};

pub(crate) struct AppState {
    /// `None` when the settings or the job store failed to load. `pipeline_error` says why.
    pub pipeline: Option<Arc<Pipeline>>,
    pub pipeline_error: Option<String>,
    /// Exists without a pipeline, so the settings window can save a fix for settings that failed
    /// to load.
    pub settings: Arc<SettingsStore>,
    pub browser_calls: browser_calls::BrowserCalls,
    pub calls: calls::Calls,
    pub call_capture: call_capture::CallCapture,
    pub call_debug: call_debug::CallDebug,
    pub debug_mode: debug_mode::AppDebugMode,
    pub updates: updates::Updates,
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
    let file_level = match init_host_logging(&app_dirs) {
        Ok(file_level) => Some(file_level),
        Err(error) => {
            eprintln!("host mode: could not open its log file: {error}");
            None
        }
    };
    let socket = app_dirs.browser_call_socket().to_path_buf();
    let Some(file_level) = file_level else {
        return native_host::run(origin, &socket, || {});
    };
    // The host hears no settings change, so it rereads the file as it relays.
    let settings = SettingsStore::new(app_dirs);
    let mut debug_mode = DebugModeFollower::new(file_level, move || {
        match settings.read().and_then(|file| file.debug.mode()) {
            Ok(mode) => mode,
            Err(error) => {
                tracing::warn!(exception.message = %error, "could not read debug mode; it stays off");
                DebugMode::Off
            }
        }
    });
    debug_mode.follow();
    native_host::run(origin, &socket, move || debug_mode.follow())
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
    let file_level = init_logging(&app_dirs)?;
    remove_plain_text_logs(&app_dirs);

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
    let call_debug = call_debug::CallDebug::new(settings_store.app_dirs());
    app.manage(AppState {
        pipeline: pipeline.clone(),
        pipeline_error,
        settings: settings_store,
        browser_calls,
        calls: calls::Calls::new(),
        call_capture: call_capture::CallCapture::new(),
        call_debug,
        debug_mode: debug_mode::AppDebugMode::new(file_level),
        updates: updates::Updates::default(),
    });
    // Before the tray and the helpers, so they start in the stored mode.
    debug_mode::start(app.handle());
    tray::create(app.handle())?;
    updates::start(app.handle());
    meet_extension::start(app.handle());
    call_status::start(app.handle());
    call_debug::start(app.handle());
    host_metrics::start(app.handle());
    // Before the call sources, so it hears every call from the first.
    call_capture::start(app.handle());
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

// JSON lines in the app's log folder. A tray app has no terminal to write to.
fn init_logging(app_dirs: &AppDirs) -> Result<telemetry::FileLevelHandle, telemetry::InitError> {
    telemetry::init(telemetry::TelemetryConfig {
        service: telemetry::Service::App,
        service_version: env!("CARGO_PKG_VERSION"),
        log_sink: telemetry::LogSink::DailyFiles {
            dir: app_dirs.log_dir(),
        },
        panic_dump_dir: Some(app_dirs.call_debug_dir()),
    })
}

/// The log files of versions before the daily JSON files. Nothing writes them any more.
const PLAIN_TEXT_LOG_FILE_NAMES: &[&str] = &[
    "app.log",
    "native-host.log",
    "mc-capture.log",
    "mc-capture.log.1",
];

// Runs at every launch: once the files are gone, it costs four failed unlinks.
fn remove_plain_text_logs(app_dirs: &AppDirs) {
    let log_dir = app_dirs.log_dir();
    for file_name in PLAIN_TEXT_LOG_FILE_NAMES {
        let path = log_dir.join(file_name);
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(file.path = %path.display(), "removed an old log file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                file.path = %path.display(),
                exception.message = %error,
                "could not remove an old log file"
            ),
        }
    }
}

// Its own service and so its own files: several hosts can run at once, one per Chrome profile,
// next to the tray app.
fn init_host_logging(
    app_dirs: &AppDirs,
) -> Result<telemetry::FileLevelHandle, telemetry::InitError> {
    telemetry::init(telemetry::TelemetryConfig {
        service: telemetry::Service::NativeHost,
        service_version: env!("CARGO_PKG_VERSION"),
        log_sink: telemetry::LogSink::DailyFiles {
            dir: app_dirs.log_dir(),
        },
        panic_dump_dir: Some(app_dirs.call_debug_dir()),
    })
}
