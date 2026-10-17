//! Tray app with no Dock icon and no window. Receives TranscripTonic's webhook and runs the
//! pipeline for each meeting. Starts at login once "Start at login" is on.

// Release builds on Windows would otherwise open a console window next to the tray icon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod huddles;
mod launch;
mod meetings;
mod settings_window;
mod tray;

use std::sync::{Arc, Mutex, PoisonError, RwLock};

use jobs::JobStore;
use pipeline::Pipeline;
use pipeline::settings::{AppDirs, Settings, SlackSettings};
use tauri::{AppHandle, Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;

#[derive(Debug, Clone)]
pub(crate) enum Listener {
    Starting,
    Listening { port: u16 },
    Stopped { reason: String },
}

pub(crate) struct AppState {
    pub app_dirs: AppDirs,
    /// `None` when the settings or the job store failed to load. The listener then says why.
    pub pipeline: Option<Arc<Pipeline>>,
    pub listener: Mutex<Listener>,
    /// Held across each read, edit and write of the settings file, so two edits never drop each
    /// other's change.
    pub settings_file_lock: Mutex<()>,
    /// The settings window replaces this, and the huddle watcher reads it before each step.
    pub slack: RwLock<SlackSettings>,
    pub huddle_status: Mutex<huddles::HuddleStatus>,
}

impl AppState {
    pub fn listener(&self) -> Listener {
        self.listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn slack(&self) -> SlackSettings {
        *self.slack.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_slack(&self, slack: SlackSettings) {
        *self.slack.write().unwrap_or_else(PoisonError::into_inner) = slack;
    }

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

/// Records the listener state and shows it in the tray.
pub(crate) fn set_listener(app: &AppHandle, listener: Listener) {
    match &listener {
        Listener::Starting => {}
        Listener::Listening { port } => tracing::info!(server.port = port, "listening"),
        Listener::Stopped { reason } => {
            tracing::error!(exception.message = reason, "not listening");
        }
    }
    *app.state::<AppState>()
        .listener
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = listener;
    tray::refresh(app);
}

fn main() -> tauri::Result<()> {
    let app = tauri::Builder::default()
        // Registered first, so a second launch exits before it binds the port.
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {
            tracing::info!("a second launch was refused; the tray app is already running");
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
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

    app.run(|_, event| {
        // Closing the last window must not quit a tray app; only the Quit item does.
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let app_dirs = AppDirs::locate()?;
    app_dirs.create()?;
    init_logging(&app_dirs)?;

    let loaded = Settings::load(&app_dirs)
        .map_err(|error| error.to_string())
        .and_then(|settings| {
            let store =
                JobStore::open(&app_dirs.job_store_file()).map_err(|error| error.to_string())?;
            Ok((settings, store))
        });
    let (pipeline, listener) = match loaded {
        Ok((settings, store)) => {
            let port = settings.receiver_port;
            let pipeline = Arc::new(Pipeline::new(app_dirs.clone(), settings, store));
            (Some((pipeline, port)), Listener::Starting)
        }
        Err(reason) => (None, Listener::Stopped { reason }),
    };
    let slack = pipeline
        .as_ref()
        .map(|(pipeline, _)| pipeline.settings().slack)
        .unwrap_or_default();
    app.manage(AppState {
        app_dirs,
        pipeline: pipeline.as_ref().map(|(pipeline, _)| Arc::clone(pipeline)),
        listener: Mutex::new(listener.clone()),
        settings_file_lock: Mutex::new(()),
        slack: RwLock::new(slack),
        huddle_status: Mutex::new(huddles::HuddleStatus::Idle),
    });
    tray::create(app.handle())?;
    match pipeline {
        Some((pipeline, port)) => {
            check_google_accounts(app.handle().clone(), Arc::clone(&pipeline));
            huddles::start(app.handle(), &pipeline);
            meetings::start(app.handle().clone(), pipeline, port);
        }
        None => set_listener(app.handle(), listener),
    }
    Ok(())
}

/// Refreshes each account's token in the background and shows the outcome in the tray. At launch,
/// a revoked account shows "sign in again" before the next call needs it.
pub(crate) fn check_google_accounts(app: AppHandle, pipeline: Arc<Pipeline>) {
    tauri::async_runtime::spawn_blocking(move || {
        pipeline.google().check();
        tray::refresh(&app);
    });
}

pub(crate) const LOG_FILE_NAME: &str = "app.log";

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
