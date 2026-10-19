//! Intake: reads an input file, parses it with the parser for its kind, skips a short huddle when
//! the tray asks, copies the file into the app folder for its kind when it is not there yet, and
//! records the meeting.

use std::path::{Path, PathBuf};

use jobs::{InputKind, MeetingId, StoreError};
use sources::Transcript;

use crate::Pipeline;

// A huddle with almost no captions was most likely a misclick or a quick check.
const MIN_CAPTION_SEGMENTS: usize = 3;

/// What intake did with a huddle log the tray passed.
#[derive(Debug, Clone, Copy)]
pub enum Intake {
    Recorded(MeetingId),
    /// Shorter than the tray's minimum, or with too few caption segments. Intake logs the length.
    TooShort,
}

/// A saved input that cannot be read, or does not parse as its kind.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    ParseTranscriptonic(#[from] sources::transcriptonic::ParseError),
    #[error(transparent)]
    ParseSlackHuddle(#[from] sources::slack_huddle::ParseError),
}

/// Why intake could not record a meeting for an input.
#[derive(Debug, thiserror::Error)]
pub enum IntakeError {
    /// The file is unreadable as its kind; a retry with the same file fails the same way.
    #[error(transparent)]
    Input(#[from] InputError),
    #[error("could not copy {input_path} into {folder}: {source}")]
    Copy {
        input_path: PathBuf,
        folder: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Why two meetings could not be merged.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("meeting {id} is a {kind}; only TranscripTonic meetings can be merged")]
    NotMergeable { id: MeetingId, kind: &'static str },
    #[error(transparent)]
    Input(#[from] InputError),
    #[error("could not save the merged payload in {folder}: {source}")]
    Save {
        folder: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Pipeline {
    /// Parses an input file as `kind`, copies it into the app folder for its kind when it is not
    /// there yet, and records the meeting, however short a huddle is. A payload from outside the
    /// payload folder gets a new name there; a huddle log gets its start-time name. Recording a
    /// meeting already in the store, such as a re-posted payload, reuses that meeting and its
    /// note.
    ///
    /// # Errors
    ///
    /// Returns `IntakeError::Input` when the file cannot be read or parsed as `kind`, and
    /// `IntakeError::Copy` or `IntakeError::Store` when it cannot be saved or recorded.
    pub fn take_in(&self, kind: InputKind, input_path: &Path) -> Result<MeetingId, IntakeError> {
        let (input, transcript) = read_as(kind, input_path)?;
        self.record(kind, input_path, &input, &transcript)
    }

    /// Takes in a huddle log as `take_in` does, but skips a huddle shorter than
    /// `min_huddle_seconds` or with fewer than `MIN_CAPTION_SEGMENTS` caption segments. The tray
    /// does this. The log stays, so `mc ingest` can still write its note.
    ///
    /// # Errors
    ///
    /// Returns the errors of `take_in`.
    pub fn take_in_huddle_log_unless_short(
        &self,
        log_path: &Path,
        min_huddle_seconds: u32,
    ) -> Result<Intake, IntakeError> {
        let kind = InputKind::SlackHuddleLog;
        let (input, transcript) = read_as(kind, log_path)?;
        let length_s = transcript.duration().as_secs();
        let segments = transcript.segments.len();
        if length_s < i64::from(min_huddle_seconds) || segments < MIN_CAPTION_SEGMENTS {
            tracing::info!(
                huddle.length_s = length_s,
                caption.segments = segments,
                "short huddle; no note"
            );
            return Ok(Intake::TooShort);
        }
        let id = self.record(kind, log_path, &input, &transcript)?;
        Ok(Intake::Recorded(id))
    }

    /// Joins two meetings TranscripTonic filed for one call into `kept`, which keeps its note
    /// path. The merged payload is saved as a new payload, `dropped` leaves the store, and `kept`
    /// runs again. Returns the note path `dropped` held; the caller removes that note.
    ///
    /// # Errors
    ///
    /// Returns `MergeError::NotMergeable` for a meeting from another source, and the other
    /// `MergeError`s when a payload cannot be read, merged or saved.
    pub fn merge(
        &self,
        kept: MeetingId,
        dropped: MeetingId,
    ) -> Result<Option<PathBuf>, MergeError> {
        let mut payloads = Vec::with_capacity(2);
        for id in [kept, dropped] {
            let meeting = self.store.get(id)?;
            match meeting.input_kind {
                InputKind::TranscriptonicPayload => {}
                InputKind::SlackHuddleLog => {
                    return Err(MergeError::NotMergeable {
                        id,
                        kind: "Slack huddle",
                    });
                }
            }
            payloads.push(read_input(&meeting.input_path)?);
        }
        let merged = sources::transcriptonic::merge_payloads(&payloads[0], &payloads[1])
            .map_err(InputError::from)?;
        let transcript = parse(InputKind::TranscriptonicPayload, &merged)?;
        let payload_dir = self.app_dirs.payload_dir();
        let payload_path =
            sources::transcriptonic::save_payload(&payload_dir, &merged).map_err(|source| {
                MergeError::Save {
                    folder: payload_dir.clone(),
                    source,
                }
            })?;
        let dropped_note = self
            .store
            .merge(kept, dropped, &payload_path, transcript.started_at)?;
        tracing::info!(meeting.id = %kept, meeting.dropped_id = %dropped, "meetings merged");
        Ok(dropped_note)
    }

    fn record(
        &self,
        kind: InputKind,
        input_path: &Path,
        input: &[u8],
        transcript: &Transcript,
    ) -> Result<MeetingId, IntakeError> {
        let recorded_path = self.copy_in(kind, input_path, input, transcript)?;
        Ok(self.store.upsert_captured(
            kind,
            &recorded_path,
            transcript.started_at,
            transcript.title.as_deref(),
        )?)
    }

    /// Returns the path in the app folder to record. A file already in place stays there, so a
    /// payload the receiver saved, or a log the watcher wrote, never gets a second copy.
    fn copy_in(
        &self,
        kind: InputKind,
        input_path: &Path,
        input: &[u8],
        transcript: &Transcript,
    ) -> Result<PathBuf, IntakeError> {
        let copy_error = |folder: PathBuf| {
            move |source| IntakeError::Copy {
                input_path: input_path.to_path_buf(),
                folder,
                source,
            }
        };
        match kind {
            InputKind::TranscriptonicPayload => {
                let payload_dir = self.app_dirs.payload_dir();
                // Canonical first, so a bare file name passed from inside the folder has a parent.
                let is_in_place = std::fs::canonicalize(input_path).is_ok_and(|path| {
                    path.parent()
                        .is_some_and(|input_dir| is_same_file(input_dir, &payload_dir))
                });
                if is_in_place {
                    return Ok(input_path.to_path_buf());
                }
                sources::transcriptonic::save_payload(&payload_dir, input)
                    .map_err(copy_error(payload_dir))
            }
            // Named for its start time, so a huddle the tray skipped as too short is already in
            // place, and ingesting it writes its note.
            InputKind::SlackHuddleLog => {
                let log_dir = self.app_dirs.huddle_log_dir();
                let log_path =
                    log_dir.join(sources::slack_huddle::log_file_name(transcript.started_at));
                if !is_same_file(input_path, &log_path) {
                    std::fs::copy(input_path, &log_path).map_err(copy_error(log_dir))?;
                }
                Ok(log_path)
            }
        }
    }
}

// Canonical paths, so a relative or symlinked path from `mc ingest` still counts as in place.
// A path that does not exist is never the same as another.
fn is_same_file(path: &Path, other_path: &Path) -> bool {
    match (
        std::fs::canonicalize(path),
        std::fs::canonicalize(other_path),
    ) {
        (Ok(path), Ok(other_path)) => path == other_path,
        (Err(_), _) | (_, Err(_)) => false,
    }
}

/// Reads and parses a saved input with the parser for its kind.
pub(crate) fn read_transcript(
    kind: InputKind,
    input_path: &Path,
) -> Result<Transcript, InputError> {
    parse(kind, &read_input(input_path)?)
}

fn read_as(kind: InputKind, input_path: &Path) -> Result<(Vec<u8>, Transcript), InputError> {
    let input = read_input(input_path)?;
    let transcript = parse(kind, &input)?;
    Ok((input, transcript))
}

fn read_input(input_path: &Path) -> Result<Vec<u8>, InputError> {
    std::fs::read(input_path).map_err(|source| InputError::Read {
        path: input_path.to_path_buf(),
        source,
    })
}

fn parse(kind: InputKind, input: &[u8]) -> Result<Transcript, InputError> {
    match kind {
        InputKind::TranscriptonicPayload => Ok(sources::transcriptonic::parse(input)?),
        InputKind::SlackHuddleLog => Ok(sources::slack_huddle::parse(input)?),
    }
}
