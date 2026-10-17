//! Renders a meeting note and moves it into the vault in one rename.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use jobs::MeetingId;
use serde::Serialize;
use sources::{SpeakerNaming, Transcript};

use crate::PipelineError;
use crate::calendar::CalendarMatch;
use crate::extraction::{CheckedCitation, CheckedNotes, CommitmentStrength};

const BUILT_IN_TEMPLATE: &str = include_str!("note.md.j2");
const UNTITLED_CALL: &str = "Untitled call";
const NEEDS_EXTRACTION_TAG: &str = "status/needs-extraction";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    extraction: Option<&'a str>,
}

// The frontmatter keys the app writes. On a rerun these are rewritten; any other key stays.
pub(crate) const APP_KEYS: [&str; 10] = [
    "title",
    "date",
    "created",
    "updated",
    "tags",
    "source",
    "meeting_url",
    "meeting_id",
    "attendees",
    "extraction",
];
// Tags the app adds or removes. Other tags in `tags` are the user's and stay.
pub(crate) const APP_TAGS: [&str; 4] = [
    "area/meetings",
    "topic/transcript",
    "status/active",
    NEEDS_EXTRACTION_TAG,
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
pub(crate) fn render(
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
    let started_at = transcript.started_at.to_zoned(note.time_zone.clone());
    let attendees: Vec<String> = note
        .calendar_match
        .event
        .iter()
        .flat_map(|event| event.attendees.clone())
        .collect();
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
            meeting_url => transcript.meeting_url.as_deref(),
            attendees => attendees.iter().map(|attendee| inline_markdown(attendee)).collect::<Vec<_>>(),
            segments => minijinja::value::Serde(&segments),
            notes => minijinja::value::Serde(&notes),
            chat_messages => minijinja::value::Serde(&chat_messages),
        })?;
    Ok(rendered)
}

// TranscripTonic's name for the local user.
const TRANSCRIPTONIC_ME: &str = "You";

/// The name a transcript speaker goes by in the note and the prompt.
pub(crate) fn speaker_name<'a>(
    transcript: &Transcript,
    my_name: Option<&'a str>,
    speaker: &'a str,
) -> &'a str {
    match transcript.speaker_naming {
        SpeakerNaming::Real if speaker == TRANSCRIPTONIC_ME => my_name.unwrap_or(speaker),
        SpeakerNaming::Real | SpeakerNaming::MeVsThem | SpeakerNaming::Diarized => speaker,
    }
}

/// The local user's speaker name, when the transcript can tell which speaker that is.
pub(crate) fn my_speaker_name<'a>(
    transcript: &Transcript,
    my_name: Option<&'a str>,
) -> Option<&'a str> {
    match transcript.speaker_naming {
        SpeakerNaming::Real => Some(my_name.unwrap_or(TRANSCRIPTONIC_ME)),
        // The recorder's stem labels arrive with it in phase 3; diarized labels are anonymous.
        SpeakerNaming::MeVsThem | SpeakerNaming::Diarized => None,
    }
}

fn frontmatter_yaml(note: &NoteInput<'_>, attendees: &[String]) -> Result<String, PipelineError> {
    let transcript = note.transcript;
    let started_at = transcript.started_at.to_zoned(note.time_zone.clone());
    let mut tags = vec!["area/meetings", "topic/transcript", "status/active"];
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
        extraction: note.notes.and(note.extraction_provider),
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
        Self {
            text: inline_markdown(text),
            links: links.join(" "),
            owner: None,
            recipient: None,
            due: None,
            strength: None,
        }
    }
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
                ..ItemView::new(&item.task, &item.citations)
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
                ..ItemView::new(&item.task, &item.citations)
            })
            .collect(),
    }
}

/// Note paths to try for a new meeting, in order: `YYYY/MM/yyyy-mm-dd-hhmm-<title-slug>.md`
/// under `meetings_dir`, then the same with the meeting id, which is unique to the meeting.
pub(crate) fn note_path_candidates(meetings_dir: &Path, note: &NoteInput<'_>) -> [PathBuf; 2] {
    let started_at = note.transcript.started_at.to_zoned(note.time_zone.clone());
    let month_dir = meetings_dir
        .join(started_at.strftime("%Y").to_string())
        .join(started_at.strftime("%m").to_string());
    let date_time = started_at.strftime("%Y-%m-%d-%H%M");
    let title = title_slug(note.title());
    let id = note.meeting_id;
    [
        month_dir.join(format!("{date_time}-{title}.md")),
        month_dir.join(format!("{date_time}-{title}-{id}.md")),
    ]
}

// Long event titles would push paths past Windows' 260-character limit.
const MAX_SLUG_CHARS: usize = 60;

fn title_slug(title: &str) -> String {
    let slug = slug::slugify(title);
    let mut slug = if slug.len() <= MAX_SLUG_CHARS {
        slug
    } else {
        let cut = slug[..MAX_SLUG_CHARS].rfind('-').unwrap_or(MAX_SLUG_CHARS);
        slug[..cut].to_owned()
    };
    if slug.is_empty() {
        slug = slug::slugify(UNTITLED_CALL);
    }
    slug
}

/// SHA-256 of a note, in lowercase hex. The job store keeps it to tell the pipeline's own write
/// from a user edit.
pub(crate) fn note_sha256(contents: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    sha2::Sha256::digest(contents)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Writes `contents` to a staging file outside the vault, then renames it over `note_path`.
/// `check_before_rename` runs once the staged file is on disk, as close to the rename as possible.
pub(crate) fn write_atomically(
    staging_dir: &Path,
    note_path: &Path,
    contents: &str,
    check_before_rename: impl Fn() -> Result<(), PipelineError>,
) -> Result<(), PipelineError> {
    let write_error = |source| PipelineError::WriteNote {
        path: note_path.to_path_buf(),
        source,
    };
    let note_dir = note_path
        .parent()
        .ok_or_else(|| write_error(std::io::Error::other("note path has no folder")))?;
    create_dir_durably(note_dir).map_err(write_error)?;

    let staged = tempfile::NamedTempFile::new_in(staging_dir).map_err(write_error)?;
    let staged = fill(staged, contents).map_err(write_error)?;
    check_before_rename()?;
    match staged.persist(note_path) {
        Ok(_) => {}
        // A rename works only within one volume. For a vault on another disk, stage next to the
        // note under a dot-name, which Obsidian hides.
        Err(error) if error.error.kind() == std::io::ErrorKind::CrossesDevices => {
            let staged = tempfile::Builder::new()
                .prefix(".")
                .suffix(".tmp")
                .tempfile_in(note_dir)
                .map_err(write_error)?;
            let staged = fill(staged, contents).map_err(write_error)?;
            check_before_rename()?;
            staged
                .persist(note_path)
                .map_err(|error| write_error(error.error))?;
        }
        Err(error) => return Err(write_error(error.error)),
    }
    sync_dir(note_dir).map_err(write_error)
}

// A new month folder survives power loss only once its parent's entry is on disk too.
fn create_dir_durably(dir: &Path) -> std::io::Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|path| !path.exists()).collect();
    std::fs::create_dir_all(dir)?;
    for created in missing.into_iter().rev() {
        if let Some(parent) = created.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

// A rename or a new entry is durable only once the folder is on disk. Windows has no folder fsync.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn fill(
    mut staged: tempfile::NamedTempFile,
    contents: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    staged.write_all(contents.as_bytes())?;
    // Temp files are private (0600); a note gets the vault's usual mode.
    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    staged.as_file().sync_all()?;
    Ok(staged)
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
