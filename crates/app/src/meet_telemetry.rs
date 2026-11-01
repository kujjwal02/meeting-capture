//! mc-meet's telemetry: once a second in a Meet call, caption channel counters and lifecycle and
//! press events; in debug mode, also the raw caption frames. The app builds the counters' metric
//! row from typed fields, so no page text can reach a point key. Rows go to the call's
//! `metrics.jsonl`, events to the app log as `tracing` events with target `mc_meet`, and frames
//! to the call's `frames.jsonl` under a size cap. None of it is queued, acked or replayed.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Manager as _};

use telemetry::metrics::RowPoints;

use crate::AppState;
use crate::call_debug::FrameChannel;
use crate::call_metrics;

const SCHEMA: u32 = 2;
const FRAMES_FILE: &str = "frames.jsonl";
// Raw frames hold call speech and arrive every second in debug mode; a long call must not fill
// the disk.
const MAX_FRAMES_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Calls whose `frames.jsonl` reached the cap in this run, so the cap is logged once per call.
static CAPPED_CALLS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// Sessions already warned about a message of another schema, so the warning is once per
/// session and not once a second.
static WARNED_SESSIONS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// The fields read before the rest, so a message of another schema is told apart from a broken
/// one.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TelemetryHeader {
    Telemetry {
        schema: u32,
        session_id: String,
    },
    TelemetryFrames,
    /// Any other message type: not telemetry, and not this module's to judge.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TelemetryMessage {
    Telemetry {
        session_id: String,
        at: Timestamp,
        channels: Vec<ChannelCounts>,
        events: Vec<Value>,
    },
    TelemetryFrames {
        session_id: String,
        frames: Vec<RawFrame>,
    },
}

/// One caption channel's cumulative counts since `started_at`.
#[derive(Deserialize)]
struct ChannelCounts {
    label: FrameChannel,
    id: u16,
    /// The page hook's count of caption channels, so a rejoin's channel with the same label and
    /// id has points of its own.
    serial: u32,
    is_own: bool,
    started_at: Timestamp,
    raw: u64,
    parsed: u64,
    control: u64,
    rejected: u64,
    acks: u64,
}

#[derive(Deserialize)]
struct MeetEvent {
    at: Timestamp,
    name: MeetEventName,
    #[serde(default)]
    attributes: serde_json::Map<String, Value>,
}

/// Every event mc-meet sends. An unknown name fails the event, so a new one is added here first.
#[derive(Debug, Clone, Copy, Deserialize)]
enum MeetEventName {
    #[serde(rename = "mc.caption.channel.opened")]
    ChannelOpened,
    #[serde(rename = "mc.caption.channel.closed")]
    ChannelClosed,
    #[serde(rename = "mc.caption.channel.error")]
    ChannelError,
    #[serde(rename = "mc.caption.channel.close_blocked")]
    ChannelCloseBlocked,
    #[serde(rename = "mc.caption.press")]
    Press,
    #[serde(rename = "mc.caption.control.changed")]
    ControlChanged,
    #[serde(rename = "mc.caption.first_frame")]
    FirstFrame,
    #[serde(rename = "mc.caption.first_caption")]
    FirstCaption,
}

/// What an event attribute's value may be.
#[derive(Clone, Copy)]
enum AttributeRule {
    Flag,
    /// A whole number of zero or more.
    Count,
    OneOf(&'static [&'static str]),
}

const CHANNEL_LABELS: &[&str] = &["captions", "captions_v2"];
/// `RTCErrorEvent.error.errorDetail` values in the WebRTC spec, and mc-meet's `unknown`.
const CHANNEL_ERROR_TYPES: &[&str] = &[
    "data-channel-failure",
    "dtls-failure",
    "fingerprint-failure",
    "sctp-failure",
    "sdp-syntax-error",
    "hardware-encoder-not-available",
    "hardware-encoder-error",
    "unknown",
];
const CONTROL_STATES: &[&str] = &["not_found", "unavailable", "on", "off"];
const CONTROL_STATES_BEFORE: &[&str] = &["none", "not_found", "unavailable", "on", "off"];
const CONTROL_KINDS: &[&str] = &["none", "button", "menu"];
/// The codes of `captionsControlTexts` in mc-meet's selectors, with `none` and `unknown`.
const CONTROL_TEXT_CODES: &[&str] = &[
    "none",
    "unknown",
    "turn_on_captions",
    "turn_off_captions",
    "captions_temporarily_unavailable",
    "captions_unavailable",
    "captions_and_translation",
];

type AttributeRules = &'static [(&'static str, AttributeRule)];

const CHANNEL_ATTRIBUTES: AttributeRules = &[
    (
        "mc.caption.channel.label",
        AttributeRule::OneOf(CHANNEL_LABELS),
    ),
    ("mc.caption.channel.id", AttributeRule::Count),
    ("mc.caption.channel.serial", AttributeRule::Count),
    ("mc.caption.channel.is_own", AttributeRule::Flag),
    ("mc.caption.channel.negotiated", AttributeRule::Flag),
];
const CHANNEL_OPTION_ATTRIBUTES: AttributeRules = &[
    ("mc.caption.channel.ordered", AttributeRule::Flag),
    ("mc.caption.channel.max_retransmits", AttributeRule::Count),
];
const CHANNEL_CLOSED_ATTRIBUTES: AttributeRules =
    &[("mc.caption.channel.lifetime_ms", AttributeRule::Count)];
const CHANNEL_ERROR_ATTRIBUTES: AttributeRules =
    &[("error.type", AttributeRule::OneOf(CHANNEL_ERROR_TYPES))];
const CHANNEL_AGE_ATTRIBUTES: AttributeRules =
    &[("mc.caption.channel.age_ms", AttributeRule::Count)];
const PRESS_ATTRIBUTES: AttributeRules = &[
    (
        "mc.caption.press.kind",
        AttributeRule::OneOf(&["click", "shortcut"]),
    ),
    (
        "mc.caption.press.reason",
        AttributeRule::OneOf(&["first", "retry"]),
    ),
    ("mc.caption.press.has_focus", AttributeRule::Flag),
    ("mc.caption.press.is_visible", AttributeRule::Flag),
    ("mc.caption.press.number", AttributeRule::Count),
    (
        "mc.caption.control.state_before",
        AttributeRule::OneOf(CONTROL_STATES),
    ),
    (
        "mc.caption.control.kind",
        AttributeRule::OneOf(CONTROL_KINDS),
    ),
];
const CONTROL_CHANGED_ATTRIBUTES: AttributeRules = &[
    (
        "mc.caption.control.state",
        AttributeRule::OneOf(CONTROL_STATES),
    ),
    (
        "mc.caption.control.state_before",
        AttributeRule::OneOf(CONTROL_STATES_BEFORE),
    ),
    (
        "mc.caption.control.kind",
        AttributeRule::OneOf(CONTROL_KINDS),
    ),
    (
        "mc.caption.control.label",
        AttributeRule::OneOf(CONTROL_TEXT_CODES),
    ),
    (
        "mc.caption.control.notice",
        AttributeRule::OneOf(CONTROL_TEXT_CODES),
    ),
];

impl MeetEventName {
    fn as_str(self) -> &'static str {
        match self {
            Self::ChannelOpened => "mc.caption.channel.opened",
            Self::ChannelClosed => "mc.caption.channel.closed",
            Self::ChannelError => "mc.caption.channel.error",
            Self::ChannelCloseBlocked => "mc.caption.channel.close_blocked",
            Self::Press => "mc.caption.press",
            Self::ControlChanged => "mc.caption.control.changed",
            Self::FirstFrame => "mc.caption.first_frame",
            Self::FirstCaption => "mc.caption.first_caption",
        }
    }

    /// The attributes this event may carry, and what each value may be.
    fn attribute_rules(self) -> [AttributeRules; 2] {
        match self {
            Self::ChannelOpened | Self::ChannelCloseBlocked => {
                [CHANNEL_ATTRIBUTES, CHANNEL_OPTION_ATTRIBUTES]
            }
            Self::ChannelClosed => [CHANNEL_ATTRIBUTES, CHANNEL_CLOSED_ATTRIBUTES],
            Self::ChannelError => [CHANNEL_ATTRIBUTES, CHANNEL_ERROR_ATTRIBUTES],
            Self::FirstFrame | Self::FirstCaption => [CHANNEL_ATTRIBUTES, CHANNEL_AGE_ATTRIBUTES],
            Self::Press => [PRESS_ATTRIBUTES, &[]],
            Self::ControlChanged => [CONTROL_CHANGED_ATTRIBUTES, &[]],
        }
    }
}

impl AttributeRule {
    fn allows(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Flag, Value::Bool(_)) => true,
            (Self::Count, Value::Number(number)) => number.is_u64(),
            (Self::OneOf(codes), Value::String(text)) => codes.contains(&text.as_str()),
            (Self::Flag | Self::Count | Self::OneOf(_), _) => false,
        }
    }
}

#[derive(Deserialize)]
struct RawFrame {
    channel: FrameChannel,
    #[serde(default)]
    channel_id: Option<u16>,
    #[serde(default)]
    is_own: bool,
    at: Timestamp,
    byte_length: u64,
    bytes_base64: String,
}

/// Takes a `telemetry` or `telemetry_frames` message from a port past its host hello. Any other
/// message type is left alone.
pub(crate) fn take(app: &AppHandle, frame: &[u8]) {
    let header: TelemetryHeader = match serde_json::from_slice(frame) {
        Ok(header) => header,
        Err(error) => {
            tracing::warn!(exception.message = %error, "ignored mc-meet telemetry that is not valid");
            return;
        }
    };
    match header {
        TelemetryHeader::Telemetry { schema, session_id } if schema != SCHEMA => {
            warn_schema_once(&session_id, schema);
            return;
        }
        TelemetryHeader::Telemetry { .. } | TelemetryHeader::TelemetryFrames => {}
        TelemetryHeader::Other => return,
    }
    let message: TelemetryMessage = match serde_json::from_slice(frame) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(exception.message = %error, "ignored mc-meet telemetry that is not valid");
            return;
        }
    };
    match message {
        TelemetryMessage::Telemetry {
            session_id,
            at,
            channels,
            events,
        } => {
            let Some((call_id, folder)) = call_folder(app, &session_id) else {
                return;
            };
            if !channels.is_empty() {
                call_metrics::write_mc_meet_rows(
                    &call_id,
                    &folder,
                    &[(at, channel_points(&channels))],
                );
            }
            for event in events {
                log_event(&call_id, &session_id, event);
            }
        }
        TelemetryMessage::TelemetryFrames { session_id, frames } => {
            let Some((call_id, folder)) = call_folder(app, &session_id) else {
                return;
            };
            let state = app.state::<AppState>();
            // Frames hold the call's speech: none are kept past a stop, or outside debug mode,
            // whatever the page sends.
            if !state.debug_mode.mode().is_on(Timestamp::now())
                || state.calls.status().is_stopped_by_user(&call_id)
            {
                return;
            }
            // The folder cap holds all of a call's debug files; `frames.jsonl` has its own too.
            if !state.call_debug.has_room(&call_id) {
                return;
            }
            append_frames(&call_id, &folder, &frames);
        }
    }
}

fn warn_schema_once(session_id: &str, schema: u32) {
    let is_first = WARNED_SESSIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(session_id.to_owned());
    if is_first {
        tracing::warn!(
            browser_call.session_id = session_id,
            mc.telemetry.schema = schema,
            mc.telemetry.expected_schema = SCHEMA,
            "ignored mc-meet telemetry of another schema; mc-meet and the app differ in version"
        );
    }
}

/// The row of every channel's counters: frames by kind, and acks.
fn channel_points(channels: &[ChannelCounts]) -> RowPoints {
    let mut points = RowPoints::default();
    for channel in channels {
        let attributes = [
            (
                "mc.caption.channel.label",
                Value::from(channel.label.label()),
            ),
            ("mc.caption.channel.id", Value::from(channel.id)),
            ("mc.caption.channel.serial", Value::from(channel.serial)),
            ("mc.caption.channel.is_own", Value::from(channel.is_own)),
        ];
        let frame_counts = [
            ("raw", channel.raw),
            ("parsed", channel.parsed),
            ("control", channel.control),
            ("rejected", channel.rejected),
        ];
        for (kind, total) in frame_counts {
            let mut frame_attributes = attributes.to_vec();
            frame_attributes.push(("mc.caption.frame.kind", Value::from(kind)));
            points.counter(
                "mc.captions.frames",
                "{frame}",
                &frame_attributes,
                total,
                channel.started_at,
            );
        }
        points.counter(
            "mc.captions.acks",
            "{ack}",
            &attributes,
            channel.acks,
            channel.started_at,
        );
    }
    points
}

/// The call the session writes to, and that call's debug folder.
fn call_folder(app: &AppHandle, session_id: &str) -> Option<(PathBuf, PathBuf)> {
    let state = app.state::<AppState>();
    let call_id = match state.browser_calls.call_of_session(session_id) {
        Ok(Some(call_id)) => call_id,
        Ok(None) => return None,
        Err(invalid) => {
            tracing::warn!(exception.message = %invalid, "refused mc-meet telemetry");
            return None;
        }
    };
    let folder = state.call_debug.call_folder(&call_id);
    Some((call_id, folder))
}

fn log_event(call_id: &Path, session_id: &str, event: Value) {
    let event: MeetEvent = match serde_json::from_value(event) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(
                call.id = %call_id.display(),
                exception.message = %error,
                "ignored an mc-meet event that is not valid"
            );
            return;
        }
    };
    let sent_count = event.attributes.len();
    let attributes = allowed_attributes(event.name, event.attributes);
    let dropped_count = sent_count - attributes.len();
    let attributes = Value::Object(attributes);
    let name = event.name.as_str();
    // Braced fields: after `target:`, the macro cannot tell dotted names from its other arms.
    tracing::event!(
        target: "mc_meet",
        tracing::Level::INFO,
        {
            call.id = %call_id.display(),
            browser_call.session_id = session_id,
            event.name = name,
            mc.meet.at = %event.at,
            mc.meet.attributes = %attributes,
            mc.meet.dropped_attribute_count = dropped_count,
        },
        "{name}"
    );
}

/// Keeps only the attributes the event's rules name, with a value its rule allows. mc-meet
/// sends no page text, and a page that forges an event cannot make the log say any.
fn allowed_attributes(
    name: MeetEventName,
    attributes: serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let rules = name.attribute_rules();
    attributes
        .into_iter()
        .filter(|(key, value)| {
            rules
                .iter()
                .flat_map(|rules| rules.iter())
                .find(|(rule_key, _)| rule_key == key)
                .is_some_and(|(_, rule)| rule.allows(value))
        })
        .collect()
}

fn append_frames(call_id: &Path, folder: &Path, frames: &[RawFrame]) {
    if frames.is_empty() {
        return;
    }
    let path = folder.join(FRAMES_FILE);
    let received_at = Timestamp::now();
    let appended = std::fs::create_dir_all(folder).and_then(|()| {
        let mut size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        for frame in frames {
            let mut line = serde_json::to_vec(&serde_json::json!({
                "type": "raw_frame",
                "channel": frame.channel,
                "channel_id": frame.channel_id,
                "is_own": frame.is_own,
                "at": frame.at,
                "byte_length": frame.byte_length,
                "bytes_base64": frame.bytes_base64,
                "received_at": received_at,
            }))
            .map_err(std::io::Error::other)?;
            line.push(b'\n');
            let line_bytes = u64::try_from(line.len()).unwrap_or(u64::MAX);
            if size.saturating_add(line_bytes) > MAX_FRAMES_FILE_BYTES {
                note_capped(call_id, &path);
                break;
            }
            file.write_all(&line)?;
            size = size.saturating_add(line_bytes);
        }
        Ok(())
    });
    if let Err(error) = appended {
        tracing::warn!(
            file.path = %path.display(),
            exception.message = %error,
            "could not write mc-meet raw frames"
        );
    }
}

fn note_capped(call_id: &Path, path: &Path) {
    let is_first = CAPPED_CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(call_id.to_path_buf());
    if is_first {
        tracing::warn!(
            call.id = %call_id.display(),
            file.path = %path.display(),
            "the call's raw frames reached 50 MB; later frames are dropped"
        );
    }
}
