//! Opens the `meetingcapture://call/...` links in meeting notes. The OS hands a clicked link to
//! the running app: on macOS as an open-URL event, on Windows as a second launch that the
//! single-instance plugin forwards. A link to a file the Drive archive took opens its Drive copy,
//! from the folder's `drive.json`. A link to other media this machine lacks shows why in a
//! notification, and opens the call folder when it exists.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use call_folder::drive_record::DriveRecord;
use call_folder::media::{MediaLink, MediaTarget, PurgeRecord, VIDEO_FILE_NAME};
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
    if !path.exists()
        && let Some(drive_copy) = drive_copy(&folder, &link)
    {
        open_drive_copy(app, &drive_copy);
        return;
    }
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

/// The Drive copy a link opens in place of a file the archive deleted.
struct DriveCopy {
    web_view_link: String,
    /// The stem lives on only in the video's mix, so the video opens instead.
    is_in_video_mix: bool,
}

/// The Drive copy of the link's file, from `drive.json`. A stem the archive deleted with the
/// video that mixed it opens the video's copy.
fn drive_copy(folder: &std::path::Path, link: &MediaLink) -> Option<DriveCopy> {
    let local_name = match link.target {
        MediaTarget::Video => VIDEO_FILE_NAME.to_owned(),
        MediaTarget::Stem { number, side } => side.file_name(number),
        MediaTarget::Folder => return None,
    };
    let record = match DriveRecord::<serde::de::IgnoredAny>::read(folder) {
        Ok(record) => record?,
        Err(error) => {
            tracing::warn!(exception.message = %error, "drive.json unreadable; no Drive copy to open");
            return None;
        }
    };
    let link_of = |name: &str| {
        record
            .files
            .iter()
            .find(|file| file.local_name == name)
            .and_then(|file| file.drive_link().map(str::to_owned))
    };
    if let Some(web_view_link) = link_of(&local_name) {
        return Some(DriveCopy {
            web_view_link,
            is_in_video_mix: false,
        });
    }
    if record.mixed_stem_names.contains(&local_name) {
        return link_of(VIDEO_FILE_NAME).map(|web_view_link| DriveCopy {
            web_view_link,
            is_in_video_mix: true,
        });
    }
    None
}

fn open_drive_copy(app: &AppHandle, drive_copy: &DriveCopy) {
    tracing::info!(
        mc.media.is_in_video_mix = drive_copy.is_in_video_mix,
        "media archived; opening its Drive copy"
    );
    if drive_copy.is_in_video_mix {
        crate::notify(
            app,
            "Opening the video on Google Drive",
            "This audio was archived inside the call's video. The video opens instead.",
        );
    }
    if let Err(error) = app
        .opener()
        .open_url(&drive_copy.web_view_link, None::<&str>)
    {
        tracing::error!(exception.message = %error, "could not open the Drive copy");
    }
}
