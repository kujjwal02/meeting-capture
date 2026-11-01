//! What a debug report without text keeps of each JSON line. It keeps only strings under keys
//! known to hold times, ids, versions and codes. Every other string becomes its length: caption
//! and chat text, names, meeting titles and codes, Slack channels and workspaces, window titles,
//! error messages, device names and any field added later. Numbers and booleans stay. Frame bytes
//! are dropped. A log line from a dependency also loses its message and `file.path`. Log, metric
//! and environment lines, capture manifests and the job row all go through it.
//! Device and message ids become numbers that stay the same across the report: the DOM reader
//! uses a speaker's name as the device id. A kept path loses the user's name: its home folder
//! becomes `~`.
//!
//! Object keys are kept as they are. The keys the app writes are field names, metric point keys
//! and the labels of `frame_counters`; the app builds the last two from closed sets of values,
//! never from page or user text.
//!
//! The module depends on `serde_json` alone, so a check outside the app can include it.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// Keys whose strings are kept as they are.
const KEPT_KEYS: &[&str] = &[
    // Times.
    "at",
    "received_at",
    "saved_at",
    "seen_at",
    "timestamp",
    "started_at",
    "ended_at",
    "last_caption_at",
    "recording_since",
    "described_at",
    "start_timestamp",
    "created_at",
    "updated_at",
    "closed_at",
    "first_frame_at",
    "modified_at",
    "recorded_at",
    "debug_mode_until",
    "utc",
    "mc.meet.at",
    // Units of metric points, such as `dBFS` and `{frame}`.
    "unit",
    // Files the capture helpers named in their manifests.
    "stem",
    "file",
    "folder",
    "playlist",
    "init",
    "codec",
    // The report's own files that reached their size cap.
    "mc.report.truncated_files",
    // Ids the extension, Meet or the app made up.
    "seq",
    "session_id",
    "utterance_id",
    "call_id",
    "call.id",
    "browser_call.session_id",
    "meeting.id",
    "file.path",
    "file.directory",
    "server.address",
    "trace_id",
    "span_id",
    "parent_span_id",
    "service.instance.id",
    // Names the app and the decoders chose.
    "event.name",
    "skeleton",
    "layout_profile",
    "slack.layout_profile",
    "target",
    "scope",
    "record",
    "service.name",
    // mc-meet's event attributes as JSON text. The app kept only booleans, numbers and codes.
    "mc.meet.attributes",
    // The log message itself. Values go in fields, not in the message.
    "body",
    // Versions and builds.
    "app_version",
    "service.version",
    "os.type",
    "os.version",
    "host.arch",
    "chrome.version",
    "mc_meet.version",
    "meet.build",
    "extension_version",
    "browser.extension.version",
    "chrome_version",
    "meet_build",
    "slack_version",
    "slack.version",
    "update.version",
    "browser.name",
    "gen_ai.request.model",
];

/// Keys whose strings are kept when they are a code, such as `no-device` or `captions_off`: a huddle
/// health `reason` is a sentence, and a Meet one a code.
const CODE_KEYS: &[&str] = &[
    "type",
    "kind",
    "event",
    "platform",
    "call.platform",
    "call.status",
    "call.end_reason",
    "reason",
    "error.type",
    "source",
    "state",
    "meeting.state",
    "huddle.health",
    "ready_state",
    "meeting_connection_state",
    "tag",
    "role",
    "level",
    "severity_text",
    "mc.recorder.reason",
    "job.step",
    "stopped_by",
    // Metric point attributes.
    "mc.audio.stem",
    "mc.caption.channel.id",
    "mc.caption.frame.kind",
    "mc.video.frame.kind",
    "process.command",
    // Pipeline decisions.
    "mc.decision.kind",
    "mc.decision.chosen",
    "mc.transcript.speaker_naming",
    // Environment lines: device transports, permission states, power.
    "transport",
    "microphone",
    "screen",
    "accessibility",
    "audio_capture",
    "screen_recording",
    "lid",
];

/// The object that holds a `tracing` JSON line's fields, in reports saved before the telemetry
/// format. Its `message` is the log message; a `message` anywhere else can be a chat message or
/// an error's text.
const LEGACY_FIELDS_KEY: &str = "fields";
const LEGACY_MESSAGE_KEY: &str = "message";
const MAX_CODE_CHARS: usize = 64;

/// Meet's data channel labels. `channel` also names a Slack channel in a huddle log.
const MEET_CHANNEL_LABELS: &[&str] = &["captions", "captions_v2", "collections", "meet_messages"];
const CHANNEL_KEYS: &[&str] = &[
    "channel",
    "browser_call.channel",
    "label",
    "mc.caption.channel.label",
];

/// Holds a tracing span's name, under `span` or `spans`. A `name` anywhere else is a person's.
const SPAN_KEYS: &[&str] = &["span", "spans"];

/// Keys whose strings become a numbered stand-in, the same one for the same id in one report.
const NUMBERED_KEYS: &[(&str, &str)] = &[("device_id", "device"), ("message_id", "message")];

/// Frame bytes hold call speech.
const BYTES_KEY: &str = "bytes_base64";

/// The app's crates, as a log line's `scope` starts. A dependency's message and file path can
/// hold anything, such as a URL or a meeting title. A crate missing here only loses its messages
/// and paths in a report without text.
const WORKSPACE_CRATES: &[&str] = &[
    "browser_shortcut",
    "call_finalize",
    "call_folder",
    "capture",
    "capture_protocol",
    "huddle_captions",
    "jobs",
    "mc",
    "mc_capture",
    "mc_meet",
    "meeting_capture",
    "native_host",
    "pipeline",
    "sources",
    "stem_timing",
    "stem_writer",
    "telemetry",
    "video_capture",
];

/// Redacts the lines of one report. It numbers ids in the order it meets them.
#[derive(Default)]
pub(crate) struct Redactor {
    stand_ins: HashMap<(&'static str, String), String>,
}

impl Redactor {
    /// Replaces each string `value` must not keep with its length or a stand-in, and drops frame
    /// bytes.
    pub(crate) fn redact(&mut self, value: &mut Value) {
        let is_dependency_line = value
            .get("scope")
            .and_then(Value::as_str)
            .is_some_and(|scope| !is_workspace_scope(scope));
        if is_dependency_line {
            redact_text(value.get_mut("body"));
            redact_text(
                value
                    .get_mut("attributes")
                    .and_then(|attributes| attributes.get_mut("file.path")),
            );
        }
        self.redact_under(value, None, None);
    }

    // `key` is the field that holds `value`; `container` the field that holds its object.
    fn redact_under(&mut self, value: &mut Value, key: Option<&str>, container: Option<&str>) {
        match value {
            Value::Object(fields) => self.redact_object(fields, key),
            Value::Array(values) => {
                for element in values {
                    self.redact_under(element, key, container);
                }
            }
            Value::String(text) => {
                if let Some(stand_in) = self.stand_in(key, text) {
                    *value = Value::String(stand_in);
                } else if !is_kept(key, container, text) {
                    *value = Value::String(text_length(text));
                } else if let Some(short) = without_home(text) {
                    *value = Value::String(short);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn redact_object(&mut self, fields: &mut Map<String, Value>, container: Option<&str>) {
        for (key, field) in fields.iter_mut() {
            if key == BYTES_KEY {
                *field = Value::Null;
                continue;
            }
            self.redact_under(field, Some(key), container);
        }
    }

    fn stand_in(&mut self, key: Option<&str>, text: &str) -> Option<String> {
        let (_, prefix) = NUMBERED_KEYS
            .iter()
            .find(|(numbered, _)| Some(*numbered) == key)?;
        let next_number = self
            .stand_ins
            .keys()
            .filter(|(known_prefix, _)| known_prefix == prefix)
            .count()
            + 1;
        Some(
            self.stand_ins
                .entry((prefix, text.to_owned()))
                .or_insert_with(|| format!("{prefix}-{next_number}"))
                .clone(),
        )
    }
}

fn is_workspace_scope(scope: &str) -> bool {
    let crate_name = scope.split("::").next().unwrap_or(scope);
    WORKSPACE_CRATES.contains(&crate_name)
}

fn redact_text(value: Option<&mut Value>) {
    if let Some(value) = value
        && let Value::String(text) = value
    {
        *value = Value::String(text_length(text));
    }
}

fn text_length(text: &str) -> String {
    format!("[{} chars]", text.chars().count())
}

fn is_kept(key: Option<&str>, container: Option<&str>, text: &str) -> bool {
    let Some(key) = key else {
        return false;
    };
    if KEPT_KEYS.contains(&key) {
        return true;
    }
    if CODE_KEYS.contains(&key) {
        return is_code(text);
    }
    if CHANNEL_KEYS.contains(&key) {
        return MEET_CHANNEL_LABELS.contains(&text);
    }
    if key == LEGACY_MESSAGE_KEY {
        return container == Some(LEGACY_FIELDS_KEY);
    }
    key == "name" && container.is_some_and(|container| SPAN_KEYS.contains(&container))
}

/// The path with the home folder written as `~`, or `None` when it starts in no home folder:
/// `/Users/<name>`, `/home/<name>` or `<drive>:\Users\<name>`, with either slash on Windows.
fn without_home(path: &str) -> Option<String> {
    let after_users = path
        .strip_prefix("/Users/")
        .or_else(|| path.strip_prefix("/home/"))
        .or_else(|| after_windows_users_folder(path))?;
    let name_end = after_users.find(['/', '\\']).unwrap_or(after_users.len());
    (name_end > 0).then(|| format!("~{}", &after_users[name_end..]))
}

// What follows `C:\Users\`, `\\?\C:\Users\` or `C:/Users/`.
fn after_windows_users_folder(path: &str) -> Option<&str> {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    let is_drive = bytes.len() > 9
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        && bytes[3..8].eq_ignore_ascii_case(b"users")
        && matches!(bytes[8], b'\\' | b'/');
    is_drive.then(|| &path[9..])
}

fn is_code(text: &str) -> bool {
    (1..=MAX_CODE_CHARS).contains(&text.len())
        && text.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
}
