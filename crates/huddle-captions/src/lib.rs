//! Reads Slack huddle captions from the Slack desktop app through the OS accessibility API and
//! writes one log per huddle. The log is the huddle's raw input; `sources::slack_huddle` parses
//! it. macOS only for now; on other OSes `spawn` starts nothing.

// Only the macOS reader writes huddle logs and feeds the tracker. Neither needs macOS, so a
// Windows reader can use both.
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "only the macOS reader writes huddle logs")
)]
mod log;
#[cfg(target_os = "macos")]
mod mac;
pub mod profile;
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "only the macOS reader feeds the tracker")
)]
mod tracker;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub use log::finish_open_logs;
pub use sources::append_log::FinishedLogs;
pub use sources::slack_huddle::HuddleHealth;

/// Read before each huddle step, so a settings change applies without a restart.
#[derive(Debug, Clone)]
pub struct WatchSettings {
    pub read_captions: bool,
    /// Press Slack's Captions tab when captions are on but the caption panel is hidden.
    pub auto_start_captions: bool,
    /// The logs of huddles the user stopped recording. Their captions are read but not logged,
    /// so a resume does not log what was said while stopped.
    pub stopped_logs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchPaths {
    /// One log per huddle goes here.
    pub log_dir: PathBuf,
    /// Layout profile files here override the built-in ones.
    pub layout_override_dir: PathBuf,
}

/// What the watcher tells the app. Each huddle is named by its log path.
#[derive(Debug, Clone)]
pub enum Notice {
    /// The app lacks the Accessibility permission. The watcher showed the system prompt once.
    PermissionMissing,
    PermissionGranted,
    HuddleStarted {
        log_path: PathBuf,
        /// A channel name, or the other people's names in a DM.
        channel: String,
    },
    /// The huddle's health changed. Reported once per change.
    Health {
        log_path: PathBuf,
        channel: String,
        state: HuddleHealth,
        reason: String,
    },
    /// Caption lines were logged or changed since the last report.
    CaptionsRead {
        log_path: PathBuf,
        /// Caption rows and overlay lines logged in the huddle so far.
        caption_lines: u64,
    },
    /// The huddle window closed, or reading stopped. The log is complete.
    HuddleEnded {
        log_path: PathBuf,
        channel: String,
    },
    /// A layout profile file is invalid, or a log could not be written.
    Problem {
        message: String,
    },
}

/// Starts the watcher on its own thread. It runs until the process exits.
///
/// # Errors
///
/// Returns the error from starting the thread.
pub fn spawn(
    paths: WatchPaths,
    settings: impl Fn() -> WatchSettings + Send + 'static,
    notify: impl FnMut(Notice) + Send + 'static,
) -> std::io::Result<Option<std::thread::JoinHandle<()>>> {
    #[cfg(target_os = "macos")]
    return std::thread::Builder::new()
        .name("huddle-captions".to_owned())
        .spawn(move || mac::run(&paths, &settings, notify))
        .map(Some);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (paths, settings, notify);
        tracing::info!("Slack huddle captions are read on macOS only");
        Ok(None)
    }
}

/// The files in `dir` with `extension`, sorted. A missing folder has none.
fn files_with_extension(dir: &Path, extension: &str) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|found| found == extension))
        .collect();
    paths.sort();
    Ok(paths)
}
