//! Turns each input into the common transcript format.

pub mod transcriptonic;

use jiff::Timestamp;
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
    pub segments: Vec<Segment>,
    pub chat_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    Transcriptonic,
    Recorder,
    MeetApi,
    AudioFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Meet,
    Teams,
    Zoom,
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
