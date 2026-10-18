//! Checks the GitHub releases for a newer version and downloads it in the background. The update
//! installs when the user picks it in the tray menu or quits, never in the middle of a meeting.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt as _};

use crate::{AppState, tray};

const CHECK_INTERVAL: Duration = Duration::from_hours(6);

struct DownloadedUpdate {
    update: Update,
    package: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Updates {
    downloaded: Mutex<Option<DownloadedUpdate>>,
}

impl Updates {
    /// The version that installs on the next restart, if one is downloaded.
    pub fn ready_version(&self) -> Option<String> {
        self.lock()
            .as_ref()
            .map(|downloaded| downloaded.update.version.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<DownloadedUpdate>> {
        self.downloaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) fn start(app: &AppHandle) {
    // A dev build runs from target/, not from an app bundle, so there is nothing to replace.
    if cfg!(debug_assertions) {
        tracing::info!("update checks are off in dev builds");
        return;
    }
    tauri::async_runtime::spawn(check_periodically(app.clone()));
}

async fn check_periodically(app: AppHandle) {
    loop {
        if let Err(error) = download_newer(&app).await {
            tracing::warn!(exception.message = %error, "update check failed");
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

async fn download_newer(app: &AppHandle) -> tauri_plugin_updater::Result<()> {
    let Some(update) = app.updater()?.check().await? else {
        tracing::debug!("no newer version");
        return Ok(());
    };
    let updates = &app.state::<AppState>().updates;
    if updates.ready_version().as_deref() == Some(update.version.as_str()) {
        return Ok(());
    }
    tracing::info!(update.version = update.version, "downloading update");
    let package = update.download(|_, _| {}, || {}).await?;
    tracing::info!(update.version = update.version, "update ready to install");
    *updates.lock() = Some(DownloadedUpdate { update, package });
    tray::refresh(app);
    Ok(())
}

/// Replaces the app bundle with the downloaded update, if there is one. Returns whether it did.
/// The running process keeps its old code until it restarts.
pub(crate) fn install_downloaded(app: &AppHandle) -> bool {
    let Some(downloaded) = app.state::<AppState>().updates.lock().take() else {
        return false;
    };
    let version = &downloaded.update.version;
    match downloaded.update.install(&downloaded.package) {
        Ok(()) => {
            tracing::info!(update.version = version, "update installed");
            true
        }
        Err(error) => {
            tracing::error!(update.version = version, exception.message = %error, "update install failed");
            false
        }
    }
}

/// Installs the downloaded update and relaunches. Call it off the main thread: a restart from
/// there skips the exit events that release the single-instance lock.
pub(crate) fn install_and_restart(app: &AppHandle) {
    if install_downloaded(app) {
        app.restart();
    }
    tray::refresh(app);
}
