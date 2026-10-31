//! Device, power and video events a debug build injects on a timetable, so the device rules and the
//! sleep path run without touching the user's devices or sleeping the Mac. Set
//! `MC_CAPTURE_INJECT` to a list such as `5:mic-lost,12:tap-stall,30:sleep,32:wake`: seconds
//! since the process started, then the event. `0:screen-recording-refused` makes the video
//! preflight read Screen Recording as denied from then on, and `screen-recording-restored` lets
//! the real state count again, so the warning runs without revoking the grant. Release builds do
//! not contain this module.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use capture::{PermissionState, PowerEvent, SleepAck};
use video_capture::mac::stream::StreamEvent;

const SCHEDULE_VARIABLE: &str = "MC_CAPTURE_INJECT";

/// Events the record loop applies itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceEvent {
    /// The listeners' flags, as if the HAL had reported the change.
    DefaultInput,
    DefaultOutput,
    DeviceList,
    /// The recording mic's stream reports its device gone.
    MicLost,
    /// The mic delivers exact zeros from now on, as a muted mic does.
    MicZeros,
    /// The mic delivers what its device hears again.
    MicSound,
    /// The lid reads closed from now on, or open again.
    LidClosed,
    LidOpen,
    /// The next new mic stream delivers no buffers.
    CandidateNoBuffers,
    /// The next tap to open delivers no buffers. At 0 s it is the segment's first tap.
    TapSilentOpen,
    /// The tap's format listener reports a rate change.
    TapFormat,
    /// The tap's device stops, followed by an output route change.
    TapStall,
    /// The tap's device stops with no route change: a rebuild follows after 5 s.
    TapStallQuiet,
    /// The tap's aggregate is destroyed under its running IO proc.
    TapDead,
    /// A private aggregate appears and goes, so the real device-list listener fires.
    HalProbe,
    /// The video stream's callbacks are dropped from now on, as a hung stream sends none.
    VideoStall,
    /// The video stream sends one idle status and then nothing, as ScreenCaptureKit does for a
    /// window whose picture stops changing.
    VideoStill,
    /// The video stream's callbacks arrive again.
    VideoResume,
}

const DEVICE_EVENTS_ALL: [DeviceEvent; 18] = [
    DeviceEvent::DefaultInput,
    DeviceEvent::DefaultOutput,
    DeviceEvent::DeviceList,
    DeviceEvent::MicLost,
    DeviceEvent::MicZeros,
    DeviceEvent::MicSound,
    DeviceEvent::LidClosed,
    DeviceEvent::LidOpen,
    DeviceEvent::CandidateNoBuffers,
    DeviceEvent::TapSilentOpen,
    DeviceEvent::TapFormat,
    DeviceEvent::TapStall,
    DeviceEvent::TapStallQuiet,
    DeviceEvent::TapDead,
    DeviceEvent::HalProbe,
    DeviceEvent::VideoStall,
    DeviceEvent::VideoStill,
    DeviceEvent::VideoResume,
];

impl fmt::Display for DeviceEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DefaultInput => "default-input",
            Self::DefaultOutput => "default-output",
            Self::DeviceList => "device-list",
            Self::MicLost => "mic-lost",
            Self::MicZeros => "mic-zeros",
            Self::MicSound => "mic-sound",
            Self::LidClosed => "lid-closed",
            Self::LidOpen => "lid-open",
            Self::CandidateNoBuffers => "candidate-no-buffers",
            Self::TapSilentOpen => "tap-silent-open",
            Self::TapFormat => "tap-format",
            Self::TapStall => "tap-stall",
            Self::TapStallQuiet => "tap-stall-quiet",
            Self::TapDead => "tap-dead",
            Self::HalProbe => "hal-probe",
            Self::VideoStall => "video-stall",
            Self::VideoStill => "video-still",
            Self::VideoResume => "video-resume",
        })
    }
}

/// Power events, sent from their own thread as the sleep watch would send them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerInjection {
    Sleep,
    Wake,
    /// A wake without graphics. The sleep watch then waits for the real full wake.
    DarkWake,
}

const POWER_INJECTIONS_ALL: [PowerInjection; 3] = [
    PowerInjection::Sleep,
    PowerInjection::Wake,
    PowerInjection::DarkWake,
];

impl fmt::Display for PowerInjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Sleep => "sleep",
            Self::Wake => "wake",
            Self::DarkWake => "dark-wake",
        })
    }
}

/// What the video preflight reads for Screen Recording, from the event's time on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionInjection {
    ScreenRecordingRefused,
    /// The real state counts again.
    ScreenRecordingRestored,
}

const PERMISSION_INJECTIONS_ALL: [PermissionInjection; 2] = [
    PermissionInjection::ScreenRecordingRefused,
    PermissionInjection::ScreenRecordingRestored,
];

impl fmt::Display for PermissionInjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ScreenRecordingRefused => "screen-recording-refused",
            Self::ScreenRecordingRestored => "screen-recording-restored",
        })
    }
}

enum ScheduledEvent {
    Device(DeviceEvent),
    Power(PowerInjection),
    Permission(PermissionInjection),
}

static DEVICE_EVENTS: Mutex<Vec<(Instant, DeviceEvent)>> = Mutex::new(Vec::new());
static PERMISSION_EVENTS: Mutex<Vec<(Instant, PermissionInjection)>> = Mutex::new(Vec::new());
static IS_MIC_ZEROED: AtomicBool = AtomicBool::new(false);
static IS_CANDIDATE_SILENCED: AtomicBool = AtomicBool::new(false);
static IS_TAP_SILENCED: AtomicBool = AtomicBool::new(false);
static VIDEO_FEED: Mutex<VideoFeed> = Mutex::new(VideoFeed::Live);
/// 0 while the real lid state counts, 1 for open, 2 for closed.
static LID_STATE: AtomicU8 = AtomicU8::new(0);

/// Reads the timetable once at process start. Power events go to `on_power` from their own
/// thread; device events wait for `take_due_device_events`.
pub(crate) fn start(on_power: impl Fn(PowerEvent) + Send + 'static) {
    let Ok(schedule) = std::env::var(SCHEDULE_VARIABLE) else {
        return;
    };
    let started = Instant::now();
    let mut power = Vec::new();
    let mut device = Vec::new();
    let mut permission = Vec::new();
    for entry in schedule
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let parsed = entry
            .split_once(':')
            .and_then(|(seconds, name)| Some((seconds.parse::<f64>().ok()?, parse_event(name)?)));
        let Some((seconds, event)) = parsed else {
            tracing::warn!(
                inject.entry = entry,
                inject.valid_events = %valid_names(),
                "inject.bad_entry"
            );
            continue;
        };
        let at = started + Duration::from_secs_f64(seconds.max(0.0));
        match event {
            // The first tap opens before the record loop applies any event.
            ScheduledEvent::Device(DeviceEvent::TapSilentOpen) if seconds <= 0.0 => {
                silence_next_tap();
            }
            ScheduledEvent::Device(event) => device.push((at, event)),
            ScheduledEvent::Power(event) => power.push((at, event)),
            ScheduledEvent::Permission(event) => permission.push((at, event)),
        }
    }
    tracing::info!(inject.schedule = schedule, "inject.scheduled");
    *DEVICE_EVENTS.lock().unwrap_or_else(PoisonError::into_inner) = device;
    *PERMISSION_EVENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = permission;
    if power.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("inject-power".to_owned())
        .spawn(move || {
            for (at, event) in power {
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                tracing::info!(inject.event = %event, "inject.power");
                match event {
                    PowerInjection::Sleep => on_power(PowerEvent::WillSleep(SleepAck::detached())),
                    PowerInjection::Wake => on_power(PowerEvent::Woke),
                    PowerInjection::DarkWake => capture::inject_powered_on(false),
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(exception.message = %error, "inject.thread_failed");
    }
}

/// The device events whose time has come, in order.
pub(crate) fn take_due_device_events() -> Vec<DeviceEvent> {
    let mut events = DEVICE_EVENTS.lock().unwrap_or_else(PoisonError::into_inner);
    if events.is_empty() {
        return Vec::new();
    }
    let now = Instant::now();
    let (due, later): (Vec<_>, Vec<_>) = events.drain(..).partition(|(at, _)| *at <= now);
    *events = later;
    due.into_iter().map(|(_, event)| event).collect()
}

/// The Screen Recording state the latest due injection sets, or `None` while the real one counts.
pub(crate) fn screen_recording_state() -> Option<PermissionState> {
    let events = PERMISSION_EVENTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    let latest = events
        .iter()
        .filter(|(at, _)| *at <= now)
        .max_by_key(|(at, _)| *at)?;
    match latest.1 {
        PermissionInjection::ScreenRecordingRefused => Some(PermissionState::Denied),
        PermissionInjection::ScreenRecordingRestored => None,
    }
}

pub(crate) fn set_mic_zeroed(is_zeroed: bool) {
    IS_MIC_ZEROED.store(is_zeroed, Ordering::Relaxed);
}

pub(crate) fn is_mic_zeroed() -> bool {
    IS_MIC_ZEROED.load(Ordering::Relaxed)
}

pub(crate) fn silence_next_candidate() {
    IS_CANDIDATE_SILENCED.store(true, Ordering::Relaxed);
}

/// Whether this new mic stream should deliver nothing. Applies to one stream.
pub(crate) fn take_candidate_silenced() -> bool {
    IS_CANDIDATE_SILENCED.swap(false, Ordering::Relaxed)
}

pub(crate) fn silence_next_tap() {
    IS_TAP_SILENCED.store(true, Ordering::Relaxed);
}

/// Whether this new tap should deliver nothing. Applies to one tap.
pub(crate) fn take_tap_silenced() -> bool {
    IS_TAP_SILENCED.swap(false, Ordering::Relaxed)
}

/// What the video stream's callbacks reach the loop as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoFeed {
    Live,
    /// Every callback but a stop is dropped.
    Stalled,
    /// The next callback arrives as an idle status, then every one but a stop is dropped.
    Still {
        is_idle_sent: bool,
    },
}

impl VideoFeed {
    pub(crate) fn still() -> Self {
        Self::Still {
            is_idle_sent: false,
        }
    }
}

pub(crate) fn set_video_feed(feed: VideoFeed) {
    *VIDEO_FEED.lock().unwrap_or_else(PoisonError::into_inner) = feed;
}

/// The callback as the injected feed lets it through, if at all. Runs on the stream's queue. A
/// stop always passes, as a hung stream still reports one.
pub(crate) fn pass_video_event(event: StreamEvent) -> Option<StreamEvent> {
    if matches!(event, StreamEvent::Stopped(_)) {
        return Some(event);
    }
    let mut feed = VIDEO_FEED.lock().unwrap_or_else(PoisonError::into_inner);
    match *feed {
        VideoFeed::Live => Some(event),
        VideoFeed::Stalled | VideoFeed::Still { is_idle_sent: true } => None,
        VideoFeed::Still {
            is_idle_sent: false,
        } => {
            *feed = VideoFeed::Still { is_idle_sent: true };
            Some(StreamEvent::Idle)
        }
    }
}

pub(crate) fn set_lid_closed(is_closed: bool) {
    LID_STATE.store(if is_closed { 2 } else { 1 }, Ordering::Relaxed);
}

/// The injected lid state, or `None` while the real one counts.
pub(crate) fn lid_closed() -> Option<bool> {
    match LID_STATE.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

fn valid_names() -> String {
    let device = DEVICE_EVENTS_ALL.iter().map(ToString::to_string);
    let power = POWER_INJECTIONS_ALL.iter().map(ToString::to_string);
    let permission = PERMISSION_INJECTIONS_ALL.iter().map(ToString::to_string);
    device
        .chain(power)
        .chain(permission)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_event(name: &str) -> Option<ScheduledEvent> {
    if let Some(&event) = DEVICE_EVENTS_ALL
        .iter()
        .find(|event| event.to_string() == name)
    {
        return Some(ScheduledEvent::Device(event));
    }
    if let Some(&event) = POWER_INJECTIONS_ALL
        .iter()
        .find(|event| event.to_string() == name)
    {
        return Some(ScheduledEvent::Power(event));
    }
    PERMISSION_INJECTIONS_ALL
        .iter()
        .find(|event| event.to_string() == name)
        .map(|&event| ScheduledEvent::Permission(event))
}
