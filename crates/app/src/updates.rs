//! Checks the GitHub releases for a newer version and downloads it in the background. The update
//! installs when the user picks it in the tray menu or quits, never in the middle of a meeting.
//! The tray can also start a check, which reports its outcome in a notification.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt as _};

use crate::{AppState, tray};

const CHECK_INTERVAL: Duration = Duration::from_hours(1);
// Without these, a connection that stalls after sleep or a network change would hold the check,
// and the tray's "Checking for updates…", until the app quits.
const CHECK_TIMEOUT: Duration = Duration::from_mins(1);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_NOTICE_CHARS: usize = 120;

struct DownloadedUpdate {
    update: Update,
    package: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Updates {
    downloaded: Mutex<Option<DownloadedUpdate>>,
    /// One check at a time, so the timer and the tray never download the same package twice.
    is_checking: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error(transparent)]
    Updater(#[from] tauri_plugin_updater::Error),
    #[error("the update {0} took too long")]
    TimedOut(&'static str),
}

/// Clears `is_checking` when the check ends, even by a panic, so the tray item comes back.
struct CheckingGuard<'a> {
    app: &'a AppHandle,
    updates: &'a Updates,
}

impl Drop for CheckingGuard<'_> {
    fn drop(&mut self) {
        self.updates.is_checking.store(false, Ordering::Release);
        tray::refresh(self.app);
    }
}

/// Who asked for a check. Only the user hears about its outcome.
#[derive(Clone, Copy)]
enum CheckTrigger {
    Timer,
    User,
}

impl Updates {
    /// The version that installs on the next restart, if one is downloaded.
    pub fn ready_version(&self) -> Option<String> {
        self.lock()
            .as_ref()
            .map(|downloaded| downloaded.update.version.clone())
    }

    pub fn is_checking(&self) -> bool {
        self.is_checking.load(Ordering::Acquire)
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

/// Checks now, from the tray. A check already running makes this a no-op.
pub(crate) fn check_now(app: &AppHandle) {
    tauri::async_runtime::spawn(check(app.clone(), CheckTrigger::User));
}

async fn check_periodically(app: AppHandle) {
    loop {
        check(app.clone(), CheckTrigger::Timer).await;
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

async fn check(app: AppHandle, trigger: CheckTrigger) {
    let updates = &app.state::<AppState>().updates;
    if updates.is_checking.swap(true, Ordering::AcqRel) {
        // A menu opened before the running check began still offers the item.
        if let CheckTrigger::User = trigger {
            crate::notify(&app, "Checking for updates", "A check is already running.");
        }
        return;
    }
    let outcome = {
        let _guard = CheckingGuard { app: &app, updates };
        tray::refresh(&app);
        download_newer(&app).await
    };
    if let Err(error) = &outcome {
        tracing::warn!(exception.message = %error, "update check failed");
    }
    match trigger {
        CheckTrigger::Timer => {}
        CheckTrigger::User => match outcome {
            Ok(Some(version)) => crate::notify(
                &app,
                "Update ready",
                &format!("Pick \"Install update {version} and restart\" in the tray."),
            ),
            Ok(None) => crate::notify(
                &app,
                "No update",
                &format!(
                    "Meeting Capture {} is the latest version.",
                    env!("CARGO_PKG_VERSION")
                ),
            ),
            Err(error) => crate::notify(
                &app,
                "Could not check for updates",
                &tray::shorten(&error.to_string(), MAX_NOTICE_CHARS),
            ),
        },
    }
}

/// Returns the version ready to install, or `None` when this is the latest.
async fn download_newer(app: &AppHandle) -> Result<Option<String>, CheckError> {
    let checked = tokio::time::timeout(CHECK_TIMEOUT, app.updater()?.check())
        .await
        .map_err(|_| CheckError::TimedOut("check"))?;
    let Some(update) = checked? else {
        tracing::debug!("no newer version");
        return Ok(None);
    };
    let updates = &app.state::<AppState>().updates;
    let version = update.version.clone();
    if updates.ready_version().as_deref() == Some(version.as_str()) {
        return Ok(Some(version));
    }
    tracing::info!(update.version = version, "downloading update");
    let package = tokio::time::timeout(DOWNLOAD_TIMEOUT, update.download(|_, _| {}, || {}))
        .await
        .map_err(|_| CheckError::TimedOut("download"))??;
    tracing::info!(update.version = version, "update ready to install");
    *updates.lock() = Some(DownloadedUpdate { update, package });
    Ok(Some(version))
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
