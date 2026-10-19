//! Turns each input into the common transcript format, and writes the logs the app records
//! calls in.

pub mod append_log;
pub mod browser_call;
pub mod slack_huddle;
pub mod transcriptonic;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// One meeting's transcript, the same shape whatever produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub source: Source,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    pub platform: Platform,
    pub meeting_url: Option<String>,
    /// The title the call app showed. The calendar event title wins when one matches.
    pub title: Option<String>,
    pub speaker_naming: SpeakerNaming,
    /// The speaker label the local user's lines carry, when the source can tell.
    pub my_speaker: Option<String>,
    /// The people the call app showed. Empty when the source does not list them.
    pub attendees: Vec<Attendee>,
    pub segments: Vec<Segment>,
    pub chat_messages: Vec<ChatMessage>,
    /// What only a Slack huddle has. `None` for every other source.
    pub slack_huddle: Option<SlackHuddle>,
}

impl Transcript {
    /// The time the meeting ran.
    #[must_use]
    pub fn duration(&self) -> SignedDuration {
        self.ended_at.duration_since(self.started_at)
    }
}

/// A participant as the call app showed them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub name: String,
    pub is_self: bool,
    /// The call app's id for the person, such as a Slack user id.
    pub platform_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackHuddle {
    pub workspace: String,
    /// A channel name, or the other people's names in a DM.
    pub channel: String,
    pub slack_version: String,
    /// The layout profile that read the huddle window, or "none".
    pub layout_profile: String,
    /// The worst health the watcher reported during the huddle.
    pub health: slack_huddle::HuddleHealth,
    /// The caption language Slack showed, such as "English (US)".
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Transcriptonic,
    Recorder,
    MeetApi,
    AudioFile,
    SlackHuddle,
    /// The mc-meet Chrome extension.
    MeetExtension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Meet,
    Teams,
    Zoom,
    Slack,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeakerNaming {
    /// Each segment carries the participant's display name.
    Real,
    /// Two stems: the local user and everyone else.
    MeVsThem,
    /// Anonymous speaker labels from diarization.
    Diarized,
}

/// Seconds are offsets from `Transcript::started_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start_s: f64,
    pub end_s: f64,
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub sent_at: Timestamp,
    pub sender: String,
    pub text: String,
}
