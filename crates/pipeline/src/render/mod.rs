//! Renders a meeting note to a string. Only the template override is read from disk; the vault
//! module writes the note.

use std::collections::BTreeMap;
use std::path::Path;

use call_folder::media::{
    CallMedia, CallVideo, MediaLink, MediaTarget, StemSide, VIDEO_FILE_NAME, stem_of,
};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use jobs::MeetingId;
use serde::Serialize;
use sources::slack_huddle::HuddleHealth;
use sources::{SlackHuddle, Transcript};

use crate::PipelineError;
use crate::calendar::CalendarMatch;
use crate::extraction::{CheckedCitation, CheckedNotes, CommitmentStrength};
use crate::transcribe::{FallbackReason, SavedChoice, SpeechChoice};

const BUILT_IN_TEMPLATE: &str = include_str!("note.md.j2");
pub(crate) const UNTITLED_CALL: &str = "Untitled call";
const NEEDS_EXTRACTION_TAG: &str = "status/needs-extraction";
const SLACK_HUDDLE_TAG: &str = "topic/slack-huddle";
const CAPTIONS_DEGRADED_TAG: &str = "status/captions-degraded";

pub(crate) struct NoteInput<'a> {
    pub meeting_id: MeetingId,
    pub transcript: &'a Transcript,
    pub calendar_match: &'a CalendarMatch,
    /// `None` when no notes were extracted; the note then carries only the transcript.
    pub notes: Option<&'a CheckedNotes>,
    /// `<provider>/<model>` that wrote `notes`.
    pub extraction_provider: Option<&'a str>,
    pub my_name: Option<&'a str>,
    pub time_zone: &'a TimeZone,
    /// `None` when the meeting has no call folder on this machine.
    pub recording: Option<&'a CallRecording>,
    /// What the transcribe step settled on.
    pub transcription: &'a SavedChoice,
}

/// A meeting's call folder and the media in it.
pub(crate) struct CallRecording {
    /// The link to the folder itself; each file's link is built from it.
    pub folder_link: MediaLink,
    pub media: CallMedia,
    /// The Drive copies of archived files no longer in the folder, by local name such as
    /// `call.mp4` or `001-mic.wav`.
    pub drive_copies: BTreeMap<String, DriveCopy>,
}

/// A file the Drive archive holds in place of the local one.
pub(crate) struct DriveCopy {
    pub web_view_link: String,
    pub size_bytes: u64,
}

impl NoteInput<'_> {
    pub fn title(&self) -> &str {
        self.calendar_match
            .event
            .as_ref()
            .and_then(|event| event.title.as_deref())
            .or(self.transcript.title.as_deref())
            .unwrap_or(UNTITLED_CALL)
    }

    /// In the user's time zone, which the frontmatter and the note's file name both use.
    pub(crate) fn started_at(&self) -> Zoned {
        self.transcript.started_at.to_zoned(self.time_zone.clone())
    }
}

#[derive(Serialize)]
struct Frontmatter<'a> {
    title: &'a str,
    date: String,
    created: String,
    updated: String,
    tags: Vec<&'static str>,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    meeting_url: Option<&'a str>,
    meeting_id: i64,
    attendees: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    invited: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extraction: Option<&'a str>,
    /// Where the transcript's lines came from: `captions`, the provider such as `deepgram`,
    /// `mixed`, or `pending` while speech-to-text is owed. Absent when the captions passed
    /// every check.
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription: Option<&'static str>,
    /// The speech-to-text `<provider>/<model>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription_provider: Option<&'a str>,
    /// Why lines of the captions were replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription_reason: Option<&'static str>,
    /// Minutes of the call whose lines came from speech-to-text.
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription_minutes: Option<f64>,
    /// Why the provider refused, when the captions stayed for that reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    transcription_error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slack_workspace: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slack_channel: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    slack_attendees: Vec<SlackAttendee<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    captions: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slack_version: Option<&'a str>,
}

#[derive(Serialize)]
struct SlackAttendee<'a> {
    id: &'a str,
    name: &'a str,
}

// The frontmatter keys the app writes. On a rerun these are rewritten; any other key stays.
pub(crate) const APP_KEYS: [&str; 23] = [
    "title",
    "date",
    "created",
    "updated",
    "tags",
    "source",
    "meeting_url",
    "meeting_id",
    "attendees",
    "invited",
    "extraction",
    "transcription",
    "transcription_provider",
    "transcription_reason",
    "transcription_minutes",
    "transcription_error",
    "slack_workspace",
    "slack_channel",
    "slack_attendees",
    "duration_min",
    "language",
    "captions",
    "slack_version",
];
// Tags the app adds or removes. Other tags in `tags` are the user's and stay.
pub(crate) const APP_TAGS: [&str; 6] = [
    "area/meetings",
    "topic/transcript",
    "status/active",
    NEEDS_EXTRACTION_TAG,
    SLACK_HUDDLE_TAG,
    CAPTIONS_DEGRADED_TAG,
];

#[derive(Serialize)]
struct SegmentView<'a> {
    /// The block id suffix, `^s<id>`, that citations link to.
    id: usize,
    at: String,
    speaker: &'a str,
    text: String,
}

#[derive(Serialize)]
struct ChatMessageView<'a> {
    at: String,
    sender: &'a str,
    text: String,
}

/// Renders the note with the template at `template_override` if it exists, else the built-in one.
pub(crate) fn note(
    note: &NoteInput<'_>,
    template_override: &Path,
) -> Result<String, PipelineError> {
    let template = match std::fs::read_to_string(template_override) {
        Ok(template) => template,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BUILT_IN_TEMPLATE.to_owned(),
        Err(source) => {
            return Err(PipelineError::ReadFile {
                path: template_override.to_path_buf(),
                source,
            });
        }
    };

    let transcript = note.transcript;
    let started_at = note.started_at();
    let attendees = attendees(transcript, note.my_name);
    let frontmatter = frontmatter_yaml(note, &attendees)?;

    let speaker_name = |speaker: &str| -> String {
        inline_markdown(speaker_name(transcript, note.my_name, speaker))
    };
    let speakers: Vec<String> = transcript
        .segments
        .iter()
        .map(|segment| speaker_name(&segment.speaker))
        .collect();
    let senders: Vec<String> = transcript
        .chat_messages
        .iter()
        .map(|message| speaker_name(&message.sender))
        .collect();
    let segments: Vec<SegmentView<'_>> = transcript
        .segments
        .iter()
        .zip(&speakers)
        .enumerate()
        .map(|(index, (segment, speaker))| SegmentView {
            id: index + 1,
            at: offset_label(segment.start_s),
            speaker,
            text: inline_markdown(&segment.text),
        })
        .collect();
    let chat_messages: Vec<ChatMessageView<'_>> = transcript
        .chat_messages
        .iter()
        .zip(&senders)
        .map(|(message, sender)| ChatMessageView {
            at: offset_label(
                message
                    .sent_at
                    .duration_since(transcript.started_at)
                    .as_secs_f64(),
            ),
            sender,
            text: inline_markdown(&message.text),
        })
        .collect();

    let notes = note.notes.map(notes_view);
    let recording = note
        .recording
        .map(|recording| recording_view(note, recording));

    let mut environment = minijinja::Environment::new();
    environment.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .trim_blocks(true)
            .lstrip_blocks(true)
            .keep_trailing_newline(true)
            .build()?,
    );
    environment.add_template_owned("note.md", template)?;
    let rendered = environment
        .get_template("note.md")?
        .render(minijinja::context! {
            frontmatter,
            title => inline_markdown(note.title()),
            when => started_at.strftime("%Y-%m-%d %H:%M %Z").to_string(),
            huddle => minijinja::value::Serde(transcript.slack_huddle.as_ref().map(|huddle| huddle_view(transcript, huddle))),
            meeting_url => transcript.meeting_url.as_deref(),
            attendees => attendees.iter().map(|attendee| inline_markdown(attendee)).collect::<Vec<_>>(),
            recording => minijinja::value::Serde(&recording),
            segments => minijinja::value::Serde(&segments),
            notes => minijinja::value::Serde(&notes),
            chat_messages => minijinja::value::Serde(&chat_messages),
            transcription => transcription_line(note.transcription, transcript),
            shows_caption_fixes => has_caption_lines(note.transcription),
        })?;
    Ok(rendered)
}

/// The `transcription` frontmatter value.
fn transcription_source(choice: &SavedChoice) -> Option<&'static str> {
    match choice {
        SavedChoice::Captions => None,
        SavedChoice::CaptionsKept { .. } => Some("captions"),
        SavedChoice::Pending { .. } => Some("pending"),
        SavedChoice::Speech(speech) if speech.has_caption_lines => Some("mixed"),
        SavedChoice::Speech(speech) => Some(speech.provider.as_str()),
    }
}

/// Whether caption lines remain, so caption fixes still apply.
fn has_caption_lines(choice: &SavedChoice) -> bool {
    match choice {
        SavedChoice::Captions | SavedChoice::Pending { .. } | SavedChoice::CaptionsKept { .. } => {
            true
        }
        SavedChoice::Speech(speech) => speech.has_caption_lines,
    }
}

/// The line above the transcript: where its lines came from and why, when not all captions.
fn transcription_line(choice: &SavedChoice, transcript: &Transcript) -> Option<String> {
    match choice {
        SavedChoice::Captions => None,
        SavedChoice::Pending { provider_label, .. } => Some(format!(
            "These lines are the captions. A transcript from the call audio by {provider_label} replaces them once the audio uploads."
        )),
        SavedChoice::CaptionsKept {
            provider_label,
            error,
            ..
        } => Some(format!(
            "These lines are the captions: the call audio got no transcript from {provider_label} ({error})."
        )),
        SavedChoice::Speech(speech) => {
            let why = match speech.reason {
                FallbackReason::Empty => "the captions were empty",
                FallbackReason::TooFew => "the captions had almost no lines",
                FallbackReason::Failed => "the captions failed",
                FallbackReason::Sparse => "the captions missed speech",
                FallbackReason::OtherLanguage => "the captions were in another language",
                FallbackReason::NonEnglishSpeech => "the call was partly not in English",
                FallbackReason::Garbled => "the captions were garbled",
                FallbackReason::Forced => "`mc transcribe` asked for it",
            };
            let source = if speech.has_caption_lines {
                format!(
                    "{:.0} minutes of this transcript come from the call audio, transcribed by {} since {why}. The rest are the captions.",
                    speech.speech_min, speech.provider_label
                )
            } else {
                format!(
                    "Transcribed from the call audio by {}, since {why}.",
                    speech.provider_label
                )
            };
            let numbering = match transcript.speaker_naming {
                sources::SpeakerNaming::Diarized => {
                    " Speakers the captions could not name are numbered within each 5-minute part."
                }
                sources::SpeakerNaming::Real | sources::SpeakerNaming::MeVsThem => "",
            };
            Some(format!("{source}{numbering}"))
        }
    }
}

/// The name a transcript speaker goes by in the note and the prompt: the settings name for the
/// local user, else the speaker label.
pub(crate) fn speaker_name<'a>(
    transcript: &Transcript,
    my_name: Option<&'a str>,
    speaker: &'a str,
) -> &'a str {
    if transcript.my_speaker.as_deref() == Some(speaker) {
        my_name.unwrap_or(speaker)
    } else {
        speaker
    }
}

/// The local user's speaker name, when the transcript can tell which speaker that is.
pub(crate) fn my_speaker_name<'a>(
    transcript: &'a Transcript,
    my_name: Option<&'a str>,
) -> Option<&'a str> {
    transcript
        .my_speaker
        .as_deref()
        .map(|label| my_name.unwrap_or(label))
}

/// The people who joined the call: the call app's participant list, else everyone who spoke or
/// wrote in the chat, plus the local user. Invitees who never joined are left out.
pub(crate) fn attendees(transcript: &Transcript, my_name: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    };
    if transcript.attendees.is_empty() {
        let people = transcript
            .segments
            .iter()
            .map(|segment| segment.speaker.as_str())
            .chain(
                transcript
                    .chat_messages
                    .iter()
                    .map(|message| message.sender.as_str()),
            )
            .chain(transcript.my_speaker.as_deref());
        for speaker in people {
            add(speaker_name(transcript, my_name, speaker));
        }
    } else {
        for attendee in &transcript.attendees {
            add(&attendee.name);
        }
    }
    names
}

/// Everyone the matched calendar event invites, whether or not they joined.
pub(crate) fn invited(calendar_match: &CalendarMatch) -> Vec<String> {
    calendar_match
        .event
        .iter()
        .flat_map(|event| &event.invited)
        .map(|invitee| invitee.name.clone())
        .collect()
}

#[derive(Serialize)]
struct HuddleView {
    /// "#team-review in Acme", or the DM's names.
    place: String,
    duration: String,
    /// Set when the captions were not read in full.
    captions_warning: Option<&'static str>,
}

fn huddle_view(transcript: &Transcript, huddle: &SlackHuddle) -> HuddleView {
    let place = if sources::slack_huddle::is_dm(&huddle.channel) {
        format!("DM with {} in {}", huddle.channel, huddle.workspace)
    } else {
        format!("#{} in {}", huddle.channel, huddle.workspace)
    };
    let captions_warning = match huddle.health {
        HuddleHealth::Healthy | HuddleHealth::Untested => None,
        HuddleHealth::Degraded => Some("read in part; some lines or names may be missing"),
        HuddleHealth::CaptionsOff => Some("off for part of the huddle"),
        HuddleHealth::Broken => Some("could not be read for part of the huddle"),
    };
    HuddleView {
        place: inline_markdown(&place),
        duration: format!("{} min", duration_min(transcript)),
        captions_warning,
    }
}

#[derive(Serialize)]
struct RecordingView {
    video_url: Option<String>,
    /// "24 min, 69 MB".
    video_length: Option<String>,
    /// The purge's date in the user's time zone.
    video_purged_on: Option<String>,
    parts: Vec<RecordingPartView>,
    folder_url: String,
}

#[derive(Serialize)]
struct RecordingPartView {
    /// "Part 2", set only when the call has more than one segment.
    label: Option<String>,
    mic_url: Option<String>,
    remote_url: Option<String>,
    /// A link opens the Drive copy.
    is_on_drive: bool,
}

/// One segment's stem links, local or on Drive.
#[derive(Default)]
struct PartLinks {
    mic_url: Option<String>,
    remote_url: Option<String>,
    is_on_drive: bool,
}

fn recording_view(note: &NoteInput<'_>, recording: &CallRecording) -> RecordingView {
    let url = |target: MediaTarget| recording.folder_link.with_target(target).url();
    let drive_video = recording.drive_copies.get(VIDEO_FILE_NAME);
    let (video_url, video_length, video_purged_on) = match (recording.media.video, drive_video) {
        (CallVideo::Kept { bytes }, _) => (
            Some(url(MediaTarget::Video)),
            Some(format!(
                "{} min, {}",
                duration_min(note.transcript),
                file_size_label(bytes)
            )),
            None,
        ),
        (CallVideo::Purged { .. } | CallVideo::Missing, Some(copy)) => (
            Some(copy.web_view_link.clone()),
            Some(format!(
                "{} min, {}, on Google Drive",
                duration_min(note.transcript),
                file_size_label(copy.size_bytes)
            )),
            None,
        ),
        (CallVideo::Purged { purged_at }, None) => (
            None,
            None,
            Some(
                purged_at
                    .to_zoned(note.time_zone.clone())
                    .strftime("%Y-%m-%d")
                    .to_string(),
            ),
        ),
        (CallVideo::Missing, None) => (None, None, None),
    };
    let mut part_links: BTreeMap<u32, PartLinks> = BTreeMap::new();
    for segment in &recording.media.audio_segments {
        let links = part_links.entry(segment.number).or_default();
        for (side, is_present) in [
            (StemSide::Mic, segment.has_mic),
            (StemSide::Remote, segment.has_remote),
        ] {
            if is_present {
                links.set(
                    side,
                    url(MediaTarget::Stem {
                        number: segment.number,
                        side,
                    }),
                );
            }
        }
    }
    for (local_name, copy) in &recording.drive_copies {
        if let Some((number, side)) = stem_of(local_name) {
            let links = part_links.entry(number).or_default();
            links.set(side, copy.web_view_link.clone());
            links.is_on_drive = true;
        }
    }
    let has_parts = part_links.len() > 1;
    let parts = part_links
        .into_iter()
        .map(|(number, links)| RecordingPartView {
            label: has_parts.then(|| format!("Part {number}")),
            mic_url: links.mic_url,
            remote_url: links.remote_url,
            is_on_drive: links.is_on_drive,
        })
        .collect();
    RecordingView {
        video_url,
        video_length,
        video_purged_on,
        parts,
        folder_url: recording.folder_link.url(),
    }
}

impl PartLinks {
    fn set(&mut self, side: StemSide, url: String) {
        match side {
            StemSide::Mic => self.mic_url = Some(url),
            StemSide::Remote => self.remote_url = Some(url),
        }
    }
}

// Decimal units, as Finder and Explorer show sizes.
fn file_size_label(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000 * MB;
    if bytes >= GB {
        format!("{}.{} GB", bytes / GB, bytes % GB / (GB / 10))
    } else {
        format!("{} MB", (bytes + MB / 2) / MB)
    }
}

fn duration_min(transcript: &Transcript) -> i64 {
    transcript.duration().as_secs().max(0).saturating_add(59) / 60
}

fn frontmatter_yaml(note: &NoteInput<'_>, attendees: &[String]) -> Result<String, PipelineError> {
    let transcript = note.transcript;
    let started_at = note.started_at();
    let mut tags = vec!["area/meetings", "topic/transcript", "status/active"];
    let huddle = transcript.slack_huddle.as_ref();
    if let Some(huddle) = huddle {
        tags.push(SLACK_HUDDLE_TAG);
        if huddle.health.is_warning() {
            tags.push(CAPTIONS_DEGRADED_TAG);
        }
    }
    if note.notes.is_none() {
        tags.push(NEEDS_EXTRACTION_TAG);
    }
    let frontmatter = Frontmatter {
        title: note.title(),
        date: started_at.strftime("%Y-%m-%d").to_string(),
        created: started_at.strftime("%Y-%m-%d %H:%M").to_string(),
        updated: Timestamp::now()
            .to_zoned(note.time_zone.clone())
            .strftime("%Y-%m-%d")
            .to_string(),
        tags,
        source: source_name(transcript),
        meeting_url: transcript.meeting_url.as_deref(),
        meeting_id: note.meeting_id.as_i64(),
        attendees: attendees.to_vec(),
        invited: invited(note.calendar_match),
        extraction: note.notes.and(note.extraction_provider),
        transcription: transcription_source(note.transcription),
        transcription_provider: match note.transcription {
            SavedChoice::Captions => None,
            SavedChoice::Pending { provider_label, .. }
            | SavedChoice::CaptionsKept { provider_label, .. }
            | SavedChoice::Speech(SpeechChoice { provider_label, .. }) => {
                Some(provider_label.as_str())
            }
        },
        transcription_reason: match note.transcription {
            SavedChoice::Captions => None,
            SavedChoice::Pending { reason, .. } | SavedChoice::CaptionsKept { reason, .. } => {
                reason.map(FallbackReason::as_str)
            }
            SavedChoice::Speech(speech) => Some(speech.reason.as_str()),
        },
        transcription_minutes: match note.transcription {
            SavedChoice::Captions
            | SavedChoice::Pending { .. }
            | SavedChoice::CaptionsKept { .. } => None,
            SavedChoice::Speech(speech) => Some((speech.speech_min * 10.0).round() / 10.0),
        },
        transcription_error: match note.transcription {
            SavedChoice::Captions | SavedChoice::Pending { .. } | SavedChoice::Speech(_) => None,
            SavedChoice::CaptionsKept { error, .. } => Some(error.as_str()),
        },
        slack_workspace: huddle.map(|huddle| huddle.workspace.as_str()),
        slack_channel: huddle.map(|huddle| huddle.channel.as_str()),
        slack_attendees: huddle
            .map(|_| {
                transcript
                    .attendees
                    .iter()
                    .filter_map(|attendee| {
                        Some(SlackAttendee {
                            id: attendee.platform_id.as_deref()?,
                            name: &attendee.name,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        duration_min: huddle.map(|_| duration_min(transcript)),
        language: huddle.and_then(|huddle| huddle.language.as_deref()),
        captions: huddle.map(|huddle| huddle.health.as_str()),
        slack_version: huddle.map(|huddle| huddle.slack_version.as_str()),
    };
    serde_saphyr::to_string(&frontmatter)
        .map_err(|error| PipelineError::Frontmatter(error.to_string()))
}

#[derive(Serialize)]
struct NotesView {
    summary: String,
    decisions: Vec<ItemView>,
    action_items: Vec<ItemView>,
    open_questions: Vec<ItemView>,
    my_commitments: Vec<ItemView>,
    caption_fixes: Vec<CaptionFixView>,
}

#[derive(Serialize)]
struct CaptionFixView {
    heard: String,
    likely: String,
    is_unsure: bool,
    links: String,
}

#[derive(Serialize)]
struct ItemView {
    text: String,
    /// Citation links, each `[[#^s<id>|<time>]]`, space separated.
    links: String,
    owner: Option<String>,
    recipient: Option<String>,
    due: Option<String>,
    strength: Option<&'static str>,
}

impl ItemView {
    fn new(text: &str, citations: &[CheckedCitation]) -> Self {
        Self {
            text: inline_markdown(text),
            links: citation_links(citations),
            owner: None,
            recipient: None,
            due: None,
            strength: None,
        }
    }
}

// The template follows a task with ", for <name>" or "(due <date>)", so a closing full stop would
// land mid-line.
fn without_full_stop(task: &str) -> &str {
    task.trim_end().trim_end_matches('.')
}

/// One `[[#^s<id>|<time>]]` link per cited segment, space separated.
fn citation_links(citations: &[CheckedCitation]) -> String {
    let mut linked: Vec<u32> = Vec::new();
    let mut links = Vec::new();
    for citation in citations {
        if !linked.contains(&citation.segment_id) {
            linked.push(citation.segment_id);
            links.push(format!(
                "[[#^s{}|{}]]",
                citation.segment_id,
                offset_label(citation.start_s)
            ));
        }
    }
    links.join(" ")
}

// What the model wrote is escaped like transcript text: it quotes the call, and the call is data.
fn notes_view(notes: &CheckedNotes) -> NotesView {
    let escaped = |text: &Option<String>| text.as_deref().map(inline_markdown);
    NotesView {
        summary: inline_markdown(&notes.summary),
        decisions: notes
            .decisions
            .iter()
            .map(|item| ItemView::new(&item.decision, &item.citations))
            .collect(),
        action_items: notes
            .action_items
            .iter()
            .map(|item| ItemView {
                owner: escaped(&item.owner),
                due: escaped(&item.due),
                ..ItemView::new(without_full_stop(&item.task), &item.citations)
            })
            .collect(),
        open_questions: notes
            .open_questions
            .iter()
            .map(|item| ItemView::new(&item.question, &item.citations))
            .collect(),
        my_commitments: notes
            .my_commitments
            .iter()
            .map(|item| ItemView {
                recipient: escaped(&item.recipient),
                due: escaped(&item.due),
                strength: Some(match item.strength {
                    CommitmentStrength::Committed => "committed",
                    CommitmentStrength::Hedged => "hedged",
                    CommitmentStrength::Aspirational => "aspirational",
                }),
                ..ItemView::new(without_full_stop(&item.task), &item.citations)
            })
            .collect(),
        caption_fixes: notes
            .caption_fixes
            .iter()
            .map(|fix| CaptionFixView {
                heard: inline_markdown(&fix.heard),
                likely: inline_markdown(&fix.likely),
                is_unsure: fix.is_unsure,
                links: citation_links(&fix.citations),
            })
            .collect(),
    }
}

/// Makes what someone said one line of literal Markdown: it cannot open a heading, list, code
/// block, comment, link, embed, tag, math or HTML in the note.
fn inline_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for (index, word) in text.split_whitespace().enumerate() {
        if index > 0 {
            escaped.push(' ');
        }
        for character in word.chars() {
            // `^` too: a trailing ` ^s12` would be a second block id that citations jump to.
            if "\\`*_[]<>#%$|~=!^".contains(character) {
                escaped.push('\\');
            }
            escaped.push(character);
        }
    }
    escaped
}

fn source_name(transcript: &Transcript) -> &'static str {
    match transcript.source {
        sources::Source::Transcriptonic => "transcriptonic",
        sources::Source::Recorder => "recorder",
        sources::Source::MeetApi => "meet-api",
        sources::Source::AudioFile => "audio-file",
        sources::Source::SlackHuddle => "slack-huddle",
        sources::Source::MeetExtension => "meet-extension",
    }
}

fn offset_label(offset_s: f64) -> String {
    // Whole seconds are enough to find a moment in the call.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let total_s = offset_s.max(0.0) as u64;
    let (hours, minutes, seconds) = (total_s / 3600, total_s / 60 % 60, total_s % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
