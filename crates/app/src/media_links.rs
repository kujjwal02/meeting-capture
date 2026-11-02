//! Opens the `meetingcapture://call/...` links in meeting notes. The OS hands a clicked link to
//! the running app: on macOS as an open-URL event, on Windows as a second launch that the
//! single-instance plugin forwards. A link to media this machine lacks shows why in a
//! notification, and opens the call folder when it exists.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use call_folder::media::{MediaLink, MediaTarget, PurgeRecord};
use tauri::{AppHandle, Manager as _};
use tauri_plugin_deep_link::DeepLinkExt as _;
use tauri_plugin_opener::OpenerExt as _;

use crate::AppState;
use crate::call_status::display_time_zone;

// A link the launch read and the open-URL event both deliver opens once. Within this time, the
// same link again counts as that one delivery.
const REPEAT_WINDOW: Duration = Duration::from_secs(2);

static LAST_OPENED: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Listens for clicked media links. Call once, in setup.
pub(crate) fn start(app: &AppHandle) {
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            open_once(&handle, url.as_str());
        }
    });
    // On Windows a click that starts the app passes the link as an argument, which the plugin
    // reads before any listener exists. On macOS the link arrives as an event, which may come
    // before this listener; the plugin keeps it as the current link either way.
    match app.deep_link().get_current() {
        Ok(Some(urls)) => {
            for url in urls {
                open_once(app, url.as_str());
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(exception.message = %error, "could not read the launch link");
        }
    }
}

fn open_once(app: &AppHandle, url: &str) {
    let now = Instant::now();
    {
        let mut last_opened = LAST_OPENED.lock().unwrap_or_else(PoisonError::into_inner);
        let is_repeat = last_opened.as_ref().is_some_and(|(last_url, at)| {
            last_url == url && now.duration_since(*at) < REPEAT_WINDOW
        });
        if is_repeat {
            return;
        }
        *last_opened = Some((url.to_owned(), now));
    }
    open(app, url);
}

fn open(app: &AppHandle, url: &str) {
    let _span = tracing::info_span!("media_link.open", url.full = url).entered();
    let link = match MediaLink::parse(url) {
        Ok(link) => link,
        Err(error) => {
            tracing::warn!(exception.message = %error, "media link refused");
            crate::notify(app, "Link not opened", &error.to_string());
            return;
        }
    };
    let state = app.state::<AppState>();
    let calls_dir = state.settings.app_dirs().calls_dir();
    let folder = link.folder_in(&calls_dir);
    if !folder.is_dir() {
        tracing::info!("call folder not on this machine");
        crate::notify(
            app,
            "Recording not on this computer",
            "Another computer recorded this call, or its folder was deleted.",
        );
        return;
    }
    let path = link.path_in(&calls_dir);
    let (opened, missing_reason) = if path.exists() {
        (path, None)
    } else {
        let reason = match link.target {
            MediaTarget::Video => match PurgeRecord::read(&folder) {
                Some(record) => format!(
                    "The video was purged on {}. The audio is kept.",
                    record
                        .purged_at
                        .to_zoned(display_time_zone(&state))
                        .strftime("%Y-%m-%d")
                ),
                None => "This call has no video file.".to_owned(),
            },
            MediaTarget::Stem { .. } => "That audio file is missing.".to_owned(),
            MediaTarget::Folder => "The call folder is missing.".to_owned(),
        };
        (folder, Some(reason))
    };
    if let Some(reason) = &missing_reason {
        tracing::info!(
            exception.message = reason,
            "media missing; opening the call folder"
        );
        crate::notify(
            app,
            "Recording file not found",
            &format!("{reason} The call folder is open instead."),
        );
    }
    if let Err(error) = app
        .opener()
        .open_path(opened.to_string_lossy(), None::<&str>)
    {
        tracing::error!(
            file.path = %opened.display(),
            exception.message = %error,
            "could not open"
        );
    }
}
