//! Event names and the field names detectors read, spelled once. A `tracing` macro needs a
//! literal, so a call site spells the name itself; code that reads lines uses these constants.

pub const CALL_STARTED: &str = "call.started";
pub const CALL_RESUMED: &str = "call.resumed";
pub const CALL_ENDED: &str = "call.ended";
pub const CALL_RECORDING_STOPPED: &str = "call.recording_stopped";
pub const CALL_RECORDING_RESUMED: &str = "call.recording_resumed";
pub const CALL_CAPTIONS: &str = "call.captions";
pub const CALL_TAB_VIDEO_RECORDED: &str = "call.tab_video_recorded";
pub const CALL_PROBLEM: &str = "call.problem";
pub const CALL_RECOVERED: &str = "call.recovered";
pub const CALL_PROBLEM_NOTICED: &str = "call.problem_noticed";

pub const MEET_HEALTH: &str = "meet.health";
pub const MEET_PORT_HELD: &str = "meet.port_held";
pub const MEET_PAGES_AWAY: &str = "meet.pages_away";
pub const EXTENSION_CONNECTION: &str = "extension.connection";
pub const HUDDLE_HEALTH: &str = "huddle.health";
pub const HUDDLE_PERMISSION: &str = "huddle.permission";
pub const HOST_SUSPENDED: &str = "host.suspended";

pub const CAPTURE_AUDIO_RECORDING: &str = "capture.audio.recording";
pub const CAPTURE_AUDIO_GAP: &str = "capture.audio.gap";
pub const CAPTURE_AUDIO_ROTATED: &str = "capture.audio.rotated";
pub const CAPTURE_AUDIO_SEGMENT_CLOSED: &str = "capture.audio.segment_closed";
pub const CAPTURE_AUDIO_DEVICE_CHANGED: &str = "capture.audio.device_changed";
pub const CAPTURE_AUDIO_MIC_UNAVAILABLE: &str = "capture.audio.mic_unavailable";
pub const CAPTURE_AUDIO_PERMISSION: &str = "capture.audio.permission";
pub const CAPTURE_AUDIO_SLEEP: &str = "capture.audio.sleep";
pub const CAPTURE_AUDIO_WAKE: &str = "capture.audio.wake";
pub const CAPTURE_AUDIO_ERROR: &str = "capture.audio.error";
pub const CAPTURE_AUDIO_STREAM_LOST: &str = "capture.audio.stream_lost";

pub const CAPTURE_VIDEO_RECORDING: &str = "capture.video.recording";
pub const CAPTURE_VIDEO_GAP: &str = "capture.video.gap";
pub const CAPTURE_VIDEO_WINDOW_CHANGED: &str = "capture.video.window_changed";
pub const CAPTURE_VIDEO_PERMISSION: &str = "capture.video.permission";
pub const CAPTURE_VIDEO_SEGMENT_CLOSED: &str = "capture.video.segment_closed";
pub const CAPTURE_VIDEO_ERROR: &str = "capture.video.error";
pub const CAPTURE_VIDEO_STREAM_LOST: &str = "capture.video.stream_lost";

pub const CAPTURE_PROCESS_FAILED: &str = "capture.process_failed";

/// Every log line a detector reads. The live tap and `mc debug replay` keep these and nothing
/// else, so both feed the detectors the same lines.
pub const DETECTOR_EVENTS: &[&str] = &[
    CALL_STARTED,
    CALL_RESUMED,
    CALL_ENDED,
    CALL_RECORDING_STOPPED,
    CALL_RECORDING_RESUMED,
    CALL_CAPTIONS,
    MEET_HEALTH,
    MEET_PORT_HELD,
    MEET_PAGES_AWAY,
    EXTENSION_CONNECTION,
    HUDDLE_HEALTH,
    HOST_SUSPENDED,
    CAPTURE_AUDIO_RECORDING,
    CAPTURE_AUDIO_GAP,
    CAPTURE_AUDIO_ROTATED,
    CAPTURE_AUDIO_SEGMENT_CLOSED,
    CAPTURE_AUDIO_DEVICE_CHANGED,
    CAPTURE_AUDIO_MIC_UNAVAILABLE,
    CAPTURE_AUDIO_PERMISSION,
    CAPTURE_AUDIO_SLEEP,
    CAPTURE_AUDIO_WAKE,
    CAPTURE_AUDIO_ERROR,
    CAPTURE_AUDIO_STREAM_LOST,
    CAPTURE_VIDEO_RECORDING,
    CAPTURE_VIDEO_GAP,
    CAPTURE_VIDEO_WINDOW_CHANGED,
    CAPTURE_VIDEO_PERMISSION,
    CAPTURE_VIDEO_SEGMENT_CLOSED,
    CAPTURE_VIDEO_ERROR,
    CAPTURE_VIDEO_STREAM_LOST,
    CAPTURE_PROCESS_FAILED,
];

/// Detector lines that belong to no call. A call's report keeps them inside the call's span.
pub const GLOBAL_DETECTOR_EVENTS: &[&str] = &[EXTENSION_CONNECTION, HOST_SUSPENDED];

/// The board's own verdict lines. `mc debug replay` compares its firings with them.
pub const PROBLEM_EVENTS: &[&str] = &[CALL_PROBLEM, CALL_RECOVERED, CALL_PROBLEM_NOTICED];

pub const EVENT_NAME: &str = "event.name";
pub const CALL_ID: &str = "call.id";
pub const CALL_PLATFORM: &str = "call.platform";
pub const CALL_CAPTION_COUNT: &str = "call.caption_count";
pub const CAPTION_LAST_AT: &str = "mc.caption.last_at";
pub const CAPTION_IS_BACKLOG: &str = "mc.caption.is_backlog";
pub const MEET_CAPTIONS: &str = "mc.meet.captions";
pub const MEET_CAPTION_SOURCE: &str = "mc.meet.caption_source";
pub const MEET_IS_CAPTION_FRAMES_FAILING: &str = "mc.meet.is_caption_frames_failing";
pub const MEET_IS_PORT_HELD: &str = "mc.meet.is_port_held";
pub const MEET_IS_PAGES_AWAY: &str = "mc.meet.is_pages_away";
pub const EXTENSION_IS_CONNECTED: &str = "browser.extension.is_connected";
pub const HUDDLE_HEALTH_FIELD: &str = "mc.huddle.health";
pub const HUDDLE_IS_PERMISSION_MISSING: &str = "mc.huddle.is_permission_missing";
pub const HOST_SUSPENDED_MS: &str = "mc.host.suspended_ms";
pub const AUDIO_STREAM: &str = "audio.stream";
pub const AUDIO_CHANGE_CAUSE: &str = "audio.change_cause";
pub const VIDEO_REBUILD_CAUSE: &str = "video.rebuild_cause";
pub const PERMISSION_SERVICE: &str = "permission.service";
pub const PERMISSION_STATE: &str = "permission.state";
pub const ERROR_TYPE: &str = "error.type";
pub const CAPTURE_HELPER: &str = "mc.capture.helper";
pub const CAPTURE_IS_FAILING: &str = "capture.is_failing";
pub const PROBLEM_LAYER: &str = "mc.problem.layer";
pub const PROBLEM_DETECTOR: &str = "mc.problem.detector";
pub const PROBLEM_EVIDENCE: &str = "mc.problem.evidence";
pub const PROBLEM_EVIDENCE_AT: &str = "mc.problem.evidence_at";
pub const AUDIO_LEVEL_PEAK: &str = "mc.audio.level.peak";
pub const AUDIO_STEM: &str = "mc.audio.stem";
pub const AUDIO_SELF_SPEAKING: &str = "mc.audio.self_speaking";
