//! The browser call log: one JSON line per event the mc-meet extension sent for one call in one
//! tab, appended by the tray app as it arrives. The log is the raw input; `parse` turns it into a
//! transcript.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize};

use crate::append_log::InvalidLine;
use crate::{Attendee, ChatMessage, Platform, Segment, Source, SpeakerNaming, Transcript};

/// One line of the log. The app's own lines, such as the end it writes for an idle call, have no
/// `seq`.
#[derive(Debug, Clone, Deserialize)]
pub struct LogLine {
    pub seq: Option<u64>,
    pub received_at: Timestamp,
    pub event: CallEvent,
}

/// One call event, as the extension sent it. Unknown fields are ignored, so a newer extension
/// still parses.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallEvent {
    CallStarted {
        at: Timestamp,
        platform: CallPlatform,
        meeting_code: String,
        #[serde(default)]
        title: Option<String>,
    },
    /// A caption with a higher `version` for the same `device_id` and `utterance_id` replaces
    /// the earlier text.
    Caption {
        at: Timestamp,
        #[serde(deserialize_with = "id_text")]
        device_id: String,
        #[serde(deserialize_with = "id_text")]
        utterance_id: String,
        version: u64,
        /// Empty when the device had no name yet. The parser then takes the roster's name.
        speaker: String,
        text: String,
        source: ReadSource,
    },
    Chat {
        at: Timestamp,
        #[serde(deserialize_with = "id_text")]
        message_id: String,
        /// Only channel chat carries the sender's device.
        #[serde(default)]
        device_id: Option<String>,
        /// Empty when the device had no name yet, as for a caption's `speaker`.
        sender: String,
        text: String,
        source: ReadSource,
    },
    /// The whole participant list, each time it changes.
    Roster {
        at: Timestamp,
        participants: Vec<RosterParticipant>,
    },
    Health {
        at: Timestamp,
        source: HealthSource,
        /// Meet's captions button is on, and Meet does not refuse captions.
        captions_on: bool,
        /// Meet refuses captions: its captions button is disabled, or says live captions are
        /// unavailable. Older extensions leave it out.
        #[serde(default)]
        captions_unavailable: bool,
        /// Frames on a caption channel fail to decode in a row, with no channel caption since.
        #[serde(default)]
        caption_frames_failing: bool,
        parse_failures: u64,
        #[serde(default)]
        detail: Option<String>,
    },
    CallEnded {
        at: Timestamp,
        reason: EndReason,
    },
    /// A kind this app does not know, from a newer extension.
    #[serde(other)]
    Unknown,
}

impl CallEvent {
    /// When the event happened in the call. `None` for an unknown kind.
    #[must_use]
    pub fn at(&self) -> Option<Timestamp> {
        match self {
            CallEvent::CallStarted { at, .. }
            | CallEvent::Caption { at, .. }
            | CallEvent::Chat { at, .. }
            | CallEvent::Roster { at, .. }
            | CallEvent::Health { at, .. }
            | CallEvent::CallEnded { at, .. } => Some(*at),
            CallEvent::Unknown => None,
        }
    }

    /// Why the call ended, for an event that ends its call.
    #[must_use]
    pub fn end_reason(&self) -> Option<EndReason> {
        match self {
            CallEvent::CallEnded { reason, .. } => Some(*reason),
            CallEvent::CallStarted { .. }
            | CallEvent::Caption { .. }
            | CallEvent::Chat { .. }
            | CallEvent::Roster { .. }
            | CallEvent::Health { .. }
            | CallEvent::Unknown => None,
        }
    }

    /// Whether the event ends its call.
    #[must_use]
    pub fn ends_call(&self) -> bool {
        self.end_reason().is_some()
    }
}

/// What a `call_started` event says about its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallStart {
    /// Trimmed. `None` when blank.
    pub meeting_code: Option<String>,
    /// Meet's title on one line, so it reads the same in a heading, a file name and the tray
    /// menu. `None` when blank or when it holds the meeting code: Meet titles a call with no
    /// calendar event with its code, which says nothing about the meeting.
    pub title: Option<String>,
}

impl CallStart {
    /// The start `event` reports, if it is a `call_started`.
    #[must_use]
    pub fn from_event(event: &CallEvent) -> Option<Self> {
        match event {
            CallEvent::CallStarted {
                meeting_code,
                title,
                ..
            } => {
                let meeting_code = Some(meeting_code.trim())
                    .filter(|code| !code.is_empty())
                    .map(str::to_owned);
                let title = title
                    .as_deref()
                    .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|title| {
                        !title.is_empty()
                            && meeting_code
                                .as_deref()
                                .is_none_or(|code| !title.contains(code))
                    });
                Some(Self {
                    meeting_code,
                    title,
                })
            }
            CallEvent::Caption { .. }
            | CallEvent::Chat { .. }
            | CallEvent::Roster { .. }
            | CallEvent::Health { .. }
            | CallEvent::CallEnded { .. }
            | CallEvent::Unknown => None,
        }
    }
}

/// What a whole log says about its call.
#[derive(Debug, Clone)]
pub struct LogSummary {
    /// From the first `call_started`. A rejoined call has one per session, and the first names
    /// the call.
    pub start: Option<CallStart>,
    /// The first `call_started`'s time, else the earliest event time: a log whose
    /// `call_started` was lost still has times on its other events.
    pub started_at: Option<Timestamp>,
    pub last_at: Option<Timestamp>,
    /// Whether the last line ends the call.
    pub has_ended: bool,
}

/// Summarises the lines of one log.
#[must_use]
pub fn summarize(lines: &[LogLine]) -> LogSummary {
    let event_times = || lines.iter().filter_map(|line| line.event.at());
    let first_start = lines
        .iter()
        .find_map(|line| Some((line.event.at()?, CallStart::from_event(&line.event)?)));
    let started_at = first_start
        .as_ref()
        .map(|(at, _)| *at)
        .or_else(|| event_times().min());
    LogSummary {
        start: first_start.map(|(_, start)| start),
        started_at,
        last_at: event_times().max(),
        has_ended: lines.last().is_some_and(|line| line.event.ends_call()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPlatform {
    Meet,
}

/// Where the extension read a caption or a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    /// Meet's data channels.
    Channel,
    /// The page's captions and chat panels.
    Dom,
}

/// Where captions come from right now, or `None` when the extension reads none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSource {
    Channel,
    Dom,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Left,
    TabClosed,
    Kicked,
    PageUnload,
    /// Written by the app: no extension port had the call open for 5 minutes.
    Idle,
    /// Written by the app: it found the log open at start, after a quit or a crash.
    AppRestarted,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RosterParticipant {
    #[serde(deserialize_with = "id_text")]
    pub device_id: String,
    pub name: String,
    #[serde(default)]
    pub is_self: bool,
}

// The DOM reader numbers utterances, and the data channels use strings, so an id may be either.
fn id_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdText {
        Text(String),
        Number(u64),
    }
    Ok(match IdText::deserialize(deserializer)? {
        IdText::Text(text) => text,
        IdText::Number(number) => number.to_string(),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("browser call log line {line} is not valid: {source}")]
    InvalidLine {
        line: usize,
        source: serde_json::Error,
    },
    #[error("browser call log has no events")]
    NoEvents,
}

/// Reads every line of a browser call log. A torn last line, from a crash mid-write, is skipped.
///
/// # Errors
///
/// Returns `ParseError::InvalidLine` naming the first bad line other than a torn last one.
pub fn read_lines(log: &[u8]) -> Result<Vec<LogLine>, ParseError> {
    crate::append_log::read_lines(log)
        .map_err(|InvalidLine { line, source }| ParseError::InvalidLine { line, source })
}

/// Parses a browser call log into a transcript.
///
/// # Errors
///
/// Returns `ParseError` for a bad line or a log with no events.
pub fn parse(log: &[u8]) -> Result<Transcript, ParseError> {
    let lines = read_lines(log)?;
    let summary = summarize(&lines);
    let Some(started_at) = summary.started_at else {
        return Err(ParseError::NoEvents);
    };
    let ended_at = summary.last_at.unwrap_or(started_at).max(started_at);
    let (meeting_code, title) = summary
        .start
        .map_or((None, None), |start| (start.meeting_code, start.title));

    let Collected {
        captions,
        chat_messages,
        attendees,
    } = collect(&lines);
    let self_participant = attendees.iter().find(|attendee| attendee.is_self);
    // The label the local user's captions carry, else the roster name.
    let my_speaker = self_participant.map(|participant| {
        captions
            .iter()
            .find(|caption| participant.platform_id.as_deref() == Some(&caption.device_id))
            .map_or_else(
                || participant.name.clone(),
                |caption| caption.speaker.clone(),
            )
    });
    Ok(Transcript {
        source: Source::MeetExtension,
        started_at,
        ended_at,
        platform: Platform::Meet,
        meeting_url: meeting_code.map(|code| format!("https://meet.google.com/{code}")),
        title,
        speaker_naming: SpeakerNaming::Real,
        my_speaker,
        attendees,
        segments: segments(captions, started_at),
        chat_messages,
        slack_huddle: None,
    })
}

struct Caption {
    device_id: String,
    version: u64,
    first_at: Timestamp,
    last_at: Timestamp,
    speaker: String,
    text: String,
}

struct Chat<'a> {
    device_id: Option<&'a str>,
    message: ChatMessage,
}

struct Collected {
    captions: Vec<Caption>,
    chat_messages: Vec<ChatMessage>,
    attendees: Vec<Attendee>,
}

fn collect(lines: &[LogLine]) -> Collected {
    let mut captions: Vec<Caption> = Vec::new();
    let mut caption_index: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    let mut chats: Vec<Chat> = Vec::new();
    let mut chat_ids: Vec<&str> = Vec::new();
    let mut attendees: Vec<Attendee> = Vec::new();
    for line in lines {
        match &line.event {
            CallEvent::Caption {
                at,
                device_id,
                utterance_id,
                version,
                speaker,
                text,
                ..
            } => {
                let key = (device_id.as_str(), utterance_id.as_str());
                if let Some(&index) = caption_index.get(&key) {
                    let known = &mut captions[index];
                    known.first_at = known.first_at.min(*at);
                    known.last_at = known.last_at.max(*at);
                    if *version >= known.version {
                        known.version = *version;
                        known.speaker.clone_from(speaker);
                        known.text.clone_from(text);
                    }
                } else {
                    caption_index.insert(key, captions.len());
                    captions.push(Caption {
                        device_id: device_id.clone(),
                        version: *version,
                        first_at: *at,
                        last_at: *at,
                        speaker: speaker.clone(),
                        text: text.clone(),
                    });
                }
            }
            CallEvent::Chat {
                at,
                message_id,
                device_id,
                sender,
                text,
                ..
            } => {
                if !chat_ids.contains(&message_id.as_str()) {
                    chat_ids.push(message_id);
                    chats.push(Chat {
                        device_id: device_id.as_deref(),
                        message: ChatMessage {
                            sent_at: *at,
                            sender: sender.clone(),
                            text: text.clone(),
                        },
                    });
                }
            }
            CallEvent::Roster { participants, .. } => {
                for participant in participants {
                    let is_known = attendees.iter().any(|known| {
                        known.platform_id.as_deref() == Some(participant.device_id.as_str())
                    });
                    if !is_known && !participant.name.trim().is_empty() {
                        attendees.push(Attendee {
                            name: participant.name.trim().to_owned(),
                            is_self: participant.is_self,
                            platform_id: Some(participant.device_id.clone()),
                        });
                    }
                }
            }
            CallEvent::CallStarted { .. }
            | CallEvent::Health { .. }
            | CallEvent::CallEnded { .. }
            | CallEvent::Unknown => {}
        }
    }
    let mut chat_messages = name_from_roster(&mut captions, chats, &attendees);
    captions.retain(|caption| !caption.text.trim().is_empty());
    captions.sort_by_key(|caption| caption.first_at);
    chat_messages.sort_by_key(|message| message.sent_at);
    Collected {
        captions,
        chat_messages,
        attendees,
    }
}

// A caption or chat sent before the roster named its device has no name. It gets the device's
// attendee name, from any roster in the log: a late roster still names early lines, and the
// label matches the attendee list.
fn name_from_roster(
    captions: &mut [Caption],
    chats: Vec<Chat>,
    attendees: &[Attendee],
) -> Vec<ChatMessage> {
    let roster_name = |device_id: &str| {
        attendees
            .iter()
            .find(|attendee| attendee.platform_id.as_deref() == Some(device_id))
            .map(|attendee| attendee.name.clone())
    };
    for caption in captions {
        if caption.speaker.trim().is_empty()
            && let Some(name) = roster_name(&caption.device_id)
        {
            caption.speaker = name;
        }
    }
    chats
        .into_iter()
        .map(
            |Chat {
                 device_id,
                 mut message,
             }| {
                if message.sender.trim().is_empty()
                    && let Some(name) = device_id.and_then(roster_name)
                {
                    message.sender = name;
                }
                message
            },
        )
        .collect()
}

fn segments(captions: Vec<Caption>, started_at: Timestamp) -> Vec<Segment> {
    let offset_s = |at: Timestamp| {
        at.duration_since(started_at)
            .max(SignedDuration::ZERO)
            .as_secs_f64()
    };
    captions
        .into_iter()
        .map(|caption| {
            let start_s = offset_s(caption.first_at);
            Segment {
                start_s,
                end_s: offset_s(caption.last_at).max(start_s),
                speaker: caption.speaker,
                text: caption.text,
            }
        })
        .collect()
}
