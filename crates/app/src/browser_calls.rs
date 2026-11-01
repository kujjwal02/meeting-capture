//! Receives mc-meet's call events through host mode: appends each to its session log, acks it,
//! ends calls, feeds the call status board, and turns each finished log into a meeting. It also
//! takes mc-meet's debug material and sends mc-meet the status it shows.

mod call_book;
mod call_log;
mod log_folder;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use native_host::frames::{FrameFlow, LengthOrder, read_frame, write_frame};
use native_host::socket::{AppEnd, CallListener};
use pipeline::Pipeline;
use pipeline::settings::MeetSettings;
use serde::{Deserialize, Serialize};
use sources::browser_call::HealthSource;
use tauri::{AppHandle, Manager as _};
use telemetry::debug_mode::DebugMode;
use tokio::io::{AsyncWrite, ReadHalf};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::Instrument as _;

pub(crate) use self::call_book::InvalidSessionId;
use self::call_book::{CallBook, EventError, Logged, Now, OpenedCall, check_session_id};
use self::log_folder::LogFolder;
use crate::AppState;
use crate::call_debug::{FailedFrame, FrameChannel};
use crate::call_status::{
    BoardView, CallBoard, CallState, Moment, Observation, ProblemReason, clock,
};
use crate::calls::{CallEndReason, CallPlatform};
use crate::finished_logs::{self, LogSource};

/// The protocol this app speaks. An extension that says an older one gets a tray warning.
const PROTOCOL: u32 = 1;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Session requests a slow port may fall behind by before it misses some.
const SESSION_REQUEST_BACKLOG: usize = 16;
/// While a call wants its tab video, `call_status` goes out at least this often, changed or not.
/// mc-meet stops capturing after 10 s without one, so a lost stop cannot leave it recording.
const VIDEO_STATUS_HEARTBEAT: Duration = Duration::from_secs(5);

/// What the app asks of the worker that holds some sessions.
#[derive(Debug, Clone)]
enum SessionRequest {
    PageSnapshot(Vec<String>),
    ShortcutPressExpected(String),
}

/// The call book, shared by every connection.
pub(crate) struct BrowserCalls {
    book: Mutex<CallBook>,
    next_port_id: AtomicU64,
    /// Every welcomed port sends mc-meet the new value.
    meet_settings: watch::Sender<MeetSettings>,
    /// The Meet calls' status, which every welcomed port sends mc-meet when it changes.
    call_status: watch::Sender<CallStatusMessage>,
    /// Page snapshot requests and shortcut press notices. Every welcomed port forwards them;
    /// the worker that holds the named sessions acts on them.
    session_requests: broadcast::Sender<SessionRequest>,
    /// Each recording call's tab video task, by call id.
    tab_video: Mutex<HashMap<PathBuf, TabVideoEntry>>,
    /// When `call_status` last went out.
    call_status_sent_at: Mutex<Instant>,
}

/// Where a call's tab video goes, and whether mc-meet should capture it.
struct TabVideoEntry {
    sink: mpsc::Sender<TabVideoInput>,
    is_wanted: bool,
}

/// mc-meet's tab capture state, as `video_state` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TabVideoState {
    NeedsClick,
    Capturing,
    Stopped,
    Failed,
}

impl TabVideoState {
    fn as_str(self) -> &'static str {
        match self {
            Self::NeedsClick => "needs_click",
            Self::Capturing => "capturing",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

impl TabVideoInput {
    fn kind(&self) -> &'static str {
        match self {
            Self::Fragment(_) => "fragment",
            Self::NeedsClick(_) => "needs_click",
            Self::PressReady { .. } => "press_ready",
            Self::PressNoted { .. } => "press_noted",
            Self::PressRefused => "press_refused",
            Self::PressUnseen(_) => "press_unseen",
            Self::Capturing => "capturing",
            Self::Stopped { .. } => "stopped",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The last fragment of a capture that stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureEnd {
    pub capture_id: String,
    /// 0 when the capture sent no fragment.
    pub last_index: u64,
}

/// How mc-meet learned that a press missed the Meet tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PressMiss {
    /// The target worker saw no command within 1 s of its ready reply. An older mc-meet sends
    /// no reason, and this is what it means.
    #[default]
    NoCommand,
    /// The target worker saw the command on another tab, and left it alone.
    OtherTab,
    /// Another mc-meet, in another profile of the same browser, saw the command after the
    /// press notice for a session it does not hold, and left it alone.
    OtherWorker,
}

impl PressMiss {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoCommand => "no_command",
            Self::OtherTab => "other_tab",
            Self::OtherWorker => "other_worker",
        }
    }
}

/// What mc-meet reports with a `needs_click`: the facts the app's shortcut press depends on.
/// An older mc-meet sends neither field.
#[derive(Debug, Clone)]
pub(crate) struct PressChance {
    pub session_id: String,
    pub meet_tab: Option<MeetTabFocus>,
    /// mc-meet's bound shortcut from `chrome.commands.getAll()`, empty when none is bound.
    pub shortcut: Option<String>,
}

/// Where the Meet tab is in its Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct MeetTabFocus {
    /// The Meet tab is the active tab of its window.
    pub is_tab_active: bool,
    /// The Meet tab's window is the Chrome window focused last, which gets a posted key even
    /// while another app is in front.
    pub is_window_last_focused: bool,
}

/// What the relay hands the call's tab video task.
#[derive(Debug)]
pub(crate) enum TabVideoInput {
    Fragment(video_capture::TabFragment),
    /// Chrome refused the stream id because mc-meet was not invoked on the tab. mc-meet sends it
    /// again when the tab's focus changes.
    NeedsClick(PressChance),
    /// mc-meet read the press facts again and the Meet tab is still where the press lands, so
    /// the app may press now.
    PressReady {
        /// The browser that started the replying port's host, which is where the press goes.
        browser_pid: Option<u32>,
        port_id: u64,
    },
    /// An mc-meet that does not hold the session heard the press notice, and leaves alone any
    /// command for the next 1 s.
    PressNoted {
        port_id: u64,
    },
    /// mc-meet read the press facts again and they no longer hold; a `needs_click` with the
    /// new facts came first.
    PressRefused,
    /// The press did not reach the Meet tab: the key went to a window the target mc-meet cannot
    /// see, such as another Chrome profile.
    PressUnseen(PressMiss),
    Capturing,
    Stopped {
        /// Set on a `stopped` that names its capture's last fragment.
        ended: Option<CaptureEnd>,
    },
    Failed {
        detail: Option<String>,
    },
}

/// What the tray shows for mc-meet.
#[derive(Debug, Clone)]
pub(crate) struct BrowserCallStatus {
    pub is_connected: bool,
    pub is_extension_outdated: bool,
}

impl BrowserCalls {
    /// The calls whose logs go in `log_dir`.
    pub fn new(log_dir: PathBuf, meet_settings: MeetSettings) -> Self {
        Self {
            book: Mutex::new(CallBook::new(LogFolder::new(log_dir))),
            next_port_id: AtomicU64::new(0),
            meet_settings: watch::Sender::new(meet_settings),
            call_status: watch::Sender::new(CallStatusMessage::default()),
            session_requests: broadcast::channel(SESSION_REQUEST_BACKLOG).0,
            tab_video: Mutex::new(HashMap::new()),
            call_status_sent_at: Mutex::new(Instant::now()),
        }
    }

    /// Sends mc-meet the Meet calls' status when it changed. Times in labels use `time_zone`.
    pub fn publish_call_status(&self, view: &BoardView, time_zone: &TimeZone) {
        let meet_calls = view
            .calls
            .iter()
            .filter(|call| call.platform == CallPlatform::Meet);
        let mut message = {
            let book = self.lock();
            CallStatusMessage {
                calls: meet_calls
                    .clone()
                    .map(|call| MeetCallStatus::new(call, book.sessions_of(&call.id), time_zone))
                    .collect(),
            }
        };
        // After the book lock is released: the two locks are never held together.
        {
            let tab_video = self.lock_tab_video();
            for (status, call) in message.calls.iter_mut().zip(meet_calls) {
                status.is_video_wanted =
                    tab_video.get(&call.id).is_some_and(|entry| entry.is_wanted);
            }
        }
        let is_video_wanted = message.calls.iter().any(|call| call.is_video_wanted);
        let mut sent_at = self
            .call_status_sent_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let is_heartbeat_due = is_video_wanted && sent_at.elapsed() >= VIDEO_STATUS_HEARTBEAT;
        let is_sent = self.call_status.send_if_modified(|current| {
            let is_changed = *current != message;
            *current = message;
            is_changed || is_heartbeat_due
        });
        if is_sent {
            *sent_at = Instant::now();
        }
    }

    /// Takes the user's stop or resume onto the board. Logs it first in an open Meet call's
    /// log, so a reopened call keeps it. Both happen under the book lock, so an event between
    /// them cannot see the book and the board disagree.
    pub fn set_recording_stopped(&self, board: &CallBoard, call_id: &Path, is_stopped: bool) {
        let mut book = self.lock();
        if let Err(error) = book.set_recording_stopped(call_id, is_stopped, Now::read()) {
            tracing::error!(
                call.id = %call_id.display(),
                exception.message = %error,
                "could not log a recording stop or resume"
            );
        }
        board.observe(
            call_id,
            Observation::RecordingStopped(is_stopped),
            Moment::now(),
        );
    }

    /// Asks mc-meet for a page snapshot of each tab in the call.
    pub fn request_page_snapshot(&self, call_id: &Path) {
        let session_ids = self.lock().sessions_of(call_id);
        if session_ids.is_empty() {
            return;
        }
        // An error only means no port is open; the snapshot is then lost, as the page is.
        let _unheard = self
            .session_requests
            .send(SessionRequest::PageSnapshot(session_ids));
    }

    /// The log a session writes to, open or ended. `session_id` is checked: it names files.
    pub fn call_of_session(&self, session_id: &str) -> Result<Option<PathBuf>, InvalidSessionId> {
        check_session_id("session_id", session_id)?;
        let book = self.lock();
        if let Some(log_path) = book.log_path_of(session_id) {
            return Ok(Some(log_path));
        }
        Ok(book.logs().find(session_id).ok().flatten())
    }

    /// The sessions of a call: those writing to it now, and the one that created its log.
    pub fn sessions_of_call(&self, call_id: &Path) -> Vec<String> {
        let mut sessions = self.lock().sessions_of(call_id);
        if let Some(first) = log_folder::first_session_id(call_id)
            && !sessions.contains(&first)
        {
            sessions.push(first);
        }
        sessions
    }

    /// Sends the settings to every welcomed mc-meet port. A port stays open through a call, so a
    /// save must reach the ports already open.
    pub fn send_settings(&self, meet_settings: MeetSettings) {
        self.meet_settings.send_if_modified(|current| {
            let is_changed = *current != meet_settings;
            *current = meet_settings;
            is_changed
        });
    }

    /// Routes the call's tab video to `sink` and marks it wanted.
    pub fn open_tab_video(&self, call_id: &Path, sink: mpsc::Sender<TabVideoInput>) {
        self.lock_tab_video().insert(
            call_id.to_path_buf(),
            TabVideoEntry {
                sink,
                is_wanted: true,
            },
        );
    }

    /// Keeps the sink but tells mc-meet to stop or start capturing.
    pub fn set_tab_video_wanted(&self, call_id: &Path, is_wanted: bool) {
        if let Some(entry) = self.lock_tab_video().get_mut(call_id) {
            entry.is_wanted = is_wanted;
        }
    }

    /// Tells every welcomed mc-meet that the app presses the shortcut for this session's tab
    /// next. The one that holds the session answers; every other one leaves alone a command
    /// that comes within 1 s and reports it as `shortcut_press_landed`, since the press then
    /// landed in its profile instead.
    ///
    /// Returns the ports that hear it. The app presses only after each one answered, so no
    /// mc-meet can get the key before it knows the key is not the user's.
    pub fn expect_shortcut_press(&self, session_id: &str) -> HashSet<u64> {
        let book = self.lock();
        // No receiver means no port is welcomed; the press wait then times out. The book lock
        // is held across the send, so no port is greeted between the snapshot and the notice.
        let _unheard = self
            .session_requests
            .send(SessionRequest::ShortcutPressExpected(session_id.to_owned()));
        book.greeted_port_ids()
    }

    /// Drops the sink. Later fragments for the call are dropped.
    pub fn close_tab_video(&self, call_id: &Path) {
        self.lock_tab_video().remove(call_id);
    }

    /// The call's tab video sink, if its task is open.
    fn tab_video_sink(&self, call_id: &Path) -> Option<mpsc::Sender<TabVideoInput>> {
        self.lock_tab_video()
            .get(call_id)
            .map(|entry| entry.sink.clone())
    }

    fn lock(&self) -> MutexGuard<'_, CallBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_tab_video(&self) -> MutexGuard<'_, HashMap<PathBuf, TabVideoEntry>> {
        self.tab_video
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> BrowserCallStatus {
        self.lock().status()
    }
}

/// Messages from the extension, and the host's first frame.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ExtensionMessage {
    HostHello {
        origin: String,
        /// The host's parent process, which is the browser on macOS.
        #[serde(default)]
        browser_pid: Option<u32>,
    },
    Hello {
        protocol: u32,
        #[serde(default)]
        extension_version: Option<String>,
        /// The sessions the worker has open. Older extensions leave it out.
        #[serde(default)]
        sessions: Vec<String>,
    },
    Event {
        seq: u64,
        session_id: String,
        event: serde_json::Value,
    },
    /// A data channel frame that did not decode. Not queued: one lost on a disconnect is fine.
    FrameFailed {
        session_id: String,
        channel: FrameChannel,
        at: Timestamp,
        reason: String,
        skeleton: String,
        byte_length: u64,
        #[serde(default)]
        bytes_base64: Option<String>,
    },
    /// The page's state, in reply to `snapshot_request`.
    PageSnapshot {
        session_id: String,
        snapshot: serde_json::Value,
    },
    /// The sessions whose tab went away, every one each time the list changes, and after
    /// `welcome`. mc-meet ends each in 5 minutes unless its tab comes back.
    PagesAway {
        session_ids: Vec<String>,
    },
    /// The popup's "Save debug report" button.
    SaveDebugReport {
        /// The popup's tab session, if the tab is in a call.
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default, rename = "with_text")]
        is_with_text: bool,
    },
    /// The popup's "Stop recording this call" for its tab's session.
    StopRecording {
        session_id: String,
    },
    /// The popup's "Resume recording" for its tab's session.
    ResumeRecording {
        session_id: String,
    },
    /// One fMP4 fragment of the Meet tab's video. Not queued or acked: mc-meet keeps a short
    /// buffer across a reconnect, and the writer drops a repeat.
    VideoFragment(VideoFragmentMessage),
    /// mc-meet's tab capture changed state.
    VideoState(VideoStateMessage),
    /// mc-meet's answers to `shortcut_press_expected`: ready to see the press, refused because
    /// the tab moved, or unseen when the press did not reach the tab.
    ShortcutPressReady {
        session_id: String,
    },
    ShortcutPressRefused {
        session_id: String,
    },
    ShortcutPressUnseen {
        session_id: String,
        #[serde(default)]
        reason: PressMiss,
    },
    /// From an mc-meet that does not hold the session: it heard the press notice.
    ShortcutPressNoted {
        session_id: String,
    },
    /// From an mc-meet that does not hold the session: the press for it landed in this one's
    /// window. Every welcomed port hears the press notice, so this one knew to leave it alone.
    ShortcutPressLanded {
        session_id: String,
    },
    /// A type from a newer extension.
    #[serde(other)]
    Unknown,
}

/// The fields of a `video_state`, as mc-meet sends them.
#[derive(Deserialize)]
struct VideoStateMessage {
    session_id: String,
    state: TabVideoState,
    /// Why capture stopped, or the error it failed with.
    #[serde(default)]
    detail: Option<String>,
    /// With `last_index`, on a `stopped`: the capture that ended.
    #[serde(default)]
    capture_id: Option<String>,
    #[serde(default)]
    last_index: Option<u64>,
    /// On a `needs_click`: where the Meet tab is.
    #[serde(default)]
    meet_tab: Option<MeetTabFocus>,
    /// On a `needs_click`: mc-meet's bound shortcut, empty when none is bound.
    #[serde(default)]
    shortcut: Option<String>,
}

impl ExtensionMessage {
    /// Whether the message may come only after the host's hello.
    fn needs_host_hello(&self) -> bool {
        match self {
            Self::HostHello { .. } | Self::Unknown => false,
            Self::Hello { .. }
            | Self::Event { .. }
            | Self::FrameFailed { .. }
            | Self::PageSnapshot { .. }
            | Self::PagesAway { .. }
            | Self::SaveDebugReport { .. }
            | Self::StopRecording { .. }
            | Self::ResumeRecording { .. }
            | Self::VideoFragment(_)
            | Self::VideoState(_)
            | Self::ShortcutPressReady { .. }
            | Self::ShortcutPressRefused { .. }
            | Self::ShortcutPressUnseen { .. }
            | Self::ShortcutPressLanded { .. }
            | Self::ShortcutPressNoted { .. } => true,
        }
    }
}

/// The fields of a `video_fragment`, as mc-meet sends them.
#[derive(Deserialize)]
struct VideoFragmentMessage {
    session_id: String,
    capture_id: String,
    part: u32,
    codec: video_capture::VideoCodec,
    width_px: u32,
    height_px: u32,
    init_base64: String,
    index: u64,
    first_frame_utc_ms: f64,
    duration_ms: f64,
    frame_count: u32,
    #[serde(default)]
    playback_latency_ms: Option<f64>,
    #[serde(default)]
    playback_base_latency_ms: Option<f64>,
    #[serde(default)]
    part_clock_offset_ms: Option<f64>,
    #[serde(default)]
    clock_offset_ms: Option<f64>,
    bytes_base64: String,
}

impl VideoFragmentMessage {
    /// Decodes the base64 and the first frame's time. Runs on the relay task: a few hundred KB
    /// every 2 s is cheap next to a hop to another thread.
    fn decode(self) -> Result<video_capture::TabFragment, FragmentDecodeError> {
        let decode = |field: &'static str, encoded: &str| {
            BASE64
                .decode(encoded)
                .map_err(|source| FragmentDecodeError::Base64 { field, source })
        };
        let init_segment = decode("init_base64", &self.init_base64)?;
        let bytes = decode("bytes_base64", &self.bytes_base64)?;
        let first_frame_at = timestamp_of_utc_ms(self.first_frame_utc_ms).ok_or(
            FragmentDecodeError::FirstFrameTime {
                utc_ms: self.first_frame_utc_ms,
            },
        )?;
        Ok(video_capture::TabFragment {
            capture_id: self.capture_id,
            part: self.part,
            codec: self.codec,
            width_px: self.width_px,
            height_px: self.height_px,
            init_segment,
            index: self.index,
            first_frame_at,
            part_clock_offset_ms: self.part_clock_offset_ms.filter(|ms| ms.is_finite()),
            clock_offset_ms: self.clock_offset_ms.filter(|ms| ms.is_finite()),
            duration_ms: self.duration_ms,
            frame_count: self.frame_count,
            playback_latency_ms: self.playback_latency_ms,
            playback_base_latency_ms: self.playback_base_latency_ms,
            bytes,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AppMessage {
    Welcome {
        protocol: u32,
        app_version: &'static str,
        /// The version in the folder the app installs mc-meet into, when it has one.
        folder_version: Option<String>,
        open_sessions: Vec<String>,
        settings: MeetSettingsMessage,
    },
    Ack {
        seq: u64,
    },
    Settings {
        settings: MeetSettingsMessage,
    },
    CallStatus(CallStatusMessage),
    SnapshotRequest {
        session_ids: Vec<String>,
    },
    /// The app presses mc-meet's shortcut for this session's tab once mc-meet answers.
    ShortcutPressExpected {
        session_id: String,
    },
    DebugReportSaved {
        path: String,
    },
    DebugReportFailed {
        error: String,
    },
    /// The end of debug mode in RFC 3339 UTC, or `None` when it is off. mc-meet stores it and
    /// shows it.
    DebugMode {
        until: Option<String>,
    },
}

/// The open Meet calls, as mc-meet shows them on its icon and in its popup.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct CallStatusMessage {
    calls: Vec<MeetCallStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MeetCallStatus {
    /// The tabs' sessions writing to the call. mc-meet finds each tab by its session.
    session_ids: Vec<String>,
    title: Option<String>,
    state: StatusWord,
    reason: Option<ProblemReason>,
    reason_text: Option<&'static str>,
    started_at: Timestamp,
    started_at_label: String,
    recording_since_label: Option<String>,
    caption_count: u64,
    last_caption_at: Option<Timestamp>,
    caption_source: Option<CaptionSourceWord>,
    /// The last problems, oldest first.
    problems: Vec<ProblemStatus>,
    /// The app records this call and wants its Meet tab's video. mc-meet captures the tab
    /// while it holds.
    is_video_wanted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusWord {
    Waiting,
    Recording,
    Problem,
    /// mc-meet stops reading captions and chat for the call's sessions while this holds.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptionSourceWord {
    Channel,
    Dom,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ProblemStatus {
    reason: ProblemReason,
    reason_text: &'static str,
    at: Timestamp,
    at_label: String,
}

impl MeetCallStatus {
    fn new(
        call: &crate::call_status::CallView,
        session_ids: Vec<String>,
        time_zone: &TimeZone,
    ) -> Self {
        let (state, reason) = match call.state {
            CallState::Waiting => (StatusWord::Waiting, None),
            CallState::Recording => (StatusWord::Recording, None),
            CallState::Problem(reason) => (StatusWord::Problem, Some(reason)),
            CallState::Stopped => (StatusWord::Stopped, None),
        };
        let caption_source = call.caption_source.map(|source| match source {
            HealthSource::Channel => CaptionSourceWord::Channel,
            HealthSource::Dom => CaptionSourceWord::Dom,
            HealthSource::None => CaptionSourceWord::None,
        });
        Self {
            session_ids,
            title: call.title.clone(),
            state,
            reason,
            reason_text: reason.map(ProblemReason::describe),
            started_at: call.started_at,
            started_at_label: clock(call.started_at, time_zone),
            recording_since_label: call.recording_since.map(|at| clock(at, time_zone)),
            caption_count: call.caption_count,
            last_caption_at: call.last_caption_at,
            caption_source,
            problems: call
                .problems
                .iter()
                .map(|problem| ProblemStatus {
                    reason: problem.reason,
                    reason_text: problem.reason.describe(),
                    at: problem.at,
                    at_label: clock(problem.at, time_zone),
                })
                .collect(),
            // `publish_call_status` fills it from the tab video registry.
            is_video_wanted: false,
        }
    }
}

/// The settings mc-meet acts on, as the extension reads them.
#[derive(Serialize)]
struct MeetSettingsMessage {
    hide_captions: bool,
}

impl From<MeetSettings> for MeetSettingsMessage {
    fn from(settings: MeetSettings) -> Self {
        Self {
            hide_captions: settings.hide_captions,
        }
    }
}

pub(crate) fn start(app: &AppHandle, pipeline: &Arc<Pipeline>) {
    let state = app.state::<AppState>();
    let app_dirs = state.settings.app_dirs();
    register_host(app_dirs.host_manifest_dir());
    let logs = state.browser_calls.lock().logs().clone();
    // Before the listener starts, so a resent event cannot reopen a log this is closing.
    match logs.finish_open_logs() {
        Ok(logs) => {
            state
                .calls
                .end_at_launch(&logs.ended_paths, CallPlatform::Meet);
            let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
            tauri::async_runtime::spawn_blocking(move || {
                finished_logs::record_left_over(
                    &app,
                    &pipeline,
                    LogSource::MeetCall,
                    logs.log_paths,
                    0,
                );
            });
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not read the browser call folder");
        }
    }
    let socket = app_dirs.browser_call_socket().to_path_buf();
    tauri::async_runtime::spawn(listen(app.clone(), Arc::clone(pipeline), socket));
    tauri::async_runtime::spawn(end_idle_calls(app.clone(), Arc::clone(pipeline)));
}

// Chrome reads the manifest each time the extension connects, so a moved or updated app is
// found again after its next start.
fn register_host(data_dir: &Path) {
    // Canonical, since macOS reports the path the app was started with, `..` parts included,
    // and Chrome does not start a host whose path has them.
    let executable = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(executable) => executable,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not find the app executable");
            return;
        }
    };
    match native_host::manifest::register(&executable, data_dir) {
        Ok(registrations) => {
            for registration in registrations {
                match registration.outcome {
                    Ok(()) => tracing::info!(
                        browser.name = registration.browser,
                        file.path = registration.location,
                        "native host registered"
                    ),
                    Err(error) => tracing::warn!(
                        browser.name = registration.browser,
                        file.path = registration.location,
                        exception.message = %error,
                        "could not register the native host"
                    ),
                }
            }
        }
        Err(error) => {
            tracing::error!(exception.message = %error, "could not register the native host");
        }
    }
}

async fn listen(app: AppHandle, pipeline: Arc<Pipeline>, socket: PathBuf) {
    let mut listener = match CallListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(
                server.address = %socket.display(),
                exception.message = %error,
                "could not listen for mc-meet"
            );
            return;
        }
    };
    tracing::info!(server.address = %socket.display(), "listening for mc-meet");
    loop {
        match listener.accept().await {
            Ok(connection) => {
                let calls = &app.state::<AppState>().browser_calls;
                let port_id = calls.next_port_id.fetch_add(1, Ordering::Relaxed);
                let span = tracing::info_span!("browser_call.port", browser_call.port_id = port_id);
                let port = Port {
                    app: app.clone(),
                    pipeline: Arc::clone(&pipeline),
                    id: port_id,
                    browser_pid: OnceLock::new(),
                };
                tauri::async_runtime::spawn(port.serve(connection).instrument(span));
            }
            Err(error) => {
                tracing::error!(exception.message = %error, "could not accept a host connection");
                // A failing accept usually fails again at once.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// One host-mode connection: one extension port.
struct Port {
    app: AppHandle,
    pipeline: Arc<Pipeline>,
    id: u64,
    /// The browser that started this port's host, as its hello claims. Only a process of this
    /// user can reach the app's socket, but it can claim any pid, so the press checks that the
    /// pid is a Chromium browser before it posts a key there.
    browser_pid: OnceLock<u32>,
}

impl Port {
    async fn serve(self, connection: AppEnd) {
        let (from_host, mut to_host) = tokio::io::split(connection);
        match self.relay(from_host, &mut to_host).await {
            Ok(()) => tracing::info!("port closed"),
            Err(error) => tracing::warn!(exception.message = %error, "port closed on an error"),
        }
        let (unheld, is_extension_connected) = {
            let mut book = self.calls().lock();
            (book.close_port(self.id), book.is_extension_connected())
        };
        let state = self.app.state::<AppState>();
        state
            .calls
            .status()
            .set_extension_connected(is_extension_connected);
        for call_id in &unheld {
            state
                .calls
                .status()
                .observe(call_id, Observation::MeetPortHeld(false), Moment::now());
            state
                .call_debug
                .remember(call_id, serde_json::json!({ "type": "port_closed" }));
        }
        crate::tray::refresh(&self.app);
    }

    fn calls(&self) -> &BrowserCalls {
        &self.app.state::<AppState>().inner().browser_calls
    }

    async fn relay(
        &self,
        from_host: ReadHalf<AppEnd>,
        to_host: &mut (impl AsyncWrite + Unpin),
    ) -> std::io::Result<()> {
        let mut is_host_known = false;
        // Debug reports are written off the runtime and answered through here.
        let (reply_sender, replies) = mpsc::unbounded_channel::<AppMessage>();
        let debug_mode = self.app.state::<AppState>().debug_mode.subscribe();
        let mut feeds = PortFeeds::new(self.calls(), debug_mode, replies);
        let mut is_welcomed = false;
        // A frame read dropped halfway would lose its bytes, so one read lives across the
        // other sends and owns the read half until it finishes.
        let mut next_frame = Box::pin(read_next_frame(from_host));
        loop {
            let frame = tokio::select! {
                (from_host, frame) = &mut next_frame => {
                    next_frame.set(read_next_frame(from_host));
                    match frame? {
                        Some(frame) => frame,
                        None => break,
                    }
                },
                message = feeds.next(), if is_welcomed => {
                    send(to_host, &message).await?;
                    continue;
                }
            };
            let message: ExtensionMessage = match serde_json::from_slice(&frame) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(exception.message = %error, "ignored a message that is not valid");
                    continue;
                }
            };
            match message {
                ExtensionMessage::HostHello {
                    origin,
                    browser_pid,
                } => {
                    if !self.take_host_hello(&origin, browser_pid) {
                        return Ok(());
                    }
                    is_host_known = true;
                }
                // Only host mode connects here, and it always speaks first.
                _ if !is_host_known && message.needs_host_hello() => {
                    tracing::warn!("refused a connection that skipped the host hello");
                    return Ok(());
                }
                ExtensionMessage::Hello {
                    protocol,
                    extension_version,
                    sessions,
                } => {
                    let open_sessions =
                        self.greet(protocol, extension_version.as_deref(), &sessions);
                    welcome(to_host, &self.app, &mut feeds, open_sessions).await?;
                    is_welcomed = true;
                }
                ExtensionMessage::Event {
                    seq,
                    session_id,
                    event,
                } => {
                    if self.take_event(seq, &session_id, &event) {
                        send(to_host, &AppMessage::Ack { seq }).await?;
                    }
                }
                ExtensionMessage::PagesAway { session_ids } => self.take_away_sessions(session_ids),
                ExtensionMessage::StopRecording { session_id } => {
                    self.set_recording_stopped(&session_id, true);
                }
                ExtensionMessage::ResumeRecording { session_id } => {
                    self.set_recording_stopped(&session_id, false);
                }
                message @ (ExtensionMessage::FrameFailed { .. }
                | ExtensionMessage::PageSnapshot { .. }
                | ExtensionMessage::SaveDebugReport { .. }) => {
                    self.take_debug_message(message, &reply_sender);
                }
                message @ (ExtensionMessage::VideoFragment(_)
                | ExtensionMessage::VideoState(_)
                | ExtensionMessage::ShortcutPressReady { .. }
                | ExtensionMessage::ShortcutPressRefused { .. }
                | ExtensionMessage::ShortcutPressUnseen { .. }
                | ExtensionMessage::ShortcutPressLanded { .. }
                | ExtensionMessage::ShortcutPressNoted { .. }) => {
                    self.take_video_message(message);
                }
                ExtensionMessage::Unknown if is_host_known => {
                    crate::meet_telemetry::take(&self.app, &frame);
                }
                ExtensionMessage::Unknown => {}
            }
        }
        Ok(())
    }

    /// Whether the host speaks for mc-meet. Keeps the browser's pid for the shortcut press.
    fn take_host_hello(&self, origin: &str, browser_pid: Option<u32>) -> bool {
        if origin != native_host::EXTENSION_ORIGIN {
            tracing::warn!(browser.origin = origin, "refused an unknown extension");
            return false;
        }
        if let Some(pid) = browser_pid {
            let _ = self.browser_pid.set(pid);
        }
        tracing::info!(process.parent_pid = browser_pid, "host connected");
        true
    }

    /// Records the hello and returns the open sessions for `welcome`.
    fn greet(
        &self,
        protocol: u32,
        extension_version: Option<&str>,
        worker_sessions: &[String],
    ) -> Vec<String> {
        tracing::info!(
            browser.extension.version = extension_version,
            browser.extension.protocol = protocol,
            "mc-meet connected"
        );
        let (open_sessions, held_calls) =
            self.calls()
                .lock()
                .greet(self.id, protocol, worker_sessions);
        let board = self.app.state::<AppState>().inner().calls.status();
        board.set_extension_connected(true);
        for call_id in &held_calls {
            board.observe(call_id, Observation::MeetPortHeld(true), Moment::now());
        }
        crate::tray::refresh(&self.app);
        open_sessions
    }

    fn take_away_sessions(&self, session_ids: Vec<String>) {
        if let Some(invalid) = session_ids
            .iter()
            .find_map(|session_id| check_session_id("session_ids", session_id).err())
        {
            tracing::warn!(exception.message = %invalid, "ignored a pages_away message that is not valid");
            return;
        }
        let calls = self
            .calls()
            .lock()
            .note_away_sessions(self.id, session_ids.into_iter().collect::<HashSet<_>>());
        let (board, now) = (
            self.app.state::<AppState>().inner().calls.status(),
            Moment::now(),
        );
        for (call_id, is_away) in &calls {
            board.observe(call_id, Observation::MeetPagesAway(*is_away), now);
        }
    }

    /// The popup's stop or resume, for the call its tab's session writes to.
    fn set_recording_stopped(&self, session_id: &str, is_stopped: bool) {
        let call_id = match self.calls().call_of_session(session_id) {
            Ok(Some(call_id)) => call_id,
            Ok(None) => {
                tracing::warn!(
                    browser_call.session_id = session_id,
                    "a recording stop or resume names a session with no call"
                );
                return;
            }
            Err(invalid) => {
                tracing::warn!(exception.message = %invalid, "refused a recording stop or resume");
                return;
            }
        };
        if is_stopped {
            crate::call_capture::stop_call(&self.app, &call_id);
        } else {
            crate::call_capture::resume_call(&self.app, &call_id);
        }
    }

    // Debug material and the popup's report button. None of it is acked or logged as an event.
    fn take_debug_message(
        &self,
        message: ExtensionMessage,
        reply_sender: &mpsc::UnboundedSender<AppMessage>,
    ) {
        match message {
            ExtensionMessage::FrameFailed {
                session_id,
                channel,
                at,
                reason,
                skeleton,
                byte_length,
                bytes_base64,
            } => {
                let frame = FailedFrame {
                    channel,
                    at,
                    reason,
                    skeleton,
                    byte_length,
                    bytes_base64,
                };
                self.take_failed_frame(&session_id, &frame);
            }
            ExtensionMessage::PageSnapshot {
                session_id,
                snapshot,
            } => {
                let Some(call_id) = self.calls().lock().log_path_of(&session_id) else {
                    return;
                };
                self.app
                    .state::<AppState>()
                    .call_debug
                    .record_page_snapshot(&call_id, &session_id, &snapshot);
            }
            ExtensionMessage::SaveDebugReport {
                session_id,
                is_with_text,
            } => {
                if let Some(Err(invalid)) = session_id
                    .as_deref()
                    .map(|session_id| check_session_id("session_id", session_id))
                {
                    tracing::warn!(exception.message = %invalid, "refused a debug report request");
                    let _unheard = reply_sender.send(AppMessage::DebugReportFailed {
                        error: invalid.to_string(),
                    });
                    return;
                }
                let (app, reply_sender) = (self.app.clone(), reply_sender.clone());
                tauri::async_runtime::spawn_blocking(move || {
                    let reply = match crate::call_debug::save_debug_report(
                        &app,
                        session_id.as_deref(),
                        is_with_text,
                    ) {
                        Ok(folder) => AppMessage::DebugReportSaved {
                            path: folder.display().to_string(),
                        },
                        Err(error) => AppMessage::DebugReportFailed {
                            error: error.to_string(),
                        },
                    };
                    // An error only means the port closed before the report was written.
                    let _unheard = reply_sender.send(reply);
                });
            }
            ExtensionMessage::HostHello { .. }
            | ExtensionMessage::Hello { .. }
            | ExtensionMessage::Event { .. }
            | ExtensionMessage::PagesAway { .. }
            | ExtensionMessage::StopRecording { .. }
            | ExtensionMessage::ResumeRecording { .. }
            | ExtensionMessage::VideoFragment(_)
            | ExtensionMessage::VideoState(_)
            | ExtensionMessage::ShortcutPressReady { .. }
            | ExtensionMessage::ShortcutPressRefused { .. }
            | ExtensionMessage::ShortcutPressUnseen { .. }
            | ExtensionMessage::ShortcutPressLanded { .. }
            | ExtensionMessage::ShortcutPressNoted { .. }
            | ExtensionMessage::Unknown => {}
        }
    }

    /// Logs one event and reports whether to ack it.
    fn take_event(&self, seq: u64, session_id: &str, event: &serde_json::Value) -> bool {
        let span = tracing::info_span!(
            "browser_call.event",
            browser_call.seq = seq,
            browser_call.session_id = session_id
        );
        let _entered = span.enter();
        let state = self.app.state::<AppState>();
        let lead_up_line = serde_json::json!({
            "seq": seq,
            "session_id": session_id,
            "event": event,
        });
        let logged = {
            let mut book = self.calls().lock();
            let logged = book.take_event(self.id, seq, session_id, event, Now::read());
            // Under the book lock, so the call stream sees opens and ends in the book's order.
            match &logged {
                Ok(
                    Logged::Opened { opened, .. }
                    | Logged::Withheld {
                        opened: Some(opened),
                    },
                ) => open_on_stream(&state, opened),
                Ok(Logged::Ended { log_path, reason }) => {
                    state.calls.end(log_path, CallEndReason::from(*reason));
                }
                Ok(
                    Logged::Appended { .. } | Logged::Duplicate | Logged::Withheld { opened: None },
                )
                | Err(_) => {}
            }
            logged
        };
        let board = state.calls.status();
        match &logged {
            Ok(
                Logged::Appended {
                    log_path,
                    observation,
                }
                | Logged::Opened {
                    opened: OpenedCall { log_path, .. },
                    observation,
                },
            ) => {
                let now = Moment::now();
                board.observe(log_path, Observation::MeetPortHeld(true), now);
                if let Some(observation) = observation {
                    board.observe(log_path, observation.clone(), now);
                }
                state.call_debug.remember(log_path, lead_up_line);
            }
            // Its caption text stays out of the lead-up: the user stopped recording.
            Ok(Logged::Withheld {
                opened: Some(OpenedCall { log_path, .. }),
            }) => board.observe(log_path, Observation::MeetPortHeld(true), Moment::now()),
            Ok(Logged::Ended { log_path, .. }) => state.call_debug.forget_lead_up(log_path),
            Ok(Logged::Duplicate | Logged::Withheld { opened: None }) | Err(_) => {}
        }
        // Other events reach the tray with the status tick. A rebuild closes an open menu.
        match &logged {
            Ok(
                Logged::Opened { .. } | Logged::Withheld { opened: Some(_) } | Logged::Ended { .. },
            ) => crate::tray::refresh(&self.app),
            Ok(Logged::Appended { .. } | Logged::Duplicate | Logged::Withheld { opened: None })
            | Err(_) => {}
        }
        match &logged {
            Ok(
                Logged::Appended { .. }
                | Logged::Opened { .. }
                | Logged::Duplicate
                | Logged::Withheld { .. },
            ) => {}
            Ok(Logged::Ended { log_path, .. }) => {
                let (app, pipeline) = (self.app.clone(), Arc::clone(&self.pipeline));
                let log_path = log_path.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    record_and_run(&app, &pipeline, &log_path);
                });
            }
            Err(
                error @ (EventError::InvalidSessionId(_)
                | EventError::InvalidEvent(_)
                | EventError::AppOnlyEvent),
            ) => {
                tracing::warn!(exception.message = %error, "dropped an event that is not valid");
            }
            Err(error @ EventError::Write(_)) => {
                tracing::error!(exception.message = %error, "could not log an event");
            }
        }
        is_acked(&logged)
    }

    /// The call a video message's session writes to, or `None` after logging why there is none.
    fn call_of_video_session(&self, session_id: &str) -> Option<PathBuf> {
        match self.calls().call_of_session(session_id) {
            Ok(Some(call_id)) => Some(call_id),
            Ok(None) => {
                tracing::debug!(
                    browser_call.session_id = session_id,
                    "a video message names a session with no call"
                );
                None
            }
            Err(invalid) => {
                tracing::warn!(exception.message = %invalid, "refused a video message");
                None
            }
        }
    }

    /// Fragments, states and press replies, for the session's tab video task.
    fn take_video_message(&self, message: ExtensionMessage) {
        match message {
            ExtensionMessage::VideoFragment(message) => self.take_tab_fragment(message),
            ExtensionMessage::VideoState(message) => self.take_tab_video_state(message),
            ExtensionMessage::ShortcutPressReady { session_id } => {
                let browser_pid = self.browser_pid.get().copied();
                let input = TabVideoInput::PressReady {
                    browser_pid,
                    port_id: self.id,
                };
                self.route_tab_video(&session_id, input);
            }
            ExtensionMessage::ShortcutPressNoted { session_id } => {
                let input = TabVideoInput::PressNoted { port_id: self.id };
                self.route_tab_video(&session_id, input);
            }
            ExtensionMessage::ShortcutPressRefused { session_id } => {
                self.route_tab_video(&session_id, TabVideoInput::PressRefused);
            }
            ExtensionMessage::ShortcutPressUnseen { session_id, reason } => {
                self.route_tab_video(&session_id, TabVideoInput::PressUnseen(reason));
            }
            ExtensionMessage::ShortcutPressLanded { session_id } => {
                let input = TabVideoInput::PressUnseen(PressMiss::OtherWorker);
                self.route_tab_video(&session_id, input);
            }
            ExtensionMessage::HostHello { .. }
            | ExtensionMessage::Hello { .. }
            | ExtensionMessage::Event { .. }
            | ExtensionMessage::FrameFailed { .. }
            | ExtensionMessage::PageSnapshot { .. }
            | ExtensionMessage::PagesAway { .. }
            | ExtensionMessage::SaveDebugReport { .. }
            | ExtensionMessage::StopRecording { .. }
            | ExtensionMessage::ResumeRecording { .. }
            | ExtensionMessage::Unknown => {}
        }
    }

    // Never acked or answered: a lost fragment is a gap, and mc-meet resends its buffer after a
    // reconnect.
    fn take_tab_fragment(&self, message: VideoFragmentMessage) {
        let session_id = message.session_id.clone();
        let fragment = match message.decode() {
            Ok(fragment) => fragment,
            Err(error) => {
                tracing::warn!(
                    browser_call.session_id = session_id,
                    exception.message = %error,
                    "video.fragment_invalid"
                );
                return;
            }
        };
        let Some(call_id) = self.call_of_video_session(&session_id) else {
            return;
        };
        let Some(sink) = self.calls().tab_video_sink(&call_id) else {
            tracing::debug!(
                call.id = %call_id.display(),
                video.capture_id = fragment.capture_id,
                video.fragment_index = fragment.index,
                "video.fragment_unrouted"
            );
            return;
        };
        let (capture_id, index) = (fragment.capture_id.clone(), fragment.index);
        match sink.try_send(TabVideoInput::Fragment(fragment)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => tracing::warn!(
                call.id = %call_id.display(),
                video.capture_id = capture_id,
                video.fragment_index = index,
                "video.fragment_dropped"
            ),
            // The task ended between the lookup and the send; it closes its entry itself.
            Err(TrySendError::Closed(_)) => tracing::debug!(
                call.id = %call_id.display(),
                video.capture_id = capture_id,
                video.fragment_index = index,
                "video.fragment_unrouted"
            ),
        }
    }

    fn take_tab_video_state(&self, message: VideoStateMessage) {
        let VideoStateMessage {
            session_id,
            state,
            detail,
            capture_id,
            last_index,
            meet_tab,
            shortcut,
        } = message;
        tracing::info!(
            browser_call.session_id = session_id,
            video.tab_state = state.as_str(),
            video.detail = detail.as_deref(),
            video.capture_id = capture_id.as_deref(),
            video.last_fragment_index = last_index,
            video.is_tab_active = meet_tab.map(|tab| tab.is_tab_active),
            video.is_window_last_focused = meet_tab.map(|tab| tab.is_window_last_focused),
            video.shortcut = shortcut.as_deref(),
            "video.tab_state"
        );
        let input = match state {
            TabVideoState::NeedsClick => TabVideoInput::NeedsClick(PressChance {
                session_id: session_id.clone(),
                meet_tab,
                shortcut,
            }),
            TabVideoState::Capturing => TabVideoInput::Capturing,
            TabVideoState::Stopped => TabVideoInput::Stopped {
                ended: capture_id
                    .zip(last_index)
                    .map(|(capture_id, last_index)| CaptureEnd {
                        capture_id,
                        last_index,
                    }),
            },
            TabVideoState::Failed => TabVideoInput::Failed { detail },
        };
        self.route_tab_video(&session_id, input);
    }

    /// Hands a video state or press reply to the session's call task, if it runs.
    fn route_tab_video(&self, session_id: &str, input: TabVideoInput) {
        let Some(call_id) = self.call_of_video_session(session_id) else {
            return;
        };
        let Some(sink) = self.calls().tab_video_sink(&call_id) else {
            return;
        };
        match sink.try_send(input) {
            // A closed sink means the task ended between the lookup and the send.
            Ok(()) | Err(TrySendError::Closed(_)) => {}
            Err(TrySendError::Full(input)) => tracing::warn!(
                call.id = %call_id.display(),
                video.input = input.kind(),
                "video.tab_state_dropped"
            ),
        }
    }

    // A frame of a session the app has no open call for is dropped: its call ended, or its
    // first event has not arrived yet.
    fn take_failed_frame(&self, session_id: &str, frame: &FailedFrame) {
        let Some(call_id) = self.calls().lock().log_path_of(session_id) else {
            return;
        };
        tracing::warn!(
            call.id = %call_id.display(),
            browser_call.session_id = session_id,
            browser_call.channel = frame.channel.label(),
            error.type = frame.reason,
            "mc-meet could not decode a frame"
        );
        self.app
            .state::<AppState>()
            .call_debug
            .record_failed_frame(&call_id, frame);
    }
}

// An event that cannot be written is not acked, so the extension resends it after the next
// `welcome`. One that is not valid is acked: resending cannot fix it, and the ack drops it from
// the extension's queue.
fn is_acked(logged: &Result<Logged, EventError>) -> bool {
    match logged {
        Ok(
            Logged::Appended { .. }
            | Logged::Opened { .. }
            | Logged::Duplicate
            | Logged::Withheld { .. }
            | Logged::Ended { .. },
        )
        | Err(
            EventError::InvalidSessionId(_)
            | EventError::InvalidEvent(_)
            | EventError::AppOnlyEvent,
        ) => true,
        Err(EventError::Write(_)) => false,
    }
}

/// What the app sends a welcomed port unasked: settings, debug mode, call status, snapshot requests,
/// shortcut press notices and debug report replies.
struct PortFeeds {
    meet_settings: watch::Receiver<MeetSettings>,
    /// `None` once the sender is gone.
    debug_mode: Option<watch::Receiver<DebugMode>>,
    call_status: watch::Receiver<CallStatusMessage>,
    session_requests: broadcast::Receiver<SessionRequest>,
    replies: mpsc::UnboundedReceiver<AppMessage>,
    // A closed sender would end every later wait at once.
    is_settings_open: bool,
    is_call_status_open: bool,
    is_session_request_open: bool,
}

impl PortFeeds {
    fn new(
        calls: &BrowserCalls,
        debug_mode: watch::Receiver<DebugMode>,
        replies: mpsc::UnboundedReceiver<AppMessage>,
    ) -> Self {
        Self {
            meet_settings: calls.meet_settings.subscribe(),
            debug_mode: Some(debug_mode),
            call_status: calls.call_status.subscribe(),
            session_requests: calls.session_requests.subscribe(),
            replies,
            is_settings_open: true,
            is_call_status_open: true,
            is_session_request_open: true,
        }
    }

    /// The next message to send. Every wait in it can be dropped without losing a message.
    async fn next(&mut self) -> AppMessage {
        loop {
            tokio::select! {
                changed = self.meet_settings.changed(), if self.is_settings_open => {
                    if changed.is_err() {
                        self.is_settings_open = false;
                        continue;
                    }
                    let settings = (*self.meet_settings.borrow_and_update()).into();
                    return AppMessage::Settings { settings };
                }
                mode = next_debug_mode(&mut self.debug_mode) => {
                    return AppMessage::DebugMode {
                        until: mode.enabled_until(),
                    };
                }
                changed = self.call_status.changed(), if self.is_call_status_open => {
                    if changed.is_err() {
                        self.is_call_status_open = false;
                        continue;
                    }
                    return AppMessage::CallStatus(self.call_status.borrow_and_update().clone());
                }
                request = self.session_requests.recv(), if self.is_session_request_open => {
                    match request {
                        Ok(SessionRequest::PageSnapshot(session_ids)) => {
                            return AppMessage::SnapshotRequest { session_ids };
                        }
                        Ok(SessionRequest::ShortcutPressExpected(session_id)) => {
                            return AppMessage::ShortcutPressExpected { session_id };
                        }
                        // A missed press notice only means that press waits out its reply.
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::warn!(
                                browser_call.missed_requests = missed,
                                "dropped requests to mc-meet"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            self.is_session_request_open = false;
                        }
                    }
                }
                Some(reply) = self.replies.recv() => return reply,
                else => return std::future::pending().await,
            }
        }
    }
}

/// The next debug mode change. Never ends once the sender is gone.
async fn next_debug_mode(modes: &mut Option<watch::Receiver<DebugMode>>) -> DebugMode {
    if let Some(receiver) = modes {
        if receiver.changed().await.is_ok() {
            return *receiver.borrow_and_update();
        }
        *modes = None;
    }
    std::future::pending().await
}

/// Sends `welcome`, the debug mode and the current call status, which a port needs before anything else.
async fn welcome(
    to_host: &mut (impl AsyncWrite + Unpin),
    app: &AppHandle,
    feeds: &mut PortFeeds,
    open_sessions: Vec<String>,
) -> std::io::Result<()> {
    let welcome = AppMessage::Welcome {
        protocol: PROTOCOL,
        app_version: env!("CARGO_PKG_VERSION"),
        folder_version: crate::meet_extension::folder_version(app),
        open_sessions,
        settings: (*feeds.meet_settings.borrow_and_update()).into(),
    };
    send(to_host, &welcome).await?;
    let debug_mode = feeds
        .debug_mode
        .as_mut()
        .map_or(DebugMode::Off, |modes| *modes.borrow_and_update());
    send(
        to_host,
        &AppMessage::DebugMode {
            until: debug_mode.enabled_until(),
        },
    )
    .await?;
    let status = feeds.call_status.borrow_and_update().clone();
    send(to_host, &AppMessage::CallStatus(status)).await
}

async fn read_next_frame(
    mut from_host: ReadHalf<AppEnd>,
) -> (ReadHalf<AppEnd>, std::io::Result<Option<Vec<u8>>>) {
    let frame = read_frame(
        &mut from_host,
        LengthOrder::Little,
        FrameFlow::FromExtension,
    )
    .await;
    (from_host, frame)
}

async fn send(
    to_host: &mut (impl AsyncWrite + Unpin),
    message: &AppMessage,
) -> std::io::Result<()> {
    let frame = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    write_frame(to_host, LengthOrder::Little, FrameFlow::ToExtension, &frame).await
}

/// Why a `video_fragment` could not become a `TabFragment`.
#[derive(Debug, thiserror::Error)]
enum FragmentDecodeError {
    #[error("{field} is not valid base64: {source}")]
    Base64 {
        field: &'static str,
        source: base64::DecodeError,
    },
    #[error("first_frame_utc_ms {utc_ms} is not a time")]
    FirstFrameTime { utc_ms: f64 },
}

fn timestamp_of_utc_ms(utc_ms: f64) -> Option<Timestamp> {
    // Past year 9999 jiff refuses the time anyway; the bound keeps the cast exact.
    const LIMIT_MS: f64 = 253_402_300_800_000.0;
    if !utc_ms.is_finite() || utc_ms.abs() > LIMIT_MS {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is finite and checked against jiff's range above"
    )]
    let nanoseconds = (utc_ms * 1e6).round() as i128;
    Timestamp::from_nanosecond(nanoseconds).ok()
}

// A browser that quit or crashed sends no `call_ended`, so a timer ends its calls.
async fn end_idle_calls(app: AppHandle, pipeline: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(IDLE_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        let ended = {
            let state = app.state::<AppState>();
            let mut book = state.browser_calls.lock();
            let ended = book.end_idle_calls(Now::read());
            // Under the book lock, so a late event cannot reopen the call before this ends it.
            for log_path in &ended {
                state.calls.end(log_path, CallEndReason::Idle);
                state.call_debug.forget_lead_up(log_path);
            }
            ended
        };
        if ended.is_empty() {
            continue;
        }
        crate::tray::refresh(&app);
        for log_path in ended {
            let (app, pipeline) = (app.clone(), Arc::clone(&pipeline));
            tauri::async_runtime::spawn_blocking(move || {
                record_and_run(&app, &pipeline, &log_path);
            });
        }
    }
}

// No minimum length: the caption count alone tells a real call from a misclick.
fn record_and_run(app: &AppHandle, pipeline: &Pipeline, log_path: &Path) {
    finished_logs::record_and_run(app, pipeline, LogSource::MeetCall, log_path, 0);
}

// Under the book lock, so the call stream sees opens and ends in the book's order.
fn open_on_stream(state: &AppState, opened: &OpenedCall) {
    state.calls.open(
        &opened.log_path,
        CallPlatform::Meet,
        opened.title.clone(),
        None,
        opened.is_recording_stopped,
    );
    if !opened.logged_caption_keys.is_empty() {
        state.calls.status().observe(
            &opened.log_path,
            Observation::MeetLoggedCaptions(opened.logged_caption_keys.clone()),
            Moment::now(),
        );
    }
}
