//! What a debug report without text keeps of each JSON line. It keeps only strings under keys
//! known to hold times, ids, versions and codes. Every other string becomes its length: caption
//! and chat text, names, meeting titles and codes, Slack channels and workspaces, window titles,
//! error messages and any field added later. Numbers and booleans stay. Frame bytes are dropped.
//! Device and message ids become numbers that stay the same across the report: the DOM reader
//! uses a speaker's name as the device id.
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
    // Names the app and the decoders chose.
    "event.name",
    "skeleton",
    "layout_profile",
    "slack.layout_profile",
    "target",
    // The log message itself. Values go in fields, not in the message.
    "message",
    // Versions and builds.
    "app_version",
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
];
const MAX_CODE_CHARS: usize = 64;

/// Meet's data channel labels. `channel` also names a Slack channel in a huddle log.
const MEET_CHANNEL_LABELS: &[&str] = &["captions", "captions_v2", "collections", "meet_messages"];
const CHANNEL_KEYS: &[&str] = &["channel", "browser_call.channel", "label"];

/// Holds a tracing span's name, under `span` or `spans`. A `name` anywhere else is a person's.
const SPAN_KEYS: &[&str] = &["span", "spans"];

/// Keys whose strings become a numbered stand-in, the same one for the same id in one report.
const NUMBERED_KEYS: &[(&str, &str)] = &[("device_id", "device"), ("message_id", "message")];

/// Frame bytes hold call speech.
const BYTES_KEY: &str = "bytes_base64";

/// Redacts the lines of one report. It numbers ids in the order it meets them.
#[derive(Default)]
pub(crate) struct Redactor {
    stand_ins: HashMap<(&'static str, String), String>,
}

impl Redactor {
    /// Replaces each string `value` must not keep with its length or a stand-in, and drops frame
    /// bytes.
    pub(crate) fn redact(&mut self, value: &mut Value) {
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
                    *value = Value::String(format!("[{} chars]", text.chars().count()));
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
    key == "name" && container.is_some_and(|container| SPAN_KEYS.contains(&container))
}

fn is_code(text: &str) -> bool {
    (1..=MAX_CODE_CHARS).contains(&text.len())
        && text.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
}
