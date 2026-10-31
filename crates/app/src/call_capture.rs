//! Records each call through `mc-capture` child processes that own the devices and the media
//! files. The app never links Core Audio or ScreenCaptureKit, so a crash or a hung framework call
//! in a capture process never takes the tray down. `mc-capture audio` records every call.
//! `mc-capture video` records a Slack huddle's own window next to it, with the same segment
//! number. Meet video comes from mc-meet's tab capture: a task in the app writes the fragments
//! mc-meet sends, with no capture process and no Screen Recording grant. Each process restarts
//! on its own, so video may fail while audio carries on. One stop control stops both.
//!
//! A supervisor task follows the call stream: `call.started` and `call.resumed` start a
//! recording, `call.ended` stops it, with no click. One call records at a time, since the
//! audio tap hears every app anyway; a call that opens while another records waits for it to
//! end. "Stop recording this call" stops the call's capture processes and its captions until
//! "Resume recording". The call status board says which calls the user stopped.
//!
//! Each process restarts after 1, 2 and then 4 s. After 3 failures in a minute it is failing
//! until it records again, and restarts every 30 s, or every 5 minutes when a device would not
//! open. A start or resume takes the call's next segment number; a restart takes the next one
//! only when the last left files behind. While a failing process waits 5 minutes for a device,
//! `mc-capture await-device-change` watches the HAL, so a new device restarts it 1.5 s later.
//!
//! A sleep closes the segment inside the helper, which stays running. On wake the helper says
//! so and the supervisor starts the call's next segment in it. The video process follows the
//! audio helper: its segment stops at the audio sleep report, and the next one starts at wake
//! with the audio's new segment number. A video segment records the audio segment it pairs with,
//! since a video restart takes video's next free number. The helper's permission reports
//! feed the tray: a refused microphone or system audio permission shows as a problem. A refused
//! screen recording permission, at a segment start or at any stream start inside one, turns the
//! call's video row into "Video off: allow Screen Recording", which opens System Settings. The
//! audio process records on untouched. The next wake or window hint starts a new video process,
//! whose preflight checks again: macOS may cache the answer per process.
//!
//! At launch, after repair and finalize, and then once a day, `mc-capture purge-media` deletes
//! the video of calls that ended more than 90 days ago. The stems stay for transcription.

mod meet_shortcut;
mod restart_backoff;
mod tab_video;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use capture_protocol::finalize::{
    FinalizeOutcome, MEDIA_PURGE_INTERVAL, MediaPurge, PendingFinalize,
};
use capture_protocol::{
    AudioCommand, AudioErrorKind, AudioReport, DeviceChangeCause, MicMoment, PermissionService,
    PermissionState, Platform, Stem, VideoCommand, VideoErrorKind, VideoReport, WindowChangeCause,
    WindowHint,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::Instrument as _;

use self::restart_backoff::{FAILURES_SHOWN, RestartBackoff};
use crate::AppState;
use crate::call_status::{Moment, display_time_zone};
use crate::calls::{Call, CallEvent, CallPlatform};

/// A process whose counters and files both stood still this long is hung. Two device opens of
/// at most 5 s each fit inside it.
const HANG_LIMIT: Duration = Duration::from_secs(15);
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// The helper reads `stop` only after its two device opens of at most 5 s each, then closes its
/// segment within 1 s. Past this it is killed.
const STOP_LIMIT: Duration = Duration::from_secs(12);
const SIDECAR_LOG_FILE_NAME: &str = "mc-capture.log";
/// The shortest time between two respawns of one call's process for a new Screen Recording
/// check, so a refusal that comes back at once cannot spin the helper.
const RESPAWN_GAP: Duration = Duration::from_secs(5);
/// A finalize that runs this long hangs: a two-hour call takes about two minutes, plus up to
/// five waiting for another run on the same folder. Killed, it is retried once at the next
/// launch, then left.
const FINALIZE_LIMIT: Duration = Duration::from_mins(20);
/// Listing reads two small manifests per folder.
const PENDING_LIST_LIMIT: Duration = Duration::from_secs(60);
/// A capture process that outlived its call closes its segment within seconds of its stop.
const RECORDING_RETRY_DELAY: Duration = Duration::from_secs(30);
/// The purge reads a few file times per folder and unlinks at most a few thousand fragments.
const PURGE_LIMIT: Duration = Duration::from_mins(10);
/// The purge interval is time on the wall clock, not the monotonic one, which stops while the
/// Mac sleeps: a laptop asleep every night would otherwise purge every few days.
const PURGE_CHECK_INTERVAL: Duration = Duration::from_hours(1);
/// The helper log moves to `mc-capture.log.1` at a start once it is this big.
const SIDECAR_LOG_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(windows)]
const SIDECAR_FILE_NAME: &str = "mc-capture.exe";
#[cfg(not(windows))]
const SIDECAR_FILE_NAME: &str = "mc-capture";
/// After a device change ends a backoff early, the wait before the restart: Bluetooth routes
/// take this long to settle.
const DEVICE_CHANGE_SETTLE: Duration = Duration::from_millis(1_500);

/// What a call's audio is doing, as the tray shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioState {
    /// Another call holds the devices.
    Queued,
    /// The helper runs and no stream delivered yet.
    Starting {
        segment: u32,
    },
    /// Stems are open.
    Recording {
        segment: u32,
    },
    /// Stems are open, but the mic went away and no safe input is left. The helper keeps
    /// looking for one.
    RecordingWithoutMic {
        segment: u32,
    },
    /// The helper exited or hung; it restarts after the backoff.
    Restarting,
    /// The helper failed 3 times within 60 s and has not recorded since. It restarts every 30 s,
    /// or every 5 minutes when a device would not open.
    Failing,
    /// The Mac sleeps or just woke. The helper closed the segment and opens the next on wake.
    Sleeping,
    StoppedByUser,
}

impl AudioState {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Queued => "Audio: waiting, another call is recording".to_owned(),
            Self::Starting { segment } => format!("Audio: starting segment {segment:03}"),
            Self::Recording { segment } => format!("Audio: recording segment {segment:03}"),
            Self::RecordingWithoutMic { segment } => {
                format!("Audio: recording segment {segment:03} without your mic")
            }
            Self::Restarting => "Audio: the helper stopped, restarting".to_owned(),
            Self::Failing => "Audio: recording failed, retrying".to_owned(),
            Self::Sleeping => "Audio: paused while the Mac sleeps".to_owned(),
            Self::StoppedByUser => "Audio: stopped by you".to_owned(),
        }
    }
}

/// What a call's video is doing, as the tray shows it: a huddle's window, or a Meet tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoState {
    /// The process runs and no frame went to the encoder yet: it may be looking for the window.
    Starting {
        segment: u32,
    },
    Recording {
        segment: u32,
    },
    /// The huddle window closed. The process looks for it every second.
    WindowMissing {
        segment: u32,
    },
    /// The process exited or hung; it restarts after the backoff.
    Restarting,
    /// The process failed 3 times within 60 s and has not recorded since.
    Failing,
    /// The audio helper reported a sleep, so the video segment closed. The next opens at wake.
    Sleeping,
    /// macOS refuses screen recording. The row opens System Settings.
    ScreenRecordingRefused,
    /// The user ended the capture through the system's sharing controls. Audio goes on.
    StoppedFromSharingControls,
    /// The app wants the Meet tab's video and no fragment came yet.
    TabWaiting,
    /// Chrome refuses tab capture until mc-meet is invoked on the Meet tab once, and the app
    /// cannot press mc-meet's shortcut: this OS has no key sender, or its presses did not take.
    TabNeedsClick,
    /// The app presses mc-meet's shortcut once the Meet tab is the active tab of the Chrome
    /// window focused last, which a switch to the tab makes it. Until then a click on the
    /// mc-meet icon also starts it.
    TabAwaitingFront,
    /// macOS sends the app's key events only with the Accessibility grant. The row opens System
    /// Settings; the click still works.
    TabNeedsAccessibility,
    /// Chrome has no key bound to mc-meet's shortcut.
    TabShortcutUnbound,
    /// mc-meet's bound shortcut names a key the app cannot send.
    TabShortcutUnmappable,
    /// mc-meet's bound shortcut holds neither Command nor Control, so a press that lands where
    /// it is not bound could type a character.
    TabShortcutTypesText,
    /// No key on the current keyboard layout types the shortcut's character.
    TabShortcutNotOnLayout,
    /// A press reached no mc-meet, so it went to a Chrome window mc-meet cannot see, such as
    /// another profile. The app presses no more for the call.
    TabPressUnseen,
    /// mc-meet did not answer the press notice in time, so the app did not press. It asks again
    /// after the press interval.
    TabPressUnanswered,
    /// mc-meet is too old to say where the Meet tab is, so the app does not press.
    TabExtensionOutdated,
    /// The tab's stream ended, as when the tab left Meet. mc-meet starts it again when it can.
    TabStopped,
    /// mc-meet could not open the stream or the encoder.
    TabFailed,
}

impl VideoState {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Starting { segment } => format!("Video: starting segment {segment:03}"),
            Self::Recording { segment } => format!("Video: recording segment {segment:03}"),
            Self::WindowMissing { segment } => {
                format!("Video: segment {segment:03} lost the huddle window, looking for it")
            }
            Self::Restarting => "Video: the helper stopped, restarting".to_owned(),
            Self::Failing => "Video: recording failed, retrying".to_owned(),
            Self::Sleeping => "Video: paused while the Mac sleeps".to_owned(),
            Self::ScreenRecordingRefused => "Video off: allow Screen Recording".to_owned(),
            Self::StoppedFromSharingControls => {
                "Video: stopped from the sharing controls".to_owned()
            }
            Self::TabWaiting => "Video: waiting for the Meet tab".to_owned(),
            Self::TabNeedsClick => "Video: click the mc-meet icon on the Meet tab".to_owned(),
            Self::TabAwaitingFront => {
                "Video: starts when you switch to the Meet tab, or click the mc-meet icon"
                    .to_owned()
            }
            Self::TabNeedsAccessibility => {
                "Video: allow Accessibility for automatic Meet video".to_owned()
            }
            Self::TabShortcutUnbound => {
                "Video: click the mc-meet icon; mc-meet has no shortcut in Chrome".to_owned()
            }
            Self::TabShortcutUnmappable => {
                "Video: click the mc-meet icon; mc-meet's shortcut cannot be sent".to_owned()
            }
            Self::TabShortcutTypesText => {
                "Video: click the mc-meet icon; bind mc-meet's shortcut with ⌘ for automatic video"
                    .to_owned()
            }
            Self::TabShortcutNotOnLayout => {
                "Video: click the mc-meet icon; the keyboard layout has no key for its shortcut"
                    .to_owned()
            }
            Self::TabPressUnseen => {
                "Video: click the mc-meet icon; the shortcut went to another Chrome window"
                    .to_owned()
            }
            Self::TabPressUnanswered => {
                "Video: mc-meet did not answer the shortcut press, retrying; or click its icon"
                    .to_owned()
            }
            Self::TabExtensionOutdated => {
                "Video: click the mc-meet icon; update mc-meet for automatic video".to_owned()
            }
            Self::TabStopped => "Video: the Meet tab stopped sending video".to_owned(),
            Self::TabFailed => "Video: the Meet tab could not be recorded".to_owned(),
        }
    }

    /// The privacy pane the row opens, when the row is an action.
    pub(crate) fn settings_pane(self) -> Option<VideoSettingsPane> {
        match self {
            Self::ScreenRecordingRefused => Some(VideoSettingsPane::ScreenRecording),
            Self::TabNeedsAccessibility => Some(VideoSettingsPane::Accessibility),
            Self::Starting { .. }
            | Self::Recording { .. }
            | Self::WindowMissing { .. }
            | Self::Restarting
            | Self::Failing
            | Self::Sleeping
            | Self::StoppedFromSharingControls
            | Self::TabWaiting
            | Self::TabNeedsClick
            | Self::TabAwaitingFront
            | Self::TabShortcutUnbound
            | Self::TabShortcutUnmappable
            | Self::TabShortcutTypesText
            | Self::TabShortcutNotOnLayout
            | Self::TabPressUnseen
            | Self::TabPressUnanswered
            | Self::TabExtensionOutdated
            | Self::TabStopped
            | Self::TabFailed => None,
        }
    }
}

/// The System Settings pane a video row opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoSettingsPane {
    ScreenRecording,
    Accessibility,
}

/// The supervisor's wake-up for each stop or resume, and each open call's audio and video state.
pub(crate) struct CallCapture {
    /// The call whose stop or resume the board just took.
    stop_changes: mpsc::UnboundedSender<PathBuf>,
    /// Taken once, by `start`.
    stop_change_receiver: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>>,
    states: Mutex<HashMap<PathBuf, AudioState>>,
    video_states: Mutex<HashMap<PathBuf, VideoState>>,
    /// The last state the helper read for each permission.
    permissions: Mutex<HashMap<PermissionService, PermissionState>>,
}

impl CallCapture {
    pub fn new() -> Self {
        let (stop_changes, stop_change_receiver) = mpsc::unbounded_channel();
        Self {
            stop_changes,
            stop_change_receiver: Mutex::new(Some(stop_change_receiver)),
            states: Mutex::new(HashMap::new()),
            video_states: Mutex::new(HashMap::new()),
            permissions: Mutex::new(HashMap::new()),
        }
    }

    /// The permissions recording needs that macOS refuses.
    pub fn refused_permissions(&self) -> Vec<PermissionService> {
        let permissions = self
            .permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut refused: Vec<PermissionService> = permissions
            .iter()
            .filter(|(_, state)| is_refused(**state))
            .map(|(service, _)| *service)
            .collect();
        refused.sort_by_key(|service| permission_name(*service));
        refused
    }

    /// Records a permission state. Notifies once when a permission becomes refused, and
    /// refreshes the tray on any change.
    fn set_permission(&self, app: &AppHandle, service: PermissionService, state: PermissionState) {
        let previous = self
            .permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(service, state);
        if previous == Some(state) {
            return;
        }
        tracing::info!(
            permission.service = %service,
            permission.state = %state,
            "permission state changed"
        );
        if is_refused(state) && !previous.is_some_and(is_refused) {
            crate::notify(
                app,
                "Recording needs a permission",
                &format!(
                    "macOS refuses Meeting Capture {}. Allow it in System Settings, Privacy & \
                     Security.",
                    permission_name(service)
                ),
            );
        }
        crate::tray::show_call_status(app);
    }

    /// The audio state of each open call that has one.
    pub fn states(&self) -> HashMap<PathBuf, AudioState> {
        self.lock().clone()
    }

    /// Whether any call has its stems open.
    pub fn is_recording(&self) -> bool {
        self.lock().values().any(|state| {
            matches!(
                state,
                AudioState::Recording { .. } | AudioState::RecordingWithoutMic { .. }
            )
        })
    }

    /// Whether any call fails to record, records without the user's mic, or has its video off
    /// for a refused permission.
    pub fn has_problem(&self) -> bool {
        let is_audio_problem = self.lock().values().any(|state| {
            matches!(
                state,
                AudioState::Failing | AudioState::RecordingWithoutMic { .. }
            )
        });
        is_audio_problem
            || self.lock_video().values().any(|state| {
                matches!(
                    state,
                    VideoState::Failing
                        | VideoState::ScreenRecordingRefused
                        | VideoState::TabFailed
                )
            })
    }

    /// The video state of each open call that has one.
    pub fn video_states(&self) -> HashMap<PathBuf, VideoState> {
        self.lock_video().clone()
    }

    fn lock_video(&self) -> MutexGuard<'_, HashMap<PathBuf, VideoState>> {
        self.video_states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn set_video_state(&self, app: &AppHandle, call_id: &Path, state: Option<VideoState>) {
        let is_changed = {
            let mut states = self.lock_video();
            let previous = match state {
                Some(state) => states.insert(call_id.to_path_buf(), state),
                None => states.remove(call_id),
            };
            previous != state
        };
        if is_changed {
            tracing::info!(
                call.id = %call_id.display(),
                video.state = state.map(VideoState::label),
                "video state changed"
            );
            crate::tray::show_call_status(app);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, AudioState>> {
        self.states.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, app: &AppHandle, call_id: &Path, state: Option<AudioState>) {
        let is_changed = {
            let mut states = self.lock();
            let previous = match state {
                Some(state) => states.insert(call_id.to_path_buf(), state),
                None => states.remove(call_id),
            };
            previous != state
        };
        if is_changed {
            tracing::info!(
                call.id = %call_id.display(),
                audio.state = state.map(AudioState::label),
                "audio state changed"
            );
            crate::tray::show_call_status(app);
        }
    }
}

/// Denied, or blocked by a profile. Not yet asked and unreadable are no problem to show: the
/// first open asks, and an unreadable state may well be granted.
fn is_refused(state: PermissionState) -> bool {
    match state {
        PermissionState::Denied | PermissionState::Restricted => true,
        PermissionState::Granted | PermissionState::NotDetermined | PermissionState::Unknown => {
            false
        }
    }
}

/// The permission as System Settings names it.
pub(crate) fn permission_name(service: PermissionService) -> &'static str {
    match service {
        PermissionService::Microphone => "the microphone",
        PermissionService::AudioCapture => "system audio recording",
        PermissionService::ScreenRecording => "screen recording",
    }
}

/// "Stop recording this call", from the tray or mc-meet's popup. Stops the call's capture
/// processes, and its captions and chat. What was recorded stays.
pub(crate) fn stop_call(app: &AppHandle, call_id: &Path) {
    set_recording_stopped(app, call_id, true);
}

/// "Resume recording": new segments, and captions again.
pub(crate) fn resume_call(app: &AppHandle, call_id: &Path) {
    set_recording_stopped(app, call_id, false);
}

fn set_recording_stopped(app: &AppHandle, call_id: &Path, is_stopped: bool) {
    let state = app.state::<AppState>();
    tracing::info!(
        event.name = if is_stopped {
            "call.recording_stopped"
        } else {
            "call.recording_resumed"
        },
        call.id = %call_id.display(),
        "recording {} by the user",
        if is_stopped { "stopped" } else { "resumed" }
    );
    // A Meet call's log keeps the stop, so the call reopens stopped after a restart. A huddle's
    // watcher logs it at its next read of the board.
    let board = state.calls.status();
    state
        .browser_calls
        .set_recording_stopped(board, call_id, is_stopped);
    // At once, not at the next status tick, so mc-meet stops reading captions now.
    state
        .browser_calls
        .publish_call_status(&board.view(Moment::now()), &display_time_zone(&state));
    // An error only means no supervisor runs: no capture on this OS yet.
    let _unheard = state.call_capture.stop_changes.send(call_id.to_path_buf());
    crate::tray::show_call_status(app);
}

/// Starts the supervisor. Call before any call source starts, so it hears every call. Also
/// repairs, in the background, the segments a dead helper left open in any call folder, then
/// writes the `call.mp4` a crash or a quit left unwritten, then purges old video now and daily.
pub(crate) fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let app_dirs = state.settings.app_dirs();
    let calls_dir = app_dirs.calls_dir();
    let log_file = app_dirs.log_dir().join(SIDECAR_LOG_FILE_NAME);
    tauri::async_runtime::spawn(async move {
        let repair_dir = calls_dir.clone();
        if let Err(error) =
            tauri::async_runtime::spawn_blocking(move || repair_call_folders(&repair_dir)).await
        {
            tracing::error!(exception.message = %error, "repair.task_failed");
        }
        // After the repair, so finalize never meets a folder lock the repair holds and skips it.
        // Only the Mac records video, a huddle's window or a Meet tab, so only the Mac has any
        // to finalize or purge.
        if cfg!(target_os = "macos") {
            finalize_pending(&calls_dir, &log_file).await;
            // After finalize, so a folder that waited for its `call.mp4` has it by now.
            purge_media_daily(&calls_dir, &log_file).await;
        }
    });
    let stop_changes = state
        .call_capture
        .stop_change_receiver
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    // Windows capture arrives in phase 5. Dropping the receiver there makes each stop's send
    // fail at once; the stop still reaches mc-meet through the board.
    if !cfg!(target_os = "macos") {
        drop(stop_changes);
        tracing::info!("call capture is macOS only for now; calls record captions only");
        return;
    }
    let Some(stop_changes) = stop_changes else {
        return;
    };
    let events = state.calls.subscribe();
    let supervisor = Supervisor {
        app: app.clone(),
        calls_dir: app_dirs.calls_dir(),
        log_file: app_dirs.log_dir().join(SIDECAR_LOG_FILE_NAME),
        open_calls: Vec::new(),
        recorded_calls: Vec::new(),
        active: None,
    };
    tauri::async_runtime::spawn(supervisor.run(events, stop_changes));
}

/// Recovers what a `kill -9`, a crash or a power cut left open. The supervisor's state starts
/// empty, so the folder locks decide: a folder whose helper is still recording, as one that
/// outlived a killed app, is skipped, and that helper closes its own segment. Each segment start
/// repairs its folder again under the lock, so a call that starts at once loses nothing to the
/// race. stem-writer logs each repaired stem, and video-capture each video segment it ended.
fn repair_call_folders(calls_dir: &Path) {
    let folders = match call_folder::call_folders(calls_dir) {
        Ok(folders) => folders,
        Err(error) => {
            tracing::error!(
                file.directory = %calls_dir.display(),
                exception.message = %error,
                "repair.calls_unreadable"
            );
            return;
        }
    };
    let mut repaired_count = 0_usize;
    let mut failed_count = 0_usize;
    for folder in &folders {
        let audio = match stem_writer::repair_folder(folder) {
            Ok(stem_writer::FolderRepair::Repaired { .. }) => Ok(true),
            Ok(stem_writer::FolderRepair::Busy | stem_writer::FolderRepair::Clean) => Ok(false),
            Err(error) => Err(error.to_string()),
        };
        let video = match video_capture::repair_folder(folder) {
            Ok(video_capture::VideoFolderRepair::Repaired { .. }) => Ok(true),
            Ok(
                video_capture::VideoFolderRepair::Busy | video_capture::VideoFolderRepair::Clean,
            ) => Ok(false),
            Err(error) => Err(error.to_string()),
        };
        for (process, outcome) in [("audio", &audio), ("video", &video)] {
            if let Err(message) = outcome {
                tracing::error!(
                    file.directory = %folder.display(),
                    process.command = process,
                    exception.message = message,
                    "repair.failed"
                );
            }
        }
        if audio.is_err() || video.is_err() {
            failed_count += 1;
        } else if audio == Ok(true) || video == Ok(true) {
            repaired_count += 1;
        }
    }
    tracing::info!(
        file.directory = %calls_dir.display(),
        repair.folder_count = folders.len(),
        repair.repaired_folder_count = repaired_count,
        repair.failed_folder_count = failed_count,
        "repair.finished"
    );
}

/// Writes `call.mp4` for each call folder that lacks a current one, one helper run per folder,
/// so a folder that hangs or fails holds up none of the others. The helper lists the folders,
/// so the app links no AVFoundation.
async fn finalize_pending(calls_dir: &Path, log_file: &Path) {
    let listing = run_helper(
        &[
            "pending-finalize".as_ref(),
            "--calls-dir".as_ref(),
            calls_dir.as_os_str(),
        ],
        log_file,
        PENDING_LIST_LIMIT,
    )
    .await;
    let pending = match listing.map(|stdout| serde_json::from_str::<PendingFinalize>(&stdout)) {
        Ok(Ok(pending)) => pending,
        Ok(Err(error)) => {
            tracing::error!(exception.message = %error, "finalize.list_unreadable");
            return;
        }
        Err(reason) => {
            tracing::error!(exception.message = reason, "finalize.helper_failed");
            return;
        }
    };
    for folder in pending.folders {
        finalize_call(folder, log_file.to_owned()).await;
    }
}

/// Runs the purge now, then whenever a day has passed on the wall clock. Never returns.
async fn purge_media_daily(calls_dir: &Path, log_file: &Path) {
    loop {
        purge_media(calls_dir, log_file).await;
        let last_run = SystemTime::now();
        loop {
            tokio::time::sleep(PURGE_CHECK_INTERVAL).await;
            // A clock set back waits until it passes the last run again.
            let is_due = SystemTime::now()
                .duration_since(last_run)
                .is_ok_and(|elapsed| elapsed >= MEDIA_PURGE_INTERVAL);
            if is_due {
                break;
            }
        }
    }
}

/// Deletes the video of calls past the retention through the helper's `purge-media`. The helper
/// logs each deletion in the helper log; the app logs the totals.
async fn purge_media(calls_dir: &Path, log_file: &Path) {
    let run = run_helper(
        &[
            "purge-media".as_ref(),
            "--calls-dir".as_ref(),
            calls_dir.as_os_str(),
        ],
        log_file,
        PURGE_LIMIT,
    )
    .await;
    match run.map(|stdout| serde_json::from_str::<MediaPurge>(stdout.trim())) {
        Ok(Ok(purge)) => tracing::info!(
            file.directory = %calls_dir.display(),
            purge.trusted_now = purge.trusted_now.map(|at| at.to_string()),
            purge.first_seen_count = purge.first_seen_count,
            purge.purged_folder_count = purge.purged.len(),
            purge.kept_folder_count = purge.kept.len(),
            purge.deleted_file_count = purge
                .purged
                .iter()
                .map(|call| call.deleted_file_count)
                .sum::<u64>(),
            purge.deleted_bytes = purge.purged.iter().map(|call| call.deleted_bytes).sum::<u64>(),
            "purge.finished"
        ),
        Ok(Err(error)) => {
            tracing::error!(exception.message = %error, "purge.unreadable");
        }
        Err(reason) => tracing::error!(exception.message = reason, "purge.helper_failed"),
    }
}

/// Writes one call folder's `call.mp4` through the helper's `finalize`. A folder some capture
/// process still records in gets one more try once that process has had time to close.
async fn finalize_call(folder: PathBuf, log_file: PathBuf) {
    for is_retry in [false, true] {
        let outcome = finalize_once(&folder, &log_file).await;
        if !matches!(outcome, Some(FinalizeOutcome::Recording)) || is_retry {
            return;
        }
        tokio::time::sleep(RECORDING_RETRY_DELAY).await;
    }
}

async fn finalize_once(folder: &Path, log_file: &Path) -> Option<FinalizeOutcome> {
    let run = run_helper(
        &["finalize".as_ref(), "--folder".as_ref(), folder.as_os_str()],
        log_file,
        FINALIZE_LIMIT,
    )
    .await;
    let outcome = match run.map(|stdout| serde_json::from_str::<FinalizeOutcome>(stdout.trim())) {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            tracing::error!(
                file.directory = %folder.display(),
                exception.message = %error,
                "finalize.unreadable"
            );
            return None;
        }
        Err(reason) => {
            tracing::error!(
                file.directory = %folder.display(),
                exception.message = reason,
                "finalize.helper_failed"
            );
            return None;
        }
    };
    match &outcome {
        FinalizeOutcome::Written(summary) => tracing::info!(
            file.directory = %folder.display(),
            finalize.outcome = outcome.as_str(),
            finalize.length_ms = summary.length_ms,
            finalize.video_part_count = summary.video_part_count,
            finalize.stem_count = summary.stem_count,
            finalize.parts_shifted = summary.parts_shifted,
            finalize.skipped_count = summary.skipped.len(),
            "finalize.finished"
        ),
        FinalizeOutcome::Failed { message } => tracing::error!(
            file.directory = %folder.display(),
            finalize.outcome = outcome.as_str(),
            exception.message = message,
            "finalize.finished"
        ),
        FinalizeOutcome::NoVideo { skipped } => tracing::info!(
            file.directory = %folder.display(),
            finalize.outcome = outcome.as_str(),
            finalize.skipped_count = skipped.len(),
            "finalize.finished"
        ),
        FinalizeOutcome::Busy | FinalizeOutcome::Recording | FinalizeOutcome::Purged => {
            tracing::warn!(
                file.directory = %folder.display(),
                finalize.outcome = outcome.as_str(),
                "finalize.finished"
            );
        }
    }
    Some(outcome)
}

/// Runs the helper to its end, or kills it at `limit`, with its log lines in the helper log.
/// Its stdout, or why there is none.
async fn run_helper(
    args: &[&std::ffi::OsStr],
    log_file: &Path,
    limit: Duration,
) -> Result<String, String> {
    let helper = sidecar_path()?;
    rotate_log_if_full(log_file);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|error| format!("could not open {}: {error}", log_file.display()))?;
    let child = Command::new(&helper)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", helper.display()))?;
    // Dropping the timed-out future drops the child, which kills it.
    let output = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| format!("killed after {} s", limit.as_secs()))?
        .map_err(|error| format!("could not wait for the helper: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!("exited with {}", output.status))
    }
}

/// One capture process of the active recording.
struct RunningProcess {
    /// Sending or dropping it stops the process.
    stop: oneshot::Sender<()>,
    task: tauri::async_runtime::JoinHandle<()>,
}

/// The recording in progress: its call and its capture processes.
struct ActiveRecording {
    call_id: PathBuf,
    processes: Vec<RunningProcess>,
    /// Where the audio process stands, for a video process that starts late.
    audio_phase: watch::Receiver<AudioPhase>,
    /// New window hints for the video process. `None` while no video process runs.
    window_hints: Option<mpsc::UnboundedSender<WindowHint>>,
}

/// Where the call's audio process stands. The video process follows it across a sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioPhase {
    /// The audio segment open now, or the last one before a sleep.
    segment: u32,
    is_asleep: bool,
    /// Sleeps the audio process reported. A follower that saw neither a sleep nor its wake, as
    /// both came between two of its reads, still restarts on the wake segment.
    sleep_count: u64,
    /// When the audio last started, at the call start or a wake. Video from before it has no
    /// audio segment to pair with.
    awake_since: jiff::Timestamp,
}

/// What one process of a recording shares with the other.
#[derive(Default)]
struct ProcessLinks {
    /// The audio process publishes its phase here.
    audio_phase_out: Option<watch::Sender<AudioPhase>>,
    /// The video process follows the audio phase.
    audio_phase_in: Option<watch::Receiver<AudioPhase>>,
    /// The video process takes each window hint the huddle watcher sends.
    window_hints: Option<mpsc::UnboundedReceiver<WindowHint>>,
}

impl ProcessLinks {
    fn publish_audio(&self, change: impl FnOnce(&mut AudioPhase)) {
        if let Some(phase) = &self.audio_phase_out {
            phase.send_modify(change);
        }
    }

    /// The audio phase now, and marks it seen.
    fn audio_phase(&mut self) -> Option<AudioPhase> {
        self.audio_phase_in
            .as_mut()
            .map(|phase| *phase.borrow_and_update())
    }

    /// The audio phase now, left unseen, so `next_audio_phase` still wakes for it.
    fn audio_phase_now(&self) -> Option<AudioPhase> {
        self.audio_phase_in.as_ref().map(|phase| *phase.borrow())
    }

    /// The next audio phase change. Never ends once the audio process is gone.
    async fn next_audio_phase(phase: &mut Option<watch::Receiver<AudioPhase>>) -> AudioPhase {
        if let Some(receiver) = phase {
            if receiver.changed().await.is_ok() {
                return *receiver.borrow_and_update();
            }
            *phase = None;
        }
        std::future::pending().await
    }

    /// The next window hint. Never ends once the supervisor dropped the sender.
    async fn next_hint(hints: &mut Option<mpsc::UnboundedReceiver<WindowHint>>) -> WindowHint {
        if let Some(receiver) = hints {
            if let Some(hint) = receiver.recv().await {
                return hint;
            }
            *hints = None;
        }
        std::future::pending().await
    }

    /// The latest hint that came while no process ran.
    fn latest_hint(&mut self) -> Option<WindowHint> {
        let receiver = self.window_hints.as_mut()?;
        let mut latest = None;
        while let Ok(hint) = receiver.try_recv() {
            latest = Some(hint);
        }
        latest
    }
}

struct Supervisor {
    app: AppHandle,
    calls_dir: PathBuf,
    log_file: PathBuf,
    /// Open calls, oldest first.
    open_calls: Vec<Call>,
    active: Option<ActiveRecording>,
    /// Open calls that recorded in this app run, so their end needs a `call.mp4`.
    recorded_calls: Vec<PathBuf>,
}

impl Supervisor {
    async fn run(
        mut self,
        mut events: broadcast::Receiver<CallEvent>,
        mut stop_changes: mpsc::UnboundedReceiver<PathBuf>,
    ) {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => self.take_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::error!(
                            call.missed_events = missed,
                            "call capture missed call events; reading the open calls"
                        );
                        self.resync().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                Some(call_id) = stop_changes.recv() => self.take_stop_change(&call_id).await,
            }
            self.start_next();
        }
    }

    async fn take_event(&mut self, event: CallEvent) {
        match event {
            CallEvent::Started(call) | CallEvent::Resumed(call) => {
                // A replayed start of the recording call keeps its audio line.
                if self.is_active(&call.id) {
                    if let Some(open) = self.open_calls.iter_mut().find(|open| open.id == call.id) {
                        *open = call;
                    }
                    return;
                }
                self.open_calls.retain(|open| open.id != call.id);
                let state = self
                    .is_stopped_by_user(&call.id)
                    .then_some(AudioState::StoppedByUser);
                self.capture().set_state(&self.app, &call.id, state);
                self.open_calls.push(call);
            }
            CallEvent::Ended { call, .. } => {
                self.forget_call(&call.id).await;
            }
            CallEvent::WindowChanged(call) => self.follow_window(&call),
        }
    }

    /// Gives the call's video process the window the huddle watcher saw reopen, or starts one
    /// for a call that had no readable window before.
    fn follow_window(&mut self, call: &Call) {
        if let Some(open) = self.open_calls.iter_mut().find(|open| open.id == call.id) {
            *open = call.clone();
        }
        let Some(hint) = VideoProcess::is_for(call).cloned() else {
            return;
        };
        let folder = self.calls_dir.join(call_folder_name(&call.id));
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.call_id == call.id)
        else {
            return;
        };
        if let Some(hints) = &active.window_hints {
            // An error only means the video task ended with the recording.
            let _unheard = hints.send(hint);
            return;
        }
        let audio_segment = active.audio_phase.borrow().segment;
        let segment = VideoProcess::next_free_segment(&folder)
            .map_or(audio_segment, |free| free.max(audio_segment));
        let (hints, hint_receiver) = mpsc::unbounded_channel();
        let links = ProcessLinks {
            audio_phase_out: None,
            audio_phase_in: Some(active.audio_phase.clone()),
            window_hints: Some(hint_receiver),
        };
        active.window_hints = Some(hints);
        let process =
            spawn_process::<VideoProcess>(&self.app, &self.log_file, call, &folder, segment, links);
        if let Some(active) = self.active.as_mut() {
            active.processes.push(process);
        }
    }

    /// Matches the open calls to the call stream's, after missing some of its events.
    async fn resync(&mut self) {
        let current = self.app.state::<AppState>().calls.open_calls();
        let gone: Vec<PathBuf> = self
            .open_calls
            .iter()
            .filter(|open| !current.iter().any(|call| call.id == open.id))
            .map(|open| open.id.clone())
            .collect();
        for call_id in gone {
            self.forget_call(&call_id).await;
        }
        for call in current {
            if !self.open_calls.iter().any(|open| open.id == call.id) {
                self.take_event(CallEvent::Started(call)).await;
            }
        }
    }

    async fn forget_call(&mut self, call_id: &Path) {
        self.open_calls.retain(|open| open.id != call_id);
        self.stop_active_if(call_id).await;
        self.capture().set_state(&self.app, call_id, None);
        self.capture().set_video_state(&self.app, call_id, None);
        let was_recorded = self
            .recorded_calls
            .iter()
            .any(|recorded| recorded == call_id);
        self.recorded_calls.retain(|recorded| recorded != call_id);
        if was_recorded {
            // Its processes have stopped, so their segments are closed. Its own task, because a
            // long call takes minutes to write and the next call must not wait for it.
            let folder = self.calls_dir.join(call_folder_name(call_id));
            tauri::async_runtime::spawn(finalize_call(folder, self.log_file.clone()));
        }
    }

    /// The board took a stop or a resume of `call_id`.
    async fn take_stop_change(&mut self, call_id: &Path) {
        if !self.open_calls.iter().any(|call| call.id == call_id) {
            return;
        }
        if self.is_stopped_by_user(call_id) {
            self.stop_active_if(call_id).await;
            self.capture()
                .set_state(&self.app, call_id, Some(AudioState::StoppedByUser));
            self.capture().set_video_state(&self.app, call_id, None);
        } else if !self.is_active(call_id) {
            // `start_next` starts it at once with the call's next segment number.
            self.capture().set_state(&self.app, call_id, None);
        }
    }

    fn is_active(&self, call_id: &Path) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.call_id == call_id)
    }

    fn is_stopped_by_user(&self, call_id: &Path) -> bool {
        self.app
            .state::<AppState>()
            .calls
            .status()
            .is_stopped_by_user(call_id)
    }

    /// The capture state store, audio and video.
    fn capture(&self) -> &CallCapture {
        &self.app.state::<AppState>().inner().call_capture
    }

    async fn stop_active_if(&mut self, call_id: &Path) {
        if self.is_active(call_id)
            && let Some(active) = self.active.take()
        {
            let tasks: Vec<_> = active
                .processes
                .into_iter()
                .map(|RunningProcess { stop, task }| {
                    drop(stop);
                    task
                })
                .collect();
            for task in tasks {
                if let Err(error) = task.await {
                    tracing::error!(exception.message = %error, "a capture process task failed");
                }
            }
        }
    }

    /// Starts the oldest open call that is not stopped, when nothing records. Marks the other
    /// open calls as waiting.
    fn start_next(&mut self) {
        if self.active.is_none()
            && let Some(call) = self
                .open_calls
                .iter()
                .find(|call| !self.is_stopped_by_user(&call.id))
                .cloned()
        {
            let folder = self.calls_dir.join(call_folder_name(&call.id));
            let segment = next_call_segment(&folder).unwrap_or_else(|reason| {
                // The process's own check of the folder fails too, and restarts it.
                tracing::error!(exception.message = reason, "could not read the call folder");
                1
            });
            let (audio_phase_out, audio_phase) = watch::channel(AudioPhase {
                segment,
                is_asleep: false,
                sleep_count: 0,
                awake_since: jiff::Timestamp::now(),
            });
            let audio_links = ProcessLinks {
                audio_phase_out: Some(audio_phase_out),
                ..ProcessLinks::default()
            };
            let mut processes = vec![spawn_process::<AudioProcess>(
                &self.app,
                &self.log_file,
                &call,
                &folder,
                segment,
                audio_links,
            )];
            // A huddle without a window id has nothing that tells its window from the Slack
            // main window, which shows private messages.
            let mut window_hints = None;
            match call.platform {
                // Only on the Mac until finalize and the purge run on Windows: video recorded there
                // would never become a call.mp4 or be deleted.
                CallPlatform::Meet => {
                    if cfg!(target_os = "macos") {
                        processes.push(tab_video::spawn(
                            &self.app,
                            &call,
                            &folder,
                            audio_phase.clone(),
                        ));
                    }
                }
                CallPlatform::SlackHuddle => {}
            }
            if VideoProcess::is_for(&call).is_some() {
                let (hints, hint_receiver) = mpsc::unbounded_channel();
                let video_links = ProcessLinks {
                    audio_phase_out: None,
                    audio_phase_in: Some(audio_phase.clone()),
                    window_hints: Some(hint_receiver),
                };
                processes.push(spawn_process::<VideoProcess>(
                    &self.app,
                    &self.log_file,
                    &call,
                    &folder,
                    segment,
                    video_links,
                ));
                window_hints = Some(hints);
            }
            if !self.recorded_calls.contains(&call.id) {
                self.recorded_calls.push(call.id.clone());
            }
            self.active = Some(ActiveRecording {
                call_id: call.id,
                processes,
                audio_phase,
                window_hints,
            });
        }
        let active_id = self.active.as_ref().map(|active| active.call_id.clone());
        for call in &self.open_calls {
            if Some(&call.id) != active_id.as_ref() && !self.is_stopped_by_user(&call.id) {
                self.capture()
                    .set_state(&self.app, &call.id, Some(AudioState::Queued));
            }
        }
    }
}

fn spawn_process<P: CaptureProcess>(
    app: &AppHandle,
    log_file: &Path,
    call: &Call,
    folder: &Path,
    segment: u32,
    links: ProcessLinks,
) -> RunningProcess {
    let (stop, stop_receiver) = oneshot::channel();
    let recording = Recording::<P> {
        app: app.clone(),
        call: call.clone(),
        folder: folder.to_path_buf(),
        log_file: log_file.to_path_buf(),
        segment,
        backoff: RestartBackoff::default(),
        last_respawn_at: None,
        process: std::marker::PhantomData,
    };
    let span = tracing::info_span!(
        "capture.recording",
        call.id = %call.id.display(),
        process.command = P::SUBCOMMAND
    );
    let task = tauri::async_runtime::spawn(recording.run(stop_receiver, links).instrument(span));
    RunningProcess { stop, task }
}

/// The call folder's name: the call log's file name without its extension, which is unique per
/// call.
fn call_folder_name(call_id: &Path) -> String {
    call_id.file_stem().map_or_else(
        || "call".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// One above every segment any capture process used in the folder: in `audio.json` and the stem
/// files, and in `video.json` and the `NNN-video` folders.
fn next_call_segment(folder: &Path) -> Result<u32, String> {
    let from_audio = stem_writer::next_segment_number(folder)
        .map_err(|error| format!("could not read the call folder: {error}"))?;
    let from_video = video_capture::next_segment_number(folder)
        .map_err(|error| format!("could not read the call folder's video: {error}"))?;
    Ok(from_audio.max(from_video))
}

/// What the supervisor needs from one report line.
enum ReportSign {
    /// A stream delivered its first buffer into `segment`.
    Recording {
        segment: u32,
    },
    /// The process's counters for `segment`, summed. Growth shows it still works.
    Progress {
        segment: u32,
        counter_total: u64,
    },
    Rotated {
        to_segment: u32,
    },
    /// `segment` is closed and its manifest says so.
    Closed {
        segment: u32,
    },
    /// The process found Screen Recording refused at a segment or stream start. It holds no
    /// segment and waits for the next command.
    Refused,
    /// The user stopped video through the system's sharing controls. The process closes the
    /// segment and waits for the next command.
    StoppedByUser,
    /// A device did not open. Restarts back off further.
    DeviceOpenFailed,
    /// The Mac is about to sleep. Any open segment is already closed.
    Asleep,
    /// The Mac woke. The process waits for the next `start`.
    Woke,
    /// Nothing the supervisor acts on.
    Logged,
}

/// Where one process of a call's recording stands, for its tray row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessPhase {
    Starting {
        segment: u32,
    },
    Recording {
        segment: u32,
    },
    /// Recording went on into `segment` at a rotation.
    Rotated {
        segment: u32,
    },
    Restarting,
    Failing,
    Sleeping,
}

/// One kind of `mc-capture` child process: audio or video.
trait CaptureProcess: Send + 'static {
    type Command: Serialize + Send + Sync;
    type Report: DeserializeOwned;
    /// The `mc-capture` subcommand that runs it.
    const SUBCOMMAND: &'static str;

    /// `None` when the call has nothing for the process to record.
    fn start_command(
        call: &Call,
        folder: &Path,
        segment: u32,
        audio_segment: u32,
    ) -> Option<Self::Command>;
    fn stop_command() -> Self::Command;
    /// The command that hands a running process a new window, for a process that records one.
    fn window_command(hint: WindowHint) -> Option<Self::Command>;
    /// The lowest segment number the process may start in the folder.
    fn next_free_segment(folder: &Path) -> Result<u32, String>;
    /// The files whose growth shows the process still writes `segment`.
    fn segment_files(folder: &Path, segment: u32) -> Vec<PathBuf>;
    /// Logs the report, shows what only this process knows in the tray, and says what the
    /// report means for the supervisor.
    fn read_report(app: &AppHandle, call_id: &Path, report: &Self::Report) -> ReportSign;
    /// Shows the process's phase for the call in the tray, on its own row.
    fn show_phase(app: &AppHandle, call_id: &Path, phase: ProcessPhase);
}

struct AudioProcess;

impl CaptureProcess for AudioProcess {
    type Command = AudioCommand;
    type Report = AudioReport;
    const SUBCOMMAND: &'static str = "audio";

    fn start_command(
        call: &Call,
        folder: &Path,
        segment: u32,
        _audio_segment: u32,
    ) -> Option<AudioCommand> {
        Some(AudioCommand::Start {
            call_id: call.id.display().to_string(),
            folder: folder.to_path_buf(),
            segment,
            platform: match call.platform {
                CallPlatform::Meet => Platform::Meet,
                CallPlatform::SlackHuddle => Platform::Slack,
            },
            // Segment 1 is the call's start. A later one follows a wake, a rotation, a resume
            // or a helper restart, when a Bluetooth headset may be in the call already.
            mic_moment: if segment == 1 {
                MicMoment::CallStart
            } else {
                MicMoment::MidCall
            },
        })
    }

    fn stop_command() -> AudioCommand {
        AudioCommand::Stop
    }

    fn window_command(_hint: WindowHint) -> Option<AudioCommand> {
        None
    }

    fn next_free_segment(folder: &Path) -> Result<u32, String> {
        stem_writer::next_segment_number(folder)
            .map_err(|error| format!("could not read the call folder: {error}"))
    }

    fn segment_files(folder: &Path, segment: u32) -> Vec<PathBuf> {
        Stem::ALL
            .into_iter()
            .map(|stem| folder.join(stem_writer::stem_file_name(segment, stem)))
            .collect()
    }

    fn read_report(app: &AppHandle, call_id: &Path, report: &AudioReport) -> ReportSign {
        log_audio_report(call_id, report);
        let capture = &app.state::<AppState>().call_capture;
        match report {
            AudioReport::Recording { segment, .. } => ReportSign::Recording { segment: *segment },
            AudioReport::Heartbeat {
                segment,
                mic,
                remote,
            } => ReportSign::Progress {
                segment: *segment,
                counter_total: [mic, remote]
                    .into_iter()
                    .flatten()
                    .map(|counters| counters.samples_in.saturating_add(counters.samples_written))
                    .fold(0_u64, u64::saturating_add),
            },
            AudioReport::Rotated { to_segment, .. } => ReportSign::Rotated {
                to_segment: *to_segment,
            },
            AudioReport::Stopped { segment } => ReportSign::Closed { segment: *segment },
            AudioReport::Error {
                kind: AudioErrorKind::DeviceOpenFailed,
                ..
            } => ReportSign::DeviceOpenFailed,
            AudioReport::Permission { service, state } => {
                capture.set_permission(app, *service, *state);
                ReportSign::Logged
            }
            AudioReport::SystemSleep { .. } => ReportSign::Asleep,
            AudioReport::SystemWake => ReportSign::Woke,
            AudioReport::MicUnavailable { segment, .. } => {
                capture.set_state(
                    app,
                    call_id,
                    Some(AudioState::RecordingWithoutMic { segment: *segment }),
                );
                ReportSign::Logged
            }
            AudioReport::DeviceChanged {
                segment,
                stream: Stem::Mic,
                ..
            } => {
                // The mic moved to another device, which ends a time without one.
                capture.set_state(
                    app,
                    call_id,
                    Some(AudioState::Recording { segment: *segment }),
                );
                ReportSign::Logged
            }
            AudioReport::Gap { .. }
            | AudioReport::DeviceChanged {
                stream: Stem::Remote,
                ..
            }
            | AudioReport::Error { .. } => ReportSign::Logged,
        }
    }

    fn show_phase(app: &AppHandle, call_id: &Path, phase: ProcessPhase) {
        let state = match phase {
            ProcessPhase::Starting { segment } => AudioState::Starting { segment },
            ProcessPhase::Recording { segment } => {
                audio_recording_state(app, call_id, segment, false)
            }
            ProcessPhase::Rotated { segment } => audio_recording_state(app, call_id, segment, true),
            ProcessPhase::Restarting => AudioState::Restarting,
            ProcessPhase::Failing => AudioState::Failing,
            ProcessPhase::Sleeping => AudioState::Sleeping,
        };
        app.state::<AppState>()
            .call_capture
            .set_state(app, call_id, Some(state));
    }
}

/// The state for stems open in `segment`. A segment that recorded without the user's mic stays
/// so when its remote stem starts, and across a rotation, until the mic returns.
fn audio_recording_state(
    app: &AppHandle,
    call_id: &Path,
    segment: u32,
    is_rotation: bool,
) -> AudioState {
    let shown = app
        .state::<AppState>()
        .call_capture
        .states()
        .get(call_id)
        .copied();
    let is_without_mic = matches!(
        shown,
        Some(AudioState::RecordingWithoutMic { segment: shown_segment })
            if is_rotation || shown_segment == segment
    );
    if is_without_mic {
        AudioState::RecordingWithoutMic { segment }
    } else {
        AudioState::Recording { segment }
    }
}

struct VideoProcess;

impl VideoProcess {
    /// The window the call's video records: a huddle's, once the watcher read its id.
    fn is_for(call: &Call) -> Option<&WindowHint> {
        match call.platform {
            CallPlatform::SlackHuddle => call.window_hint.as_ref(),
            CallPlatform::Meet => None,
        }
    }

    fn show(app: &AppHandle, call_id: &Path, state: VideoState) {
        app.state::<AppState>()
            .call_capture
            .set_video_state(app, call_id, Some(state));
    }
}

impl CaptureProcess for VideoProcess {
    type Command = VideoCommand;
    type Report = VideoReport;
    const SUBCOMMAND: &'static str = "video";

    fn start_command(
        call: &Call,
        folder: &Path,
        segment: u32,
        audio_segment: u32,
    ) -> Option<VideoCommand> {
        let window_hint = Self::is_for(call)?.clone();
        Some(VideoCommand::Start {
            call_id: call.id.display().to_string(),
            folder: folder.to_path_buf(),
            segment,
            audio_segment,
            platform: match call.platform {
                CallPlatform::Meet => Platform::Meet,
                CallPlatform::SlackHuddle => Platform::Slack,
            },
            window_hint,
        })
    }

    fn stop_command() -> VideoCommand {
        VideoCommand::Stop
    }

    fn window_command(hint: WindowHint) -> Option<VideoCommand> {
        Some(VideoCommand::Window { window_hint: hint })
    }

    fn next_free_segment(folder: &Path) -> Result<u32, String> {
        video_capture::next_segment_number(folder)
            .map_err(|error| format!("could not read the call folder's video: {error}"))
    }

    fn segment_files(folder: &Path, segment: u32) -> Vec<PathBuf> {
        vec![
            folder
                .join(video_capture::segment_folder_name(segment))
                .join(video_capture::PLAYLIST_FILE_NAME),
        ]
    }

    fn read_report(app: &AppHandle, call_id: &Path, report: &VideoReport) -> ReportSign {
        log_video_report(call_id, report);
        match report {
            VideoReport::Recording { segment, .. } => ReportSign::Recording { segment: *segment },
            VideoReport::Heartbeat {
                segment,
                frames_output,
                fragments_written,
                window_lookups,
                ..
            } => ReportSign::Progress {
                segment: *segment,
                // Frames handed to the encoder still count up when VideoToolbox hangs, so only
                // its output counts. Lookups count, so a process that looks for a closed or
                // hidden window is not hung.
                counter_total: [frames_output, fragments_written, window_lookups]
                    .into_iter()
                    .fold(0_u64, |total, counter| total.saturating_add(*counter)),
            },
            VideoReport::Stopped { segment } => ReportSign::Closed { segment: *segment },
            VideoReport::WindowChanged { segment, cause, .. } => {
                match cause {
                    WindowChangeCause::Closed => Self::show(
                        app,
                        call_id,
                        VideoState::WindowMissing { segment: *segment },
                    ),
                    WindowChangeCause::Returned | WindowChangeCause::Replaced => {
                        Self::show(app, call_id, VideoState::Recording { segment: *segment });
                    }
                    WindowChangeCause::Resized => {}
                }
                ReportSign::Logged
            }
            // The process waits for the next command with no segment open, so nothing grows.
            VideoReport::Error {
                kind: VideoErrorKind::PermissionDenied,
                ..
            } => {
                Self::show(app, call_id, VideoState::ScreenRecordingRefused);
                ReportSign::Refused
            }
            VideoReport::Error {
                kind: VideoErrorKind::StoppedByUser,
                ..
            } => {
                Self::show(app, call_id, VideoState::StoppedFromSharingControls);
                ReportSign::StoppedByUser
            }
            VideoReport::Gap { .. }
            | VideoReport::Permission { .. }
            | VideoReport::Error { .. } => ReportSign::Logged,
        }
    }

    fn show_phase(app: &AppHandle, call_id: &Path, phase: ProcessPhase) {
        let state = match phase {
            ProcessPhase::Starting { segment } => VideoState::Starting { segment },
            ProcessPhase::Recording { segment } | ProcessPhase::Rotated { segment } => {
                VideoState::Recording { segment }
            }
            ProcessPhase::Restarting => VideoState::Restarting,
            ProcessPhase::Failing => VideoState::Failing,
            ProcessPhase::Sleeping => VideoState::Sleeping,
        };
        Self::show(app, call_id, state);
    }
}

fn log_video_report(call_id: &Path, report: &VideoReport) {
    let call_id = call_id.display().to_string();
    match report {
        VideoReport::Recording {
            segment,
            window,
            width_px,
            height_px,
            codec,
            first_host_time_ns,
        } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            video.window_id = window.window_id,
            video.width_px = width_px,
            video.height_px = height_px,
            video.codec = %codec,
            video.first_host_time_ns = first_host_time_ns,
            "video recording"
        ),
        VideoReport::Gap {
            segment,
            length_ms,
            cause,
            ..
        } => tracing::warn!(
            call.id = call_id,
            segment.number = segment,
            video.gap_length_ms = length_ms,
            error.type = %cause,
            "video gap"
        ),
        VideoReport::WindowChanged {
            segment,
            cause,
            window,
            ..
        } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            video.change_cause = %cause,
            video.window_id = window.as_ref().map(|window| window.window_id),
            "video window changed"
        ),
        VideoReport::Permission { service, state } => tracing::info!(
            call.id = call_id,
            permission.service = %service,
            permission.state = %state,
            "permission checked"
        ),
        VideoReport::Heartbeat { segment, .. } => tracing::debug!(
            call.id = call_id,
            segment.number = segment,
            "video heartbeat"
        ),
        VideoReport::Stopped { segment } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            "video segment closed"
        ),
        VideoReport::Error { kind, message } => tracing::error!(
            call.id = call_id,
            error.type = %kind,
            exception.message = message,
            "video helper error"
        ),
    }
}

fn log_mic_unavailable(
    call_id: &str,
    segment: u32,
    old_device: Option<&str>,
    cause: Option<DeviceChangeCause>,
    message: &str,
) {
    tracing::warn!(
        call.id = call_id,
        segment.number = segment,
        audio.previous_device = old_device,
        audio.change_cause = cause.map(|cause| cause.to_string()),
        exception.message = message,
        "audio mic unavailable"
    );
}

fn log_audio_report(call_id: &Path, report: &AudioReport) {
    let call_id = call_id.display().to_string();
    match report {
        AudioReport::Recording {
            segment,
            stream,
            device,
            rate_hz,
            first_host_time_ns,
        } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            audio.stream = %stream,
            audio.device = device,
            audio.rate_hz = rate_hz,
            audio.first_host_time_ns = first_host_time_ns,
            "audio stream recording"
        ),
        AudioReport::Gap {
            segment,
            stream,
            start_ms,
            length_ms,
            cause,
            ..
        } => tracing::warn!(
            call.id = call_id,
            segment.number = segment,
            audio.stream = %stream,
            audio.gap_start_ms = start_ms,
            audio.gap_length_ms = length_ms,
            error.type = %cause,
            "audio gap"
        ),
        AudioReport::Heartbeat { segment, .. } => tracing::debug!(
            call.id = call_id,
            segment.number = segment,
            "audio heartbeat"
        ),
        AudioReport::Rotated {
            from_segment,
            to_segment,
        } => tracing::info!(
            call.id = call_id,
            segment.number = to_segment,
            segment.previous_number = from_segment,
            "audio segment rotated"
        ),
        AudioReport::Stopped { segment } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            "audio segment closed"
        ),
        AudioReport::DeviceChanged {
            segment,
            stream,
            old_device,
            new_device,
            rate_hz,
            cause,
        } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            audio.stream = %stream,
            audio.previous_device = old_device.as_deref(),
            audio.device = new_device,
            audio.rate_hz = rate_hz,
            audio.change_cause = %cause,
            "audio device changed"
        ),
        AudioReport::MicUnavailable {
            segment,
            old_device,
            cause,
            message,
        } => log_mic_unavailable(&call_id, *segment, old_device.as_deref(), *cause, message),
        AudioReport::Permission { service, state } => tracing::info!(
            call.id = call_id,
            permission.service = %service,
            permission.state = %state,
            "permission checked"
        ),
        AudioReport::SystemSleep { segment } => tracing::info!(
            call.id = call_id,
            segment.number = segment,
            "the Mac is going to sleep"
        ),
        AudioReport::SystemWake => tracing::info!(call.id = call_id, "the Mac woke"),
        AudioReport::Error { kind, message } => tracing::error!(
            call.id = call_id,
            error.type = %kind,
            exception.message = message,
            "audio helper error"
        ),
    }
}

/// One process of one call's recording, restarted until the call stops.
struct Recording<P: CaptureProcess> {
    app: AppHandle,
    call: Call,
    folder: PathBuf,
    log_file: PathBuf,
    /// The segment the next start uses, or the last one started or rotated to.
    segment: u32,
    backoff: RestartBackoff,
    /// When the last respawn for a Screen Recording check started its new process.
    last_respawn_at: Option<Instant>,
    process: std::marker::PhantomData<P>,
}

/// How one run of the process ended.
enum ProcessEnd {
    /// The supervisor asked it to stop.
    Stopped,
    /// It exited, hung or never started. `reason` is for the log.
    Failed {
        reason: String,
        is_device_open_failed: bool,
    },
    /// The supervisor closed it to retry after a Screen Recording refusal. macOS may cache the
    /// preflight's answer per process, so only a new process sees a grant given since.
    Respawn,
}

/// What a run does after it took an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunNext {
    Continue,
    /// End this process and start a new one, at least `RESPAWN_GAP` after the last respawn.
    Respawn,
}

impl<P: CaptureProcess> Recording<P> {
    async fn run(mut self, mut stop: oneshot::Receiver<()>, mut links: ProcessLinks) {
        loop {
            // Restarts use the latest window, never one the watcher saw close.
            if let Some(hint) = links.latest_hint() {
                self.call.window_hint = Some(hint);
            }
            let end = self.run_once(&mut stop, &mut links).await;
            let (reason, is_device_open_failed) = match end {
                ProcessEnd::Stopped => {
                    tracing::info!(call.id = %self.call.id.display(), "capture process stopped");
                    return;
                }
                ProcessEnd::Respawn => {
                    // A stop sent while the old process wound down ends the call here.
                    if !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                        tracing::info!(call.id = %self.call.id.display(), "capture process stopped");
                        return;
                    }
                    let gap = self.last_respawn_at.map_or(Duration::ZERO, |at| {
                        RESPAWN_GAP.saturating_sub(at.elapsed())
                    });
                    tracing::info!(
                        call.id = %self.call.id.display(),
                        segment.number = self.segment,
                        capture.restart_delay_ms = u64::try_from(gap.as_millis()).unwrap_or(u64::MAX),
                        "restarting the capture process for a new Screen Recording check"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(gap) => {}
                        _ = &mut stop => return,
                    }
                    self.last_respawn_at = Some(Instant::now());
                    continue;
                }
                ProcessEnd::Failed {
                    reason,
                    is_device_open_failed,
                } => (reason, is_device_open_failed),
            };
            let restart = self
                .backoff
                .note_failure(Instant::now(), is_device_open_failed);
            tracing::warn!(
                call.id = %self.call.id.display(),
                error.type = "capture_process_failed",
                exception.message = reason,
                capture.failures_in_window = restart.failures_in_window,
                capture.is_failing = restart.is_failing,
                capture.restart_delay_ms =
                    u64::try_from(restart.delay.as_millis()).unwrap_or(u64::MAX),
                "a capture process failed; restarting"
            );
            if restart.is_newly_failing {
                crate::notify(
                    &self.app,
                    "Recording failed",
                    &format!(
                        "The capture helper failed {FAILURES_SHOWN} times in a minute. It keeps \
                         retrying. Last error: {reason}"
                    ),
                );
            }
            let phase = if restart.is_failing {
                ProcessPhase::Failing
            } else {
                ProcessPhase::Restarting
            };
            P::show_phase(&self.app, &self.call.id, phase);
            // A device that would not open may come back, as a headset that reconnects: a
            // device change ends the long wait early.
            let is_device_watched =
                cfg!(target_os = "macos") && is_device_open_failed && restart.is_failing;
            let device_changed = async {
                if is_device_watched && await_device_change(restart.delay).await {
                    tokio::time::sleep(DEVICE_CHANGE_SETTLE).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            // Stopping and resuming the call starts a new recording at once.
            tokio::select! {
                () = tokio::time::sleep(restart.delay) => {}
                () = device_changed => {
                    tracing::info!(
                        call.id = %self.call.id.display(),
                        "a device changed; restarting early"
                    );
                }
                _ = &mut stop => return,
            }
            // A run that left no files gives its number to the next.
            match P::next_free_segment(&self.folder) {
                Ok(free) => self.segment = self.segment.max(free),
                Err(reason) => {
                    tracing::warn!(exception.message = reason, "kept the segment number");
                }
            }
        }
    }

    /// Runs the process once, until the supervisor stops it, or it exits or hangs.
    async fn run_once(
        &mut self,
        stop: &mut oneshot::Receiver<()>,
        links: &mut ProcessLinks,
    ) -> ProcessEnd {
        let (mut child, stdout, mut run) = match self.open_run(links).await {
            Ok(opened) => opened,
            Err(reason) => return failed(reason),
        };
        let pid = child.id().unwrap_or_default();
        let mut reports = BufReader::new(stdout).lines();
        let mut check = tokio::time::interval(LIVENESS_CHECK_INTERVAL);
        check.tick().await;
        let mut stop_deadline: Option<tokio::time::Instant> = None;
        loop {
            // A future that never ends until a stop sets the deadline.
            let stop_limit = async {
                match stop_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                line = reports.next_line() => match line {
                    Ok(Some(line)) => {
                        let audio_out = links.audio_phase_out.as_ref();
                        if let Err(error) = self.follow_report(&line, &mut run, audio_out).await {
                            kill(&mut child).await;
                            return failed(format!("could not send start after a wake: {error}"));
                        }
                    }
                    Ok(None) => {
                        let Some(deadline) = stop_deadline else {
                            let status = child.wait().await;
                            log_exit(pid, status.as_ref());
                            return ProcessEnd::Failed {
                                reason: exit_reason(status.as_ref()),
                                is_device_open_failed: run.is_device_open_failed,
                            };
                        };
                        Self::wait_stopped(
                            self.segment,
                            &mut child,
                            deadline,
                            run.has_closed_segment,
                        )
                        .await;
                        return ProcessEnd::Stopped;
                    }
                    Err(error) => {
                        kill(&mut child).await;
                        if stop_deadline.is_some() {
                            return ProcessEnd::Stopped;
                        }
                        return failed(format!("could not read its stdout: {error}"));
                    }
                },
                _ = &mut *stop, if stop_deadline.is_none() => {
                    run.send_stop::<P>().await;
                    stop_deadline = Some(tokio::time::Instant::now() + STOP_LIMIT);
                }
                phase = ProcessLinks::next_audio_phase(&mut links.audio_phase_in),
                    if stop_deadline.is_none() =>
                {
                    let followed = self.follow_audio(phase, &mut run).await;
                    let failure = "could not follow the audio's sleep";
                    if let Some(end) =
                        Self::end_after(self.segment, followed, failure, &mut child, &mut run).await
                    {
                        return end;
                    }
                }
                hint = ProcessLinks::next_hint(&mut links.window_hints),
                    if stop_deadline.is_none() =>
                {
                    let audio_phase = links.audio_phase_now();
                    let followed = self.follow_hint(hint, &mut run, audio_phase).await;
                    let failure = "could not send the new window";
                    if let Some(end) =
                        Self::end_after(self.segment, followed, failure, &mut child, &mut run).await
                    {
                        return end;
                    }
                }
                () = stop_limit => {
                    tracing::warn!(
                        process.pid = pid,
                        "the capture process did not stop in time; killing it"
                    );
                    kill(&mut child).await;
                    return ProcessEnd::Stopped;
                }
                // Nothing grows while the process waits for a wake.
                _ = check.tick(), if stop_deadline.is_none() && run.open_segment.is_some() => {
                    let files = P::segment_files(&self.folder, run.liveness.segment);
                    if run.liveness.is_hung(&files) {
                        kill(&mut child).await;
                        return failed(format!(
                            "hung: no counter moved and no file grew for {} s",
                            HANG_LIMIT.as_secs()
                        ));
                    }
                }
            }
        }
    }

    /// Launches the process for the current segment, as the audio phase allows.
    async fn open_run(
        &mut self,
        links: &mut ProcessLinks,
    ) -> Result<(Child, ChildStdout, ProcessRun), String> {
        let audio_phase = links.audio_phase();
        // A video process that starts while the Mac sleeps waits for the audio's wake segment.
        let is_sleeping = audio_phase.is_some_and(|phase| phase.is_asleep);
        let audio_segment = audio_phase.map_or(self.segment, |phase| phase.segment);
        let (child, stdin, stdout) = self.launch(is_sleeping, audio_segment).await?;
        // An audio start opens a segment, after a sleep too: its helper died while the Mac slept.
        let segment = self.segment;
        let started_at = jiff::Timestamp::now();
        links.publish_audio(|phase| {
            phase.segment = segment;
            phase.is_asleep = false;
            phase.awake_since = started_at;
        });
        let run = ProcessRun {
            stdin: Some(stdin),
            liveness: Liveness::new(segment),
            open_segment: (!is_sleeping).then_some(segment),
            has_closed_segment: false,
            is_device_open_failed: false,
            next_command_seq: 2,
            hold: if is_sleeping {
                RunHold::Asleep
            } else {
                RunHold::Free
            },
            seen_sleep_count: audio_phase.map_or(0, |phase| phase.sleep_count),
            has_refusal: false,
        };
        Ok((child, stdout, run))
    }

    /// Waits for a stopped process that closed its stdout to exit. One that never exits is
    /// killed at the stop limit.
    async fn wait_stopped(
        segment: u32,
        child: &mut Child,
        deadline: tokio::time::Instant,
        has_closed_segment: bool,
    ) {
        let pid = child.id().unwrap_or_default();
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(status) => log_exit(pid, status.as_ref()),
            Err(_elapsed) => {
                tracing::warn!(
                    process.pid = pid,
                    "the capture process did not exit in time; killing it"
                );
                kill(child).await;
            }
        }
        if !has_closed_segment {
            tracing::warn!(
                process.pid = pid,
                segment.number = segment,
                "the capture process stopped without reporting its segment closed"
            );
        }
    }

    /// Starts the process and sends it `start` for the current segment, unless the Mac sleeps:
    /// then the start waits for the wake.
    async fn launch(
        &mut self,
        is_sleeping: bool,
        audio_segment: u32,
    ) -> Result<(Child, ChildStdin, ChildStdout), String> {
        let segment = self.segment;
        let start = P::start_command(&self.call, &self.folder, segment, audio_segment)
            .ok_or_else(|| "the call has nothing for this process to record".to_owned())?;
        let mut child = self.spawn_sidecar()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill(&mut child).await;
            return Err("the helper has no stdin or stdout pipe".to_owned());
        };
        tracing::info!(
            call.id = %self.call.id.display(),
            process.pid = child.id().unwrap_or_default(),
            segment.number = segment,
            file.directory = %self.folder.display(),
            capture.is_sleeping = is_sleeping,
            "capture process started"
        );
        if is_sleeping {
            P::show_phase(&self.app, &self.call.id, ProcessPhase::Sleeping);
            return Ok((child, stdin, stdout));
        }
        P::show_phase(&self.app, &self.call.id, ProcessPhase::Starting { segment });
        if let Err(error) = send_command(&mut stdin, 1, &start).await {
            kill(&mut child).await;
            return Err(format!("could not send start: {error}"));
        }
        Ok((child, stdin, stdout))
    }

    // No setsid and no new process group: a new session cut the inherited mic grant in another
    // app, and Core Audio then delivered zeros with no error.
    fn spawn_sidecar(&self) -> Result<Child, String> {
        let helper = sidecar_path()?;
        rotate_log_if_full(&self.log_file);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
            .map_err(|error| format!("could not open {}: {error}", self.log_file.display()))?;
        Command::new(&helper)
            .arg(P::SUBCOMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", helper.display()))
    }

    fn take_report(&mut self, line: &str, liveness: &mut Liveness) -> Option<ReportSign> {
        let report = match capture_protocol::decode::<P::Report>(line) {
            Ok(report) => report.message,
            Err(error) => {
                tracing::warn!(
                    exception.message = %error,
                    "ignored a helper line that is not valid"
                );
                return None;
            }
        };
        let sign = P::read_report(&self.app, &self.call.id, &report);
        match sign {
            ReportSign::Recording { segment } => {
                self.backoff.note_recording();
                P::show_phase(
                    &self.app,
                    &self.call.id,
                    ProcessPhase::Recording { segment },
                );
            }
            ReportSign::Progress {
                segment,
                counter_total,
            } => liveness.note_progress(segment, counter_total),
            ReportSign::Rotated { to_segment } => {
                self.segment = self.segment.max(to_segment);
                liveness.segment = to_segment;
                P::show_phase(
                    &self.app,
                    &self.call.id,
                    ProcessPhase::Rotated {
                        segment: to_segment,
                    },
                );
            }
            ReportSign::Asleep => P::show_phase(&self.app, &self.call.id, ProcessPhase::Sleeping),
            ReportSign::Closed { .. }
            | ReportSign::Refused
            | ReportSign::StoppedByUser
            | ReportSign::DeviceOpenFailed
            | ReportSign::Woke
            | ReportSign::Logged => {}
        }
        Some(sign)
    }

    /// Takes one report line into the run, and publishes the audio phase it shows. Fails only
    /// when a wake's `start` cannot be sent.
    async fn follow_report(
        &mut self,
        line: &str,
        run: &mut ProcessRun,
        audio_out: Option<&watch::Sender<AudioPhase>>,
    ) -> std::io::Result<()> {
        let publish = |change: &dyn Fn(&mut AudioPhase)| {
            if let Some(phase) = audio_out {
                phase.send_modify(change);
            }
        };
        match self.take_report(line, &mut run.liveness) {
            // A sleep's stop can report after the wake's start opened the next segment.
            Some(ReportSign::Closed { segment }) if run.open_segment == Some(segment) => {
                run.has_closed_segment = true;
                run.open_segment = None;
            }
            Some(ReportSign::Refused) => {
                run.has_closed_segment = true;
                run.open_segment = None;
                run.has_refusal = true;
                // A sleep that came after the start keeps its hold, so the wake still starts.
                if run.hold == RunHold::Free {
                    run.hold = RunHold::Refused;
                }
            }
            Some(ReportSign::StoppedByUser) => run.hold = RunHold::StoppedByUser,
            Some(ReportSign::DeviceOpenFailed) => run.is_device_open_failed = true,
            Some(ReportSign::Recording { segment }) => run.open_segment = Some(segment),
            Some(ReportSign::Rotated {
                to_segment: segment,
            }) => {
                run.open_segment = Some(segment);
                publish(&|phase| phase.segment = segment);
            }
            Some(ReportSign::Asleep) => publish(&|phase| {
                phase.is_asleep = true;
                phase.sleep_count += 1;
            }),
            // stdin is gone once a stop went out: then the wake starts nothing.
            Some(ReportSign::Woke) if run.open_segment.is_none() => {
                let segment = self.next_segment_after_sleep();
                if self.send_start(segment, segment, run).await? {
                    let woke_at = jiff::Timestamp::now();
                    publish(&|phase| {
                        phase.segment = segment;
                        phase.is_asleep = false;
                        phase.awake_since = woke_at;
                    });
                }
            }
            Some(
                ReportSign::Closed { .. }
                | ReportSign::Progress { .. }
                | ReportSign::Woke
                | ReportSign::Logged,
            )
            | None => {}
        }
        Ok(())
    }

    /// Sends `start` for `segment` to a process that waits with no segment open. False once a
    /// stop went out, which took stdin.
    async fn send_start(
        &mut self,
        segment: u32,
        audio_segment: u32,
        run: &mut ProcessRun,
    ) -> std::io::Result<bool> {
        let Some(stdin) = run.stdin.as_mut() else {
            return Ok(false);
        };
        let Some(start) = P::start_command(&self.call, &self.folder, segment, audio_segment) else {
            return Ok(false);
        };
        send_command(stdin, run.next_command_seq, &start).await?;
        tracing::info!(segment.number = segment, "started the next segment");
        run.next_command_seq += 1;
        run.open_segment = Some(segment);
        run.has_closed_segment = false;
        run.liveness = Liveness::new(segment);
        self.segment = segment;
        P::show_phase(&self.app, &self.call.id, ProcessPhase::Starting { segment });
        Ok(true)
    }

    /// The video side of a sleep: the segment stops at the audio's sleep report, and the next
    /// starts at its wake, on the audio's wake segment.
    async fn follow_audio(
        &mut self,
        phase: AudioPhase,
        run: &mut ProcessRun,
    ) -> std::io::Result<RunNext> {
        let is_new_sleep = phase.sleep_count != run.seen_sleep_count;
        run.seen_sleep_count = phase.sleep_count;
        // After the user's stop no segment is open, and the tray keeps saying who stopped it. A
        // refused run sleeps too, so its wake tries again.
        if is_new_sleep && matches!(run.hold, RunHold::Free | RunHold::Refused) {
            run.hold = RunHold::Asleep;
            if run.open_segment.is_some()
                && let Some(stdin) = run.stdin.as_mut()
            {
                send_command(stdin, run.next_command_seq, &P::stop_command()).await?;
                run.next_command_seq += 1;
                tracing::info!(
                    segment.number = run.open_segment,
                    "stopped the segment for a sleep"
                );
            }
            P::show_phase(&self.app, &self.call.id, ProcessPhase::Sleeping);
        }
        if run.hold == RunHold::Asleep && !phase.is_asleep {
            // The audio's wake segment is above every number in the folder, video's included.
            let segment = P::next_free_segment(&self.folder)
                .map_or(phase.segment, |free| free.max(phase.segment));
            if run.has_refusal {
                self.segment = segment;
                return Ok(RunNext::Respawn);
            }
            if self.send_start(segment, phase.segment, run).await? {
                run.hold = RunHold::Free;
            }
        }
        Ok(RunNext::Continue)
    }

    /// Keeps the huddle watcher's new window for restarts, and hands it to an open segment. A
    /// run whose last start found Screen Recording refused respawns on it instead.
    async fn follow_hint(
        &mut self,
        hint: WindowHint,
        run: &mut ProcessRun,
        audio_phase: Option<AudioPhase>,
    ) -> std::io::Result<RunNext> {
        tracing::info!(
            video.window_id = hint.window_id,
            "the call's window changed"
        );
        self.call.window_hint = Some(hint.clone());
        if run.hold == RunHold::Refused {
            // While the Mac sleeps the wake respawns, on the audio's wake segment.
            if audio_phase.is_some_and(|phase| phase.is_asleep) {
                return Ok(RunNext::Continue);
            }
            let audio_segment = audio_phase.map_or(self.segment, |phase| phase.segment);
            self.segment = P::next_free_segment(&self.folder)
                .map_or(audio_segment, |free| free.max(audio_segment));
            return Ok(RunNext::Respawn);
        }
        // A segment that starts later takes the hint from the call.
        if run.open_segment.is_none() || run.hold != RunHold::Free {
            return Ok(RunNext::Continue);
        }
        let (Some(stdin), Some(command)) = (run.stdin.as_mut(), P::window_command(hint)) else {
            return Ok(RunNext::Continue);
        };
        send_command(stdin, run.next_command_seq, &command).await?;
        run.next_command_seq += 1;
        Ok(RunNext::Continue)
    }

    /// How a followed event ends the process, if it does: a respawn, or a failure to write to it.
    async fn end_after(
        segment: u32,
        followed: std::io::Result<RunNext>,
        failure: &str,
        child: &mut Child,
        run: &mut ProcessRun,
    ) -> Option<ProcessEnd> {
        match followed {
            Ok(RunNext::Continue) => None,
            Ok(RunNext::Respawn) => Some(Self::respawn(segment, child, run).await),
            Err(error) => {
                kill(child).await;
                Some(failed(format!("{failure}: {error}")))
            }
        }
    }

    /// Closes stdin of a process that holds no segment, so it exits, and waits for it. The run
    /// loop then starts a new process.
    async fn respawn(segment: u32, child: &mut Child, run: &mut ProcessRun) -> ProcessEnd {
        drop(run.stdin.take());
        let deadline = tokio::time::Instant::now() + STOP_LIMIT;
        Self::wait_stopped(segment, child, deadline, run.has_closed_segment).await;
        ProcessEnd::Respawn
    }

    /// The call's next segment number after a sleep closed this one. Every capture process of
    /// the call shares the number, so it comes from all their files.
    fn next_segment_after_sleep(&self) -> u32 {
        let after_this = self.segment + 1;
        match next_call_segment(&self.folder) {
            Ok(free) => after_this.max(free),
            Err(reason) => {
                tracing::warn!(
                    exception.message = reason,
                    "took the segment after this one"
                );
                after_this
            }
        }
    }
}

/// What one run of the process has done so far.
struct ProcessRun {
    /// `None` once a stop went out.
    stdin: Option<ChildStdin>,
    liveness: Liveness,
    /// `None` between a sleep's close and the next start: the process waits for the wake.
    open_segment: Option<u32>,
    has_closed_segment: bool,
    is_device_open_failed: bool,
    /// `start` went out as 1.
    next_command_seq: u64,
    hold: RunHold,
    /// The audio phase's sleep count when this run last read it.
    seen_sleep_count: u64,
    /// The process reported a Screen Recording refusal. Its next start, at a wake or a new
    /// window, goes to a new process instead.
    has_refusal: bool,
}

/// What keeps a video run from recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunHold {
    /// Nothing: it records, or records at its next `start`.
    Free,
    /// The process closed its segment for a sleep, or started while the Mac slept, and waits
    /// for the audio's wake.
    Asleep,
    /// The user stopped video through the system's sharing controls. Wakes and window hints
    /// start nothing until the call resumes, which starts a new run.
    StoppedByUser,
    /// macOS refused Screen Recording at the last start, so no segment is open. The next wake
    /// or window hint starts a new process, whose preflight checks again.
    Refused,
}

impl ProcessRun {
    /// Sends `stop` to an open segment, then closes stdin: after the segment closes, the
    /// process exits. With no segment open, or one a sleep already stopped, closing stdin alone
    /// ends it.
    async fn send_stop<P: CaptureProcess>(&mut self) {
        let Some(mut stdin) = self.stdin.take() else {
            return;
        };
        if self.open_segment.is_some()
            && self.hold == RunHold::Free
            && let Err(error) =
                send_command(&mut stdin, self.next_command_seq, &P::stop_command()).await
        {
            tracing::warn!(exception.message = %error, "could not send stop; closing stdin");
        }
        drop(stdin);
    }
}

fn failed(reason: String) -> ProcessEnd {
    ProcessEnd::Failed {
        reason,
        is_device_open_failed: false,
    }
}

/// The two signs a process still works: its heartbeat counters and its files. It is hung only
/// when both stand still for `HANG_LIMIT`.
struct Liveness {
    segment: u32,
    counter_total: u64,
    counters_moved_at: Instant,
    file_bytes: u64,
    files_grew_at: Instant,
}

impl Liveness {
    fn new(segment: u32) -> Self {
        let now = Instant::now();
        Self {
            segment,
            counter_total: 0,
            counters_moved_at: now,
            file_bytes: 0,
            files_grew_at: now,
        }
    }

    fn note_progress(&mut self, segment: u32, counter_total: u64) {
        if segment != self.segment || counter_total > self.counter_total {
            self.counters_moved_at = Instant::now();
        }
        self.segment = segment;
        self.counter_total = counter_total;
    }

    fn is_hung(&mut self, files: &[PathBuf]) -> bool {
        let file_bytes = files
            .iter()
            .filter_map(|file| std::fs::metadata(file).ok())
            .map(|metadata| metadata.len())
            .sum();
        let now = Instant::now();
        if file_bytes != self.file_bytes {
            self.file_bytes = file_bytes;
            self.files_grew_at = now;
        }
        now.duration_since(self.counters_moved_at) >= HANG_LIMIT
            && now.duration_since(self.files_grew_at) >= HANG_LIMIT
    }
}

/// Moves a full helper log to `<name>.1`, replacing the one before.
fn rotate_log_if_full(log_file: &Path) {
    let is_full =
        std::fs::metadata(log_file).is_ok_and(|metadata| metadata.len() >= SIDECAR_LOG_LIMIT_BYTES);
    if !is_full {
        return;
    }
    let mut older = log_file.as_os_str().to_owned();
    older.push(".1");
    if let Err(error) = std::fs::rename(log_file, &older) {
        tracing::warn!(
            file.path = %log_file.display(),
            exception.message = %error,
            "could not rotate the helper log"
        );
    }
}

/// `mc-capture` beside the app's executable: `Contents/MacOS/` in the bundle, where Tauri puts
/// the sidecar, or `target/<profile>/` in a dev build.
fn sidecar_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not find the app executable: {error}"))?;
    let helper = executable
        .parent()
        .map(|folder| folder.join(SIDECAR_FILE_NAME))
        .ok_or_else(|| "the app executable has no folder".to_owned())?;
    if helper.is_file() {
        Ok(helper)
    } else {
        Err(format!(
            "{} is missing; build it with `cargo build -p mc-capture`",
            helper.display()
        ))
    }
}

/// Waits up to `limit` for the default devices or the device list to change, through the
/// helper's `await-device-change`, so the app links no Core Audio. False when the time ran
/// out or the helper could not watch.
async fn await_device_change(limit: Duration) -> bool {
    #[derive(serde::Deserialize)]
    struct AwaitedChange {
        changed: bool,
    }
    let helper = match sidecar_path() {
        Ok(helper) => helper,
        Err(reason) => {
            tracing::warn!(
                exception.message = reason,
                "could not watch for device changes"
            );
            return false;
        }
    };
    let output = Command::new(&helper)
        .arg("await-device-change")
        .arg("--seconds")
        .arg(limit.as_secs().to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            serde_json::from_slice::<AwaitedChange>(&output.stdout)
                .is_ok_and(|awaited| awaited.changed)
        }
        Ok(output) => {
            tracing::warn!(
                process.exit.code = output.status.code(),
                "the device watch ended with an error"
            );
            false
        }
        Err(error) => {
            tracing::warn!(exception.message = %error, "could not start the device watch");
            false
        }
    }
}

async fn send_command(
    stdin: &mut ChildStdin,
    seq: u64,
    command: &impl Serialize,
) -> std::io::Result<()> {
    let mut line = capture_protocol::encode(seq, command).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

async fn kill(child: &mut Child) {
    if let Err(error) = child.kill().await {
        tracing::warn!(exception.message = %error, "could not kill the capture process");
    }
}

fn log_exit(pid: u32, status: Result<&ExitStatus, &std::io::Error>) {
    match status {
        Ok(status) => tracing::info!(
            process.pid = pid,
            process.exit.code = status.code(),
            "capture process exited"
        ),
        Err(error) => tracing::warn!(
            process.pid = pid,
            exception.message = %error,
            "could not read the capture process's exit"
        ),
    }
}

fn exit_reason(status: Result<&ExitStatus, &std::io::Error>) -> String {
    match status {
        Ok(status) => format!("exited with {status}"),
        Err(error) => format!("exited; its status could not be read: {error}"),
    }
}
