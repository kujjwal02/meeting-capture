//! Keeps the mc-meet folder that Chrome loads unpacked in step with the app. The app bundles the
//! extension and copies it into its data folder, stamped with the app's version. Chrome installs
//! extensions from outside the Web Store only unpacked, so the user loads that folder once. After
//! an app update, mc-meet sees the folder's new version in `welcome` and reloads itself from it.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager as _};
use tauri_plugin_opener::OpenerExt as _;

use crate::AppState;
use crate::launch::{LaunchTarget, launch};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const MANIFEST_FILE: &str = "manifest.json";
const EXTENSIONS_PAGE: &str = "chrome://extensions";

#[cfg(target_os = "macos")]
const CHROME: &str = "Google Chrome";
#[cfg(not(target_os = "macos"))]
const CHROME: &str = "chrome.exe";

// The startup copy and a tray click can overlap, and each deletes the other's staging folder.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
enum InstallError {
    #[error("the app has no bundled mc-meet: {0}")]
    NoBundle(#[source] tauri::Error),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is not a valid manifest: {source}", path.display())]
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// Copies mc-meet into the data folder when the folder holds another version.
pub(crate) fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = install(&app, Recopy::WhenVersionDiffers) {
            tracing::error!(exception.message = %error, "could not install mc-meet");
        }
    });
}

/// Shows the folder to load and opens Chrome's extensions page, where the user drops it. The
/// folder is copied again first, which repairs one with missing files.
pub(crate) fn show_setup(app: &AppHandle) {
    let folder = match install(app, Recopy::Always) {
        Ok(folder) => folder,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not install mc-meet");
            return;
        }
    };
    launch(app, &LaunchTarget::RevealFile(&folder));
    if let Err(error) = app.opener().open_url(EXTENSIONS_PAGE, Some(CHROME)) {
        tracing::warn!(exception.message = %error, "could not open the Chrome extensions page");
    }
}

/// The version in the installed folder's manifest. mc-meet reloads itself when this differs from
/// its own version.
pub(crate) fn folder_version(app: &AppHandle) -> Option<String> {
    installed_version(&extension_folder(app))
}

#[derive(Clone, Copy)]
enum Recopy {
    Always,
    WhenVersionDiffers,
}

fn extension_folder(app: &AppHandle) -> PathBuf {
    app.state::<AppState>()
        .settings
        .app_dirs()
        .meet_extension_dir()
}

fn install(app: &AppHandle, recopy: Recopy) -> Result<PathBuf, InstallError> {
    let _locked = INSTALL_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let folder = extension_folder(app);
    match recopy {
        Recopy::Always => {}
        Recopy::WhenVersionDiffers => {
            if installed_version(&folder).as_deref() == Some(APP_VERSION) {
                return Ok(folder);
            }
        }
    }
    let bundle = app
        .path()
        .resolve("mc-meet", BaseDirectory::Resource)
        .map_err(InstallError::NoBundle)?;
    // Chrome reads the folder whenever the worker starts, so the new copy is built beside it and
    // swapped in with two renames.
    let staging = folder.with_extension("new");
    let previous = folder.with_extension("old");
    remove_folder(&staging)?;
    remove_folder(&previous)?;
    copy_folder(&bundle, &staging)?;
    stamp_version(&staging.join(MANIFEST_FILE))?;
    let has_previous = folder.exists();
    if has_previous {
        rename(&folder, &previous)?;
    }
    if let Err(error) = rename(&staging, &folder) {
        // Put the old copy back, so Chrome still finds an extension at the next start.
        if has_previous && let Err(restore_error) = rename(&previous, &folder) {
            tracing::error!(exception.message = %restore_error, "could not restore mc-meet");
        }
        return Err(error);
    }
    // The new copy is in place, so a leftover old one is only clutter.
    if let Err(error) = remove_folder(&previous) {
        tracing::warn!(exception.message = %error, "could not remove the old mc-meet");
    }
    tracing::info!(
        file.directory = %folder.display(),
        browser.extension.version = APP_VERSION,
        "mc-meet installed"
    );
    Ok(folder)
}

// A missing or unreadable manifest reads as no version, which installs a fresh copy.
fn installed_version(folder: &Path) -> Option<String> {
    let text = std::fs::read(folder.join(MANIFEST_FILE)).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&text).ok()?;
    manifest["version"].as_str().map(str::to_owned)
}

fn stamp_version(manifest_path: &Path) -> Result<(), InstallError> {
    let io_error = |source| InstallError::Io {
        path: manifest_path.to_path_buf(),
        source,
    };
    let manifest_error = |source| InstallError::Manifest {
        path: manifest_path.to_path_buf(),
        source,
    };
    let text = std::fs::read(manifest_path).map_err(io_error)?;
    let mut manifest: serde_json::Value = serde_json::from_slice(&text).map_err(manifest_error)?;
    manifest["version"] = APP_VERSION.into();
    let text = serde_json::to_vec_pretty(&manifest).map_err(manifest_error)?;
    std::fs::write(manifest_path, text).map_err(io_error)
}

// Docs stay behind: the extension never reads them.
fn copy_folder(from: &Path, to: &Path) -> Result<(), InstallError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| InstallError::Io { path, source }
    };
    std::fs::create_dir_all(to).map_err(io_error(to))?;
    for entry in std::fs::read_dir(from).map_err(io_error(from))? {
        let entry = entry.map_err(io_error(from))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(io_error(&source))?.is_dir() {
            copy_folder(&source, &target)?;
        } else if source.extension().is_none_or(|extension| extension != "md") {
            std::fs::copy(&source, &target).map_err(io_error(&target))?;
        }
    }
    Ok(())
}

fn rename(from: &Path, to: &Path) -> Result<(), InstallError> {
    std::fs::rename(from, to).map_err(|source| InstallError::Io {
        path: to.to_path_buf(),
        source,
    })
}

fn remove_folder(folder: &Path) -> Result<(), InstallError> {
    match std::fs::remove_dir_all(folder) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(InstallError::Io {
            path: folder.to_path_buf(),
            source: error,
        }),
    }
}
