//! TranscripTonic's "advanced" webhook body, as built in `extension/background-script/exporters.js`
//! and typed in `types/index.js` (v3.4.3).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::{ChatMessage, Platform, Segment, Source, SpeakerNaming, Transcript};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(
        "TranscripTonic sent the simple webhook body; set webhook body type to advanced in its settings"
    )]
    SimpleBody,
    #[error("TranscripTonic payload is not valid at {field}: {source}")]
    InvalidPayload {
        field: String,
        source: serde_json::Error,
    },
}

impl From<serde_path_to_error::Error<serde_json::Error>> for ParseError {
    fn from(error: serde_path_to_error::Error<serde_json::Error>) -> Self {
        Self::InvalidPayload {
            field: error.path().to_string(),
            source: error.into_inner(),
        }
    }
}

/// Parses a raw advanced webhook body.
///
/// # Errors
///
/// Returns `ParseError::SimpleBody` for the simple body, which carries locale-formatted times
/// and a flattened transcript, and `ParseError::InvalidPayload` for anything else that does not
/// match the advanced shape.
pub fn parse(payload: &[u8]) -> Result<Transcript, ParseError> {
    let header: BodyHeader = from_json(payload)?;
    match header.webhook_body_type {
        BodyType::Simple => return Err(ParseError::SimpleBody),
        BodyType::Advanced => {}
    }
    let body: AdvancedBody = from_json(payload)?;
    Ok(body.into_transcript())
}

// Names the failing field, such as `transcript[3].timestamp`, in the error.
fn from_json<'de, T: Deserialize<'de>>(payload: &'de [u8]) -> Result<T, ParseError> {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    Ok(serde_path_to_error::deserialize(&mut deserializer)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BodyHeader {
    webhook_body_type: BodyType,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum BodyType {
    Simple,
    Advanced,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdvancedBody {
    meeting_software: MeetingSoftware,
    meeting_title: String,
    meeting_start_timestamp: Timestamp,
    meeting_end_timestamp: Timestamp,
    // JSON.stringify drops keys whose value is undefined, as on meetings stored by old versions.
    #[serde(default)]
    transcript: Vec<TranscriptBlock>,
    #[serde(default)]
    chat_messages: Vec<RawChatMessage>,
}

#[derive(Deserialize)]
enum MeetingSoftware {
    #[serde(rename = "Google Meet")]
    GoogleMeet,
    Teams,
    Zoom,
    /// Meetings saved before v3.2 carry no software name.
    #[serde(rename = "")]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptBlock {
    person_name: String,
    timestamp: Timestamp,
    transcript_text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChatMessage {
    person_name: String,
    timestamp: Timestamp,
    chat_message_text: String,
}

impl AdvancedBody {
    fn into_transcript(mut self) -> Transcript {
        let started_at = self.meeting_start_timestamp;
        let ended_at = self.meeting_end_timestamp;
        let offset_s = |at: Timestamp| {
            at.duration_since(started_at)
                .max(SignedDuration::ZERO)
                .as_secs_f64()
        };

        // A block is stamped when its speaker started but pushed when it closes, so overlapping
        // captions can arrive out of order. A block runs until the next block starts.
        self.transcript.sort_by_key(|block| block.timestamp);
        let block_starts: Vec<f64> = self
            .transcript
            .iter()
            .map(|block| offset_s(block.timestamp))
            .collect();
        let call_length_s = offset_s(ended_at);
        let segments = self
            .transcript
            .into_iter()
            .enumerate()
            .map(|(index, block)| Segment {
                start_s: block_starts[index],
                end_s: block_starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(call_length_s)
                    .max(block_starts[index]),
                speaker: block.person_name,
                text: block.transcript_text,
            })
            .collect();

        let platform = match self.meeting_software {
            MeetingSoftware::GoogleMeet => Platform::Meet,
            MeetingSoftware::Teams => Platform::Teams,
            MeetingSoftware::Zoom => Platform::Zoom,
            MeetingSoftware::Unknown => Platform::Other,
        };
        // Meet titles a call with no calendar event "Meet – abc-defg-hij". The code is the only
        // link to the call in the payload, and the title says nothing about the meeting.
        let meet_code = match platform {
            Platform::Meet => meet_code_from_title(&self.meeting_title),
            Platform::Teams | Platform::Zoom | Platform::Slack | Platform::Other => None,
        };
        let meeting_url = meet_code.map(|code| format!("https://meet.google.com/{code}"));
        // One line, so the title reads the same in a heading, a file name and the tray menu.
        let title = Some(
            self.meeting_title
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        )
        .filter(|title| meeting_url.is_none() && !title.is_empty());

        Transcript {
            source: Source::Transcriptonic,
            started_at,
            ended_at,
            platform,
            meeting_url,
            title,
            speaker_naming: SpeakerNaming::Real,
            my_speaker: Some(TRANSCRIPTONIC_ME.to_owned()),
            attendees: Vec::new(),
            segments,
            chat_messages: self
                .chat_messages
                .into_iter()
                .map(|message| ChatMessage {
                    sent_at: message.timestamp,
                    sender: message.person_name,
                    text: message.chat_message_text,
                })
                .collect(),
            slack_huddle: None,
        }
    }
}

// TranscripTonic's name for the local user.
const TRANSCRIPTONIC_ME: &str = "You";

fn meet_code_from_title(title: &str) -> Option<&str> {
    let code = title.strip_prefix("Meet \u{2013} ")?;
    let groups: Vec<&str> = code.split('-').collect();
    let is_meet_code = groups.iter().map(|group| group.len()).eq([3, 4, 3])
        && groups
            .iter()
            .all(|group| group.chars().all(|letter| letter.is_ascii_lowercase()));
    is_meet_code.then_some(code)
}

/// Joins two advanced bodies of one call that TranscripTonic filed as two meetings. The result
/// starts at the earlier start, ends at the later end and keeps the earlier title, since
/// TranscripTonic retitles the second part "Google Meet". Caption blocks and chat messages are
/// the union of both, one per speaker and start time. Where both have a block, the longer text
/// wins, since a block grows until its speaker stops.
///
/// # Errors
///
/// Returns `ParseError` when either body is not a valid advanced body.
///
/// # Panics
///
/// Never: a JSON map always serializes.
pub fn merge_payloads(payload: &[u8], other_payload: &[u8]) -> Result<Vec<u8>, ParseError> {
    let (first, second) = {
        let started_at = |payload: &[u8]| parse(payload).map(|transcript| transcript.started_at);
        if started_at(payload)? <= started_at(other_payload)? {
            (payload, other_payload)
        } else {
            (other_payload, payload)
        }
    };
    let mut merged: serde_json::Map<String, serde_json::Value> = from_json(first)?;
    let second: serde_json::Map<String, serde_json::Value> = from_json(second)?;
    let ended_at = |body: &serde_json::Map<String, serde_json::Value>| {
        body.get("meetingEndTimestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(|at| at.parse::<Timestamp>().ok())
    };
    if ended_at(&second) > ended_at(&merged)
        && let Some(end) = second.get("meetingEndTimestamp")
    {
        merged.insert("meetingEndTimestamp".to_owned(), end.clone());
    }
    for (list, text_key) in [
        ("transcript", "transcriptText"),
        ("chatMessages", "chatMessageText"),
    ] {
        let mut by_start: std::collections::BTreeMap<(String, String), serde_json::Value> =
            std::collections::BTreeMap::new();
        let entries = [&merged, &second]
            .into_iter()
            .filter_map(|body| body.get(list).and_then(serde_json::Value::as_array))
            .flatten();
        for entry in entries {
            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let text_len = |entry: &serde_json::Value| {
                entry
                    .get(text_key)
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, str::len)
            };
            // Timestamp first, so the merged list is in time order.
            let key = (field("timestamp"), field("personName"));
            match by_start.get(&key) {
                Some(kept) if text_len(kept) >= text_len(entry) => {}
                Some(_) | None => {
                    by_start.insert(key, entry.clone());
                }
            }
        }
        merged.insert(
            list.to_owned(),
            serde_json::Value::Array(by_start.into_values().collect()),
        );
    }
    Ok(serde_json::to_vec(&merged).expect("a JSON map always serializes"))
}

/// Saves a raw payload in `payload_dir` under a name no other payload has, synced to disk.
///
/// TranscripTonic marks a meeting posted on a 2xx reply and never sends it again, so the payload
/// must be durable before the receiver replies.
///
/// # Errors
///
/// Returns the I/O error that stopped the save.
pub fn save_payload(payload_dir: &Path, payload: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(payload_dir)?;
    let mut staged = tempfile::NamedTempFile::new_in(payload_dir)?;
    staged.write_all(payload)?;
    staged.as_file().sync_all()?;

    // No colons: Windows rejects them in file names.
    let received_at = Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
    let mut copy_number = 0_u32;
    let payload_path = loop {
        let file_name = match copy_number {
            0 => format!("{received_at}-transcriptonic.json"),
            _ => format!("{received_at}-{copy_number}-transcriptonic.json"),
        };
        let payload_path = payload_dir.join(file_name);
        match staged.persist_noclobber(&payload_path) {
            Ok(_) => break payload_path,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                staged = error.file;
                copy_number += 1;
            }
            Err(error) => return Err(error.error),
        }
    };
    // The rename is durable only once the folder entry is on disk. Windows has no folder fsync.
    #[cfg(unix)]
    std::fs::File::open(payload_dir)?.sync_all()?;
    Ok(payload_path)
}
