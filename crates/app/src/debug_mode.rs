//! Debug mode in the tray app. The tray turns it on for 24 hours; it ends at that time, or when
//! the tray turns it off. The settings file holds it, so a restart keeps it and native hosts read
//! it. The app rereads it every 30 s, so a hand edit applies too. The app's log file follows at
//! once, and the app tells each audio helper and each mc-meet port.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use pipeline::settings::SettingsError;
use tauri::{AppHandle, Manager as _};
use telemetry::debug_mode::{DebugMode, DebugModeSwitch};
use tokio::sync::watch;

use crate::AppState;

// Under the 60 s the plan allows, so a late tick still ends debug mode in time.
const EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct AppDebugMode {
    switch: DebugModeSwitch,
    /// The mode in effect. Audio helpers and mc-meet ports subscribe.
    mode: watch::Sender<DebugMode>,
    /// Held from reading the mode to applying the new one, so a tick that read the file before
    /// a toggle stored cannot undo the toggle.
    change: Mutex<()>,
    /// The setting failed to read on the last tick, so the next failure logs nothing.
    has_read_failed: AtomicBool,
}

impl AppDebugMode {
    pub fn new(file_level: telemetry::FileLevelHandle) -> Self {
        Self {
            switch: DebugModeSwitch::new(file_level),
            mode: watch::Sender::new(DebugMode::Off),
            change: Mutex::new(()),
            has_read_failed: AtomicBool::new(false),
        }
    }

    pub fn mode(&self) -> DebugMode {
        *self.mode.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<DebugMode> {
        self.mode.subscribe()
    }

    fn lock_change(&self) -> MutexGuard<'_, ()> {
        self.change.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Once per run of failures: the tick would log the same error every 30 s.
    fn note_read_failed(&self, error: &SettingsError) {
        if !self.has_read_failed.swap(true, Ordering::Relaxed) {
            tracing::error!(exception.message = %error, "could not read debug mode; it stays as it is");
        }
    }

    fn apply(&self, mode: DebugMode) {
        let in_effect = self.switch.apply(mode, Timestamp::now());
        self.mode.send_if_modified(|current| {
            let is_changed = *current != in_effect;
            *current = in_effect;
            is_changed
        });
    }
}

/// Takes the stored mode, then follows the setting.
pub(crate) fn start(app: &AppHandle) {
    follow_stored(app);
    tauri::async_runtime::spawn(follow_setting(app.clone()));
}

/// The tray's "Debug mode (24 h)": on for 24 hours from now, or off.
pub(crate) fn toggle(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _changing = state.debug_mode.lock_change();
        let now = Timestamp::now();
        let mode = if state.debug_mode.mode().is_on(now) {
            DebugMode::Off
        } else {
            DebugMode::turned_on_at(now)
        };
        if store(&app, mode) {
            state.debug_mode.apply(mode);
        } else {
            crate::notify(
                &app,
                "Debug mode not changed",
                "The settings file could not be written. The log file says why.",
            );
        }
        crate::tray::refresh(&app);
    });
}

/// Rereads the setting each tick, so a hand edit reaches the helpers and mc-meet, and debug mode
/// ends on time.
async fn follow_setting(app: AppHandle) {
    let mut check = tokio::time::interval(EXPIRY_CHECK_INTERVAL);
    loop {
        check.tick().await;
        let app = app.clone();
        let followed = tauri::async_runtime::spawn_blocking(move || follow_stored(&app)).await;
        if let Err(error) = followed {
            tracing::error!(exception.message = %error, "could not follow the debug mode setting");
        }
    }
}

/// Applies the stored mode when it differs from the one in effect. A stored end that has passed
/// is removed first, and one more than 24 hours ahead is stored as 24 hours from now, so every
/// reader of the file sees the mode in effect here.
fn follow_stored(app: &AppHandle) {
    let state = app.state::<AppState>();
    let _changing = state.debug_mode.lock_change();
    let read = state
        .settings
        .read()
        .and_then(|file| file.debug.mode().map(|mode| (file, mode)));
    let (file, stored) = match read {
        Ok(read) => read,
        Err(error) => {
            state.debug_mode.note_read_failed(&error);
            return;
        }
    };
    state
        .debug_mode
        .has_read_failed
        .store(false, Ordering::Relaxed);
    let in_effect = stored.at(Timestamp::now());
    if let DebugMode::On { until } = stored
        && in_effect == DebugMode::Off
    {
        tracing::info!(mc.debug_mode.until = %until, "debug mode reached its end");
    }
    let mut normal = file.debug.clone();
    normal.set_mode(in_effect);
    // Stored before it applies, as the toggle does: a host that rereads the file then agrees.
    if normal != file.debug {
        store(app, in_effect);
    }
    if in_effect != state.debug_mode.mode() {
        state.debug_mode.apply(in_effect);
        crate::tray::refresh(app);
    }
}

/// Writes the mode to the settings file. Returns whether it did.
fn store(app: &AppHandle, mode: DebugMode) -> bool {
    let edited = app.state::<AppState>().settings.edit(|file| {
        file.debug.set_mode(mode);
        Ok(())
    });
    match edited {
        // Only the debug table changed, so no token or key needs revoking.
        Ok(_unrevoked) => true,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not write the debug mode setting");
            false
        }
    }
}
