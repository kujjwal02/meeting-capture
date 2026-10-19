//! The native messaging host manifest, which tells each browser what to start for
//! `com.meetingcapture.app`.

use std::io;
use std::path::{Path, PathBuf};

use crate::{EXTENSION_ORIGIN, HOST_NAME};

const MANIFEST_FILE_NAME: &str = "com.meetingcapture.app.json";

/// Where one browser was told about the host.
#[derive(Debug)]
pub struct Registration {
    pub browser: &'static str,
    /// The manifest file, or the registry key that points at it.
    pub location: String,
    pub outcome: io::Result<()>,
}

fn manifest_json(executable: &Path) -> io::Result<String> {
    serde_json::to_string_pretty(&serde_json::json!({
        "name": HOST_NAME,
        "description": "Meeting Capture: receives mc-meet's call events",
        "path": executable,
        "type": "stdio",
        "allowed_origins": [EXTENSION_ORIGIN],
    }))
    .map_err(io::Error::other)
}

// Config folders under ~/Library/Application Support.
#[cfg(target_os = "macos")]
const BROWSER_CONFIG_DIRS: [(&str, &str); 6] = [
    ("Google Chrome", "Google/Chrome"),
    ("Chrome Beta", "Google/Chrome Beta"),
    ("Chrome Canary", "Google/Chrome Canary"),
    ("Chromium", "Chromium"),
    ("Brave", "BraveSoftware/Brave-Browser"),
    ("Microsoft Edge", "Microsoft Edge"),
];

/// Writes the host manifest, with `executable` as the host, for each installed browser. A
/// browser whose config folder is missing is skipped. `_data_dir` holds the manifest on Windows.
///
/// # Errors
///
/// Returns `NotFound` when the OS reports no home folder. Each browser's own outcome is in its
/// `Registration`.
#[cfg(target_os = "macos")]
pub fn register(executable: &Path, _data_dir: &Path) -> io::Result<Vec<Registration>> {
    let base_dirs = directories::BaseDirs::new()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home folder"))?;
    let manifest = manifest_json(executable)?;
    let mut registrations = Vec::new();
    for (browser, config_dir) in BROWSER_CONFIG_DIRS {
        let config_dir = base_dirs.data_dir().join(config_dir);
        if !config_dir.is_dir() {
            continue;
        }
        let manifest_path = config_dir
            .join("NativeMessagingHosts")
            .join(MANIFEST_FILE_NAME);
        registrations.push(Registration {
            browser,
            location: manifest_path.display().to_string(),
            outcome: write_manifest(&manifest_path, &manifest),
        });
    }
    Ok(registrations)
}

// Chrome Beta and Canary read Chrome's key on Windows.
#[cfg(windows)]
const BROWSER_REGISTRY_KEYS: [(&str, &str); 4] = [
    (
        "Google Chrome",
        r"Software\Google\Chrome\NativeMessagingHosts",
    ),
    ("Chromium", r"Software\Chromium\NativeMessagingHosts"),
    (
        "Brave",
        r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
    ),
    (
        "Microsoft Edge",
        r"Software\Microsoft\Edge\NativeMessagingHosts",
    ),
];

/// Writes the host manifest, with `executable` as the host, into `data_dir`, and points each
/// browser's registry key at it.
///
/// # Errors
///
/// Returns the error writing the manifest file. Each browser's own outcome is in its
/// `Registration`.
#[cfg(windows)]
pub fn register(executable: &Path, data_dir: &Path) -> io::Result<Vec<Registration>> {
    let manifest_path = data_dir.join(MANIFEST_FILE_NAME);
    write_manifest(&manifest_path, &manifest_json(executable)?)?;
    let manifest_path = manifest_path.to_string_lossy().into_owned();
    Ok(BROWSER_REGISTRY_KEYS
        .into_iter()
        .map(|(browser, hosts_key)| {
            let key_path = format!(r"{hosts_key}\{HOST_NAME}");
            let outcome = windows_registry::CURRENT_USER
                .create(&key_path)
                .and_then(|key| key.set_string("", &manifest_path))
                .map_err(io::Error::other);
            Registration {
                browser,
                location: format!(r"HKCU\{key_path}"),
                outcome,
            }
        })
        .collect())
}

// Through a temporary file and a rename, so a browser starting now never reads half a manifest.
fn write_manifest(manifest_path: &Path, manifest: &str) -> io::Result<()> {
    let hosts_dir = manifest_path
        .parent()
        .map_or_else(PathBuf::new, Path::to_path_buf);
    std::fs::create_dir_all(&hosts_dir)?;
    let partial_path = manifest_path.with_extension("json.partial");
    std::fs::write(&partial_path, manifest)?;
    std::fs::rename(&partial_path, manifest_path)
}
