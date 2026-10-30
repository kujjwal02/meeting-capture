//! Records each call through `mc-capture` child processes that own the devices and the media
//! files. The app never links Core Audio, so a crash or a hung HAL call in a capture process
//! never takes the tray down. Audio is the one capture process today; video joins in phase 3c.
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
//! so and the supervisor starts the call's next segment in it. The helper's permission reports
//! feed the tray: a refused microphone or system audio permission shows as a problem.

mod restart_backoff;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use capture_protocol::{
    AudioCommand, AudioErrorKind, AudioReport, DeviceChangeCause, MicMoment, PermissionService,
    PermissionState, Platform, Stem,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tauri::{AppHandle, Manager as _};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
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
/// The helper log moves to `mc-capture.log.1` at a start once it is this big.
const SIDECAR_LOG_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(windows)]
const SIDECAR_FILE_NAME: &str = "mc-capture.exe";
#[cfg(not(windows))]
const SIDECAR_FILE_NAME: &str = "mc-capture";
/// After a device change ends a backoff early, the wait before the restart: Bluetooth routes
/// take this long to settle.
const DEVICE_CHANGE_SETTLE: Duration = Duration::from_millis(1_500);
/// The video process's manifest, which phase 3c writes beside `audio.json`.
const VIDEO_MANIFEST_FILE_NAME: &str = "video.json";

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

/// The supervisor's wake-up for each stop or resume, and each open call's audio state.
pub(crate) struct AudioCapture {
    /// The call whose stop or resume the board just took.
    stop_changes: mpsc::UnboundedSender<PathBuf>,
    /// Taken once, by `start`.
    stop_change_receiver: Mutex<Option<mpsc::UnboundedReceiver<PathBuf>>>,
    states: Mutex<HashMap<PathBuf, AudioState>>,
    /// The last state the helper read for each permission.
    permissions: Mutex<HashMap<PermissionService, PermissionState>>,
}

impl AudioCapture {
    pub fn new() -> Self {
        let (stop_changes, stop_change_receiver) = mpsc::unbounded_channel();
        Self {
            stop_changes,
            stop_change_receiver: Mutex::new(Some(stop_change_receiver)),
            states: Mutex::new(HashMap::new()),
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

    /// Whether any call fails to record, or records without the user's mic.
    pub fn has_problem(&self) -> bool {
        self.lock().values().any(|state| {
            matches!(
                state,
                AudioState::Failing | AudioState::RecordingWithoutMic { .. }
            )
        })
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
    let _unheard = state.audio_capture.stop_changes.send(call_id.to_path_buf());
    crate::tray::show_call_status(app);
}

/// Starts the supervisor. Call before any call source starts, so it hears every call.
pub(crate) fn start(app: &AppHandle) {
    let state = app.state::<AppState>();
    let stop_changes = state
        .audio_capture
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
    let app_dirs = state.settings.app_dirs();
    let supervisor = Supervisor {
        app: app.clone(),
        calls_dir: app_dirs.calls_dir(),
        log_file: app_dirs.log_dir().join(SIDECAR_LOG_FILE_NAME),
        open_calls: Vec::new(),
        active: None,
    };
    tauri::async_runtime::spawn(supervisor.run(events, stop_changes));
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
}

struct Supervisor {
    app: AppHandle,
    calls_dir: PathBuf,
    log_file: PathBuf,
    /// Open calls, oldest first.
    open_calls: Vec<Call>,
    active: Option<ActiveRecording>,
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
                            "audio capture missed call events; reading the open calls"
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
                self.audio().set_state(&self.app, &call.id, state);
                self.open_calls.push(call);
            }
            CallEvent::Ended { call, .. } => {
                self.forget_call(&call.id).await;
            }
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
        self.audio().set_state(&self.app, call_id, None);
    }

    /// The board took a stop or a resume of `call_id`.
    async fn take_stop_change(&mut self, call_id: &Path) {
        if !self.open_calls.iter().any(|call| call.id == call_id) {
            return;
        }
        if self.is_stopped_by_user(call_id) {
            self.stop_active_if(call_id).await;
            self.audio()
                .set_state(&self.app, call_id, Some(AudioState::StoppedByUser));
        } else if !self.is_active(call_id) {
            // `start_next` starts it at once with the call's next segment number.
            self.audio().set_state(&self.app, call_id, None);
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

    fn audio(&self) -> &AudioCapture {
        &self.app.state::<AppState>().inner().audio_capture
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
            // Phase 3c: the video process starts here too, with the same segment number.
            let processes = vec![self.spawn_process::<AudioProcess>(&call, &folder, segment)];
            self.active = Some(ActiveRecording {
                call_id: call.id,
                processes,
            });
        }
        let active_id = self.active.as_ref().map(|active| active.call_id.clone());
        for call in &self.open_calls {
            if Some(&call.id) != active_id.as_ref() && !self.is_stopped_by_user(&call.id) {
                self.audio()
                    .set_state(&self.app, &call.id, Some(AudioState::Queued));
            }
        }
    }

    fn spawn_process<P: CaptureProcess>(
        &self,
        call: &Call,
        folder: &Path,
        segment: u32,
    ) -> RunningProcess {
        let (stop, stop_receiver) = oneshot::channel();
        let recording = Recording::<P> {
            app: self.app.clone(),
            call: call.clone(),
            folder: folder.to_path_buf(),
            log_file: self.log_file.clone(),
            segment,
            backoff: RestartBackoff::default(),
            process: std::marker::PhantomData,
        };
        let span = tracing::info_span!(
            "capture.recording",
            call.id = %call.id.display(),
            process.command = P::SUBCOMMAND
        );
        let task = tauri::async_runtime::spawn(recording.run(stop_receiver).instrument(span));
        RunningProcess { stop, task }
    }
}

/// The call folder's name: the call log's file name without its extension, which is unique per
/// call.
fn call_folder_name(call_id: &Path) -> String {
    call_id.file_stem().map_or_else(
        || "call".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

/// One above every segment any capture process used in the folder: in `audio.json`, in
/// `video.json`, and in every `NNN-` file name, which stem-writer's scan covers.
fn next_call_segment(folder: &Path) -> Result<u32, String> {
    let from_audio = stem_writer::next_segment_number(folder)
        .map_err(|error| format!("could not read the call folder: {error}"))?;
    Ok(from_audio.max(highest_video_segment(folder)? + 1))
}

fn highest_video_segment(folder: &Path) -> Result<u32, String> {
    #[derive(serde::Deserialize)]
    struct VideoManifest {
        segments: Vec<VideoSegment>,
    }
    #[derive(serde::Deserialize)]
    struct VideoSegment {
        number: u32,
    }
    let path = folder.join(VIDEO_MANIFEST_FILE_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let manifest: VideoManifest = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not valid: {error}", path.display()))?;
    Ok(manifest
        .segments
        .iter()
        .map(|segment| segment.number)
        .max()
        .unwrap_or(0))
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
    /// The segment is closed and its manifest says so.
    Closed,
    /// A device did not open. Restarts back off further.
    DeviceOpenFailed,
    Permission {
        service: PermissionService,
        state: PermissionState,
    },
    /// The mic is gone and no safe input is left; the mic stem records a gap.
    MicUnavailable {
        segment: u32,
    },
    /// The mic moved to another device, which ends a time without one.
    MicChanged {
        segment: u32,
    },
    /// The Mac is about to sleep. Any open segment is already closed.
    Asleep,
    /// The Mac woke. The process waits for the next `start`.
    Woke,
    /// Nothing the supervisor acts on.
    Logged,
}

/// One kind of `mc-capture` child process: audio now, video in phase 3c.
trait CaptureProcess: Send + 'static {
    type Command: Serialize + Send + Sync;
    type Report: DeserializeOwned;
    /// The `mc-capture` subcommand that runs it.
    const SUBCOMMAND: &'static str;

    fn start_command(call: &Call, folder: &Path, segment: u32) -> Self::Command;
    fn stop_command() -> Self::Command;
    /// The lowest segment number the process may start in the folder.
    fn next_free_segment(folder: &Path) -> Result<u32, String>;
    /// The files whose growth shows the process still writes `segment`.
    fn segment_files(folder: &Path, segment: u32) -> Vec<PathBuf>;
    /// Logs the report and says what it means for the supervisor.
    fn read_report(call_id: &Path, report: &Self::Report) -> ReportSign;
    /// Shows the process's state for the call in the tray. Takes `AudioState` because audio is
    /// the only process with a tray line; phase 3c keys the states by process when video adds
    /// its own line.
    fn show_state(app: &AppHandle, call_id: &Path, state: Option<AudioState>);
}

struct AudioProcess;

impl CaptureProcess for AudioProcess {
    type Command = AudioCommand;
    type Report = AudioReport;
    const SUBCOMMAND: &'static str = "audio";

    fn start_command(call: &Call, folder: &Path, segment: u32) -> AudioCommand {
        AudioCommand::Start {
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
        }
    }

    fn stop_command() -> AudioCommand {
        AudioCommand::Stop
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

    fn read_report(call_id: &Path, report: &AudioReport) -> ReportSign {
        log_audio_report(call_id, report);
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
            AudioReport::Stopped { .. } => ReportSign::Closed,
            AudioReport::Error {
                kind: AudioErrorKind::DeviceOpenFailed,
                ..
            } => ReportSign::DeviceOpenFailed,
            AudioReport::Permission { service, state } => ReportSign::Permission {
                service: *service,
                state: *state,
            },
            AudioReport::SystemSleep { .. } => ReportSign::Asleep,
            AudioReport::SystemWake => ReportSign::Woke,
            AudioReport::MicUnavailable { segment, .. } => {
                ReportSign::MicUnavailable { segment: *segment }
            }
            AudioReport::DeviceChanged {
                segment,
                stream: Stem::Mic,
                ..
            } => ReportSign::MicChanged { segment: *segment },
            AudioReport::Gap { .. }
            | AudioReport::DeviceChanged {
                stream: Stem::Remote,
                ..
            }
            | AudioReport::Error { .. } => ReportSign::Logged,
        }
    }

    fn show_state(app: &AppHandle, call_id: &Path, state: Option<AudioState>) {
        app.state::<AppState>()
            .audio_capture
            .set_state(app, call_id, state);
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
}

impl<P: CaptureProcess> Recording<P> {
    async fn run(mut self, mut stop: oneshot::Receiver<()>) {
        loop {
            let end = self.run_once(&mut stop).await;
            let (reason, is_device_open_failed) = match end {
                ProcessEnd::Stopped => {
                    tracing::info!(call.id = %self.call.id.display(), "capture process stopped");
                    return;
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
            let state = if restart.is_failing {
                AudioState::Failing
            } else {
                AudioState::Restarting
            };
            P::show_state(&self.app, &self.call.id, Some(state));
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
    async fn run_once(&mut self, stop: &mut oneshot::Receiver<()>) -> ProcessEnd {
        let (mut child, stdin, stdout) = match self.launch().await {
            Ok(launched) => launched,
            Err(reason) => return failed(reason),
        };
        let pid = child.id().unwrap_or_default();
        let mut reports = BufReader::new(stdout).lines();
        let mut run = ProcessRun {
            stdin: Some(stdin),
            liveness: Liveness::new(self.segment),
            open_segment: Some(self.segment),
            has_closed_segment: false,
            is_device_open_failed: false,
            next_command_seq: 2,
        };
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
                        if let Err(error) = self.follow_report(&line, &mut run).await {
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

    /// Starts the process and sends it `start` for the current segment.
    async fn launch(&mut self) -> Result<(Child, ChildStdin, ChildStdout), String> {
        let segment = self.segment;
        let mut child = self.spawn_sidecar()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            kill(&mut child).await;
            return Err("the helper has no stdin or stdout pipe".to_owned());
        };
        P::show_state(
            &self.app,
            &self.call.id,
            Some(AudioState::Starting { segment }),
        );
        tracing::info!(
            call.id = %self.call.id.display(),
            process.pid = child.id().unwrap_or_default(),
            segment.number = segment,
            file.directory = %self.folder.display(),
            "capture process started"
        );
        let start = P::start_command(&self.call, &self.folder, segment);
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
        let sign = P::read_report(&self.call.id, &report);
        match sign {
            ReportSign::Recording { segment } => {
                self.backoff.note_recording();
                P::show_state(
                    &self.app,
                    &self.call.id,
                    Some(self.recording_state(segment, false)),
                );
            }
            ReportSign::Progress {
                segment,
                counter_total,
            } => liveness.note_progress(segment, counter_total),
            ReportSign::Rotated { to_segment } => {
                self.segment = self.segment.max(to_segment);
                liveness.segment = to_segment;
                P::show_state(
                    &self.app,
                    &self.call.id,
                    Some(self.recording_state(to_segment, true)),
                );
            }
            ReportSign::Permission { service, state } => self
                .app
                .state::<AppState>()
                .audio_capture
                .set_permission(&self.app, service, state),
            ReportSign::Asleep => {
                P::show_state(&self.app, &self.call.id, Some(AudioState::Sleeping));
            }
            ReportSign::MicUnavailable { segment } => P::show_state(
                &self.app,
                &self.call.id,
                Some(AudioState::RecordingWithoutMic { segment }),
            ),
            ReportSign::MicChanged { segment } => P::show_state(
                &self.app,
                &self.call.id,
                Some(AudioState::Recording { segment }),
            ),
            ReportSign::Closed
            | ReportSign::DeviceOpenFailed
            | ReportSign::Woke
            | ReportSign::Logged => {}
        }
        Some(sign)
    }

    /// The state for stems open in `segment`. A segment that recorded without the user's mic
    /// stays so when its remote stem starts, and across a rotation, until the mic returns.
    fn recording_state(&self, segment: u32, is_rotation: bool) -> AudioState {
        let shown = self
            .app
            .state::<AppState>()
            .audio_capture
            .states()
            .get(&self.call.id)
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

    /// Takes one report line into the run. Fails only when a wake's `start` cannot be sent.
    async fn follow_report(&mut self, line: &str, run: &mut ProcessRun) -> std::io::Result<()> {
        match self.take_report(line, &mut run.liveness) {
            Some(ReportSign::Closed) => {
                run.has_closed_segment = true;
                run.open_segment = None;
            }
            Some(ReportSign::DeviceOpenFailed) => run.is_device_open_failed = true,
            Some(
                ReportSign::Recording { segment }
                | ReportSign::Rotated {
                    to_segment: segment,
                },
            ) => run.open_segment = Some(segment),
            // stdin is gone once a stop went out: then the wake starts nothing.
            Some(ReportSign::Woke) if run.open_segment.is_none() => {
                if let Some(stdin) = run.stdin.as_mut() {
                    let segment = self.next_segment_after_sleep();
                    let start = P::start_command(&self.call, &self.folder, segment);
                    send_command(stdin, run.next_command_seq, &start).await?;
                    tracing::info!(
                        segment.number = segment,
                        "started the next segment after a wake"
                    );
                    run.next_command_seq += 1;
                    run.open_segment = Some(segment);
                    run.has_closed_segment = false;
                    run.liveness = Liveness::new(segment);
                    self.segment = segment;
                    P::show_state(
                        &self.app,
                        &self.call.id,
                        Some(AudioState::Starting { segment }),
                    );
                }
            }
            Some(
                ReportSign::Progress { .. }
                | ReportSign::Permission { .. }
                | ReportSign::MicUnavailable { .. }
                | ReportSign::MicChanged { .. }
                | ReportSign::Asleep
                | ReportSign::Woke
                | ReportSign::Logged,
            )
            | None => {}
        }
        Ok(())
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
}

impl ProcessRun {
    /// Sends `stop` to an open segment, then closes stdin: after the segment closes, the
    /// process exits. With no segment open, closing stdin alone ends it.
    async fn send_stop<P: CaptureProcess>(&mut self) {
        let Some(mut stdin) = self.stdin.take() else {
            return;
        };
        if self.open_segment.is_some()
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
