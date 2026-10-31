//! Which window the video process records: the one the app's hint names by id, and no other.
//! Portable, so the rule reads the same wherever the list comes from.
//!
//! A title never picks a window. Slack's main window can carry the huddle's channel name and
//! shows private messages, so a closed huddle window waits for the app's next hint, which the
//! huddle watcher sends with the reopened window's id.

use serde::{Deserialize, Serialize};

/// What the app knows of the call's window when it starts video, or when the window reopens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowHint {
    pub bundle_id: String,
    /// The window's whole title, for the records only.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pid: Option<i32>,
    /// The window the huddle watcher saw. Video records this window only.
    pub window_id: u32,
}

/// One window as the shareable-content list shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ListedWindow {
    pub window_id: u32,
    pub bundle_id: Option<String>,
    pub title: Option<String>,
    pub pid: i32,
    pub layer: i64,
    pub width_pt: f64,
    pub height_pt: f64,
    pub is_on_screen: bool,
}

/// The hint's window, while it exists in the hinted app.
#[must_use]
pub fn pick_window<'a>(hint: &WindowHint, windows: &'a [ListedWindow]) -> Option<&'a ListedWindow> {
    windows.iter().find(|window| {
        window.window_id == hint.window_id
            && window.bundle_id.as_deref() == Some(hint.bundle_id.as_str())
            && hint.pid.is_none_or(|pid| pid == window.pid)
    })
}
