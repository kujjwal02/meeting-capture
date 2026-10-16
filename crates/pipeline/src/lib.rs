//! Calendar match, LLM extraction and the vault write. No Tauri dependency.

pub mod calendar;
mod note;
pub mod settings;

use std::path::{Path, PathBuf};

use jobs::{
    InputKind, JobStore, LeaseRelease, Meeting, MeetingId, NotePathClaim, RunLease, Step,
    StoreError,
};
use sources::Transcript;

use crate::note::NoteInput;
use crate::settings::{AppDirs, Settings};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("could not read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    ParseTranscriptonic(#[from] sources::transcriptonic::ParseError),
    #[error("stored step output is not valid JSON: {0}")]
    StepOutput(#[from] serde_json::Error),
    #[error("meeting {0} finished the calendar match but has no stored match")]
    MissingCalendarMatch(MeetingId),
    #[error("note template failed: {0}")]
    Template(#[from] minijinja::Error),
    #[error("could not write note frontmatter: {0}")]
    Frontmatter(String),
    #[error("every note name for this meeting is taken by another file in the vault")]
    NoteNameTaken,
    #[error(
        "note {path} was edited after the last write; run `mc reprocess <id> --discard-edits` to overwrite it"
    )]
    NoteEdited { path: PathBuf },
    #[error("could not write note {path}: {source}")]
    WriteNote {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// How a call to `Pipeline::run` ended.
#[derive(Debug)]
pub enum RunOutcome {
    Finished(Box<Meeting>),
    /// Another run, in this process or another, holds the meeting. It picks up any new capture
    /// before it lets go.
    AlreadyRunning,
}

pub struct Pipeline {
    app_dirs: AppDirs,
    settings: Settings,
    store: JobStore,
}

impl Pipeline {
    #[must_use]
    pub fn new(app_dirs: AppDirs, settings: Settings, store: JobStore) -> Self {
        Self {
            app_dirs,
            settings,
            store,
        }
    }

    #[must_use]
    pub fn store(&self) -> &JobStore {
        &self.store
    }

    /// Records a TranscripTonic payload that is already saved at `payload_path`. A re-post of a
    /// meeting already in the store reuses that meeting and its note.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the write fails.
    pub fn record_transcriptonic(
        &self,
        payload_path: &Path,
        transcript: &Transcript,
    ) -> Result<MeetingId, PipelineError> {
        Ok(self.store.upsert_captured(
            InputKind::TranscriptonicPayload,
            payload_path,
            transcript.started_at,
            transcript.title.as_deref(),
        )?)
    }

    /// Runs every step after the meeting's last finished one. On failure the job is marked
    /// `Failed` with the cause, and a later run resumes at the same step. A capture of the same
    /// meeting during the run makes it start over on the new input.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the run.
    pub fn run(&self, id: MeetingId) -> Result<RunOutcome, PipelineError> {
        let _span = tracing::info_span!("pipeline.run", meeting.id = %id).entered();
        let Some(lease) = self.store.lease_run(id)? else {
            tracing::info!("another run holds this meeting");
            return Ok(RunOutcome::AlreadyRunning);
        };
        let mut lease = LeaseGuard {
            store: &self.store,
            lease: Some(lease),
        };
        self.run_leased(&mut lease)
    }

    /// Runs every step again from the stored input. The note keeps its path. Without
    /// `discard_note_edits`, a note the user edited is left alone and the run fails.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the run.
    pub fn reprocess(
        &self,
        id: MeetingId,
        discard_note_edits: bool,
    ) -> Result<RunOutcome, PipelineError> {
        self.store.rewind(id, discard_note_edits)?;
        self.run(id)
    }

    fn run_leased(&self, lease: &mut LeaseGuard<'_>) -> Result<RunOutcome, PipelineError> {
        let id = lease.held().meeting_id();
        loop {
            let meeting = self.store.get(id)?;
            let revision = meeting.input_revision;
            let release = match self.run_steps(lease.held(), &meeting) {
                Ok(()) => self.store.release_run(lease.held(), revision)?,
                Err(PipelineError::Store(StoreError::Superseded(_))) => continue,
                Err(error @ PipelineError::Store(StoreError::LeaseLost(_))) => return Err(error),
                Err(error) => {
                    tracing::error!(exception.message = %error, "pipeline run failed");
                    match self
                        .store
                        .fail_run(lease.held(), revision, &error.to_string())
                    {
                        Ok(LeaseRelease::Released) => lease.released(),
                        Ok(LeaseRelease::Superseded) => continue,
                        Err(store_error) => tracing::error!(
                            exception.message = %store_error,
                            "could not record the failure"
                        ),
                    }
                    return Err(error);
                }
            };
            match release {
                LeaseRelease::Released => {
                    lease.released();
                    return Ok(RunOutcome::Finished(Box::new(self.store.get(id)?)));
                }
                LeaseRelease::Superseded => {}
            }
        }
    }

    fn run_steps(&self, lease: &RunLease, meeting: &Meeting) -> Result<(), PipelineError> {
        let id = meeting.id;
        let revision = meeting.input_revision;
        let transcript = load_transcript(meeting)?;

        let calendar_match = if meeting.finished_step < Step::Matched {
            let _span = tracing::info_span!("pipeline.match", meeting.id = %id).entered();
            let calendar_match = calendar::match_event(&transcript);
            let title = calendar_match
                .event
                .as_ref()
                .map(|event| event.title.as_str())
                .or(transcript.title.as_deref());
            self.store.record_match(
                lease,
                revision,
                title,
                &serde_json::to_string(&calendar_match)?,
            )?;
            tracing::info!(
                is_matched = calendar_match.event.is_some(),
                "calendar match done"
            );
            calendar_match
        } else {
            let stored = meeting
                .calendar_match_json
                .as_deref()
                .ok_or(PipelineError::MissingCalendarMatch(id))?;
            serde_json::from_str(stored)?
        };

        let is_extracted = if meeting.finished_step < Step::Extracted {
            let _span = tracing::info_span!("pipeline.extract", meeting.id = %id).entered();
            // LLM extraction arrives in build step 5. The note goes out tagged
            // status/needs-extraction meanwhile.
            self.store.record_extraction(lease, revision, None)?;
            tracing::info!(is_extracted = false, "extraction skipped");
            false
        } else {
            meeting.extraction_json.is_some()
        };

        if meeting.finished_step < Step::Written {
            let _span = tracing::info_span!("pipeline.write", meeting.id = %id).entered();
            let note = NoteInput {
                meeting_id: id,
                transcript: &transcript,
                calendar_match: &calendar_match,
                is_extracted,
                my_name: self.settings.my_name.as_deref(),
            };
            let contents = note::render(&note, &self.app_dirs.note_template_file())?;
            let note_sha256 = note::note_sha256(contents.as_bytes());
            let note_path = match &meeting.note_path {
                Some(note_path) => note_path.clone(),
                None => self.claim_note_path(lease, revision, &note)?,
            };
            // Recorded first, so after a crash or a re-post mid-write the next run still knows
            // the note on disk is its own.
            self.store
                .record_pending_note(lease, revision, &note_sha256)?;
            note::write_atomically(
                &self.app_dirs.note_staging_dir(),
                &note_path,
                &contents,
                || check_note_unedited(&note_path, meeting),
            )?;
            self.store.mark_written(lease, revision, &note_sha256)?;
            tracing::info!(file.path = %note_path.display(), "note written");
        }
        Ok(())
    }

    // Claimed before the write, so a crash between the write and mark_written rewrites this note
    // instead of adding a second one. A path is skipped when a file is there, which covers notes
    // that no job owns, such as the Attendee bot's.
    fn claim_note_path(
        &self,
        lease: &RunLease,
        revision: i64,
        note: &NoteInput<'_>,
    ) -> Result<PathBuf, PipelineError> {
        for note_path in note::note_path_candidates(&self.settings.meetings_dir(), note) {
            if note_path.exists() {
                continue;
            }
            match self.store.claim_note_path(lease, revision, &note_path)? {
                NotePathClaim::Claimed => return Ok(note_path),
                NotePathClaim::TakenByOtherMeeting => {}
            }
        }
        Err(PipelineError::NoteNameTaken)
    }
}

// Gives the lease back however the run ends, a panic included, so a tray thread that dies does
// not hold the meeting until the lease expires.
struct LeaseGuard<'a> {
    store: &'a JobStore,
    lease: Option<RunLease>,
}

impl LeaseGuard<'_> {
    fn held(&self) -> &RunLease {
        self.lease
            .as_ref()
            .expect("a released lease is never used again")
    }

    fn released(&mut self) {
        self.lease = None;
    }
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take()
            && let Err(error) = self.store.abandon_run(&lease)
        {
            tracing::error!(exception.message = %error, "could not give back the run lease");
        }
    }
}

// The vault promises that a rerun never discards the user's edits. A note whose contents match
// a write the pipeline made, or started, is still the pipeline's own. Hashes, unlike file times,
// survive Syncthing and git rewriting the time.
fn check_note_unedited(note_path: &Path, meeting: &Meeting) -> Result<(), PipelineError> {
    let known: Vec<&str> = meeting
        .note_sha256
        .iter()
        .chain(&meeting.pending_note_sha256s)
        .map(String::as_str)
        .collect();
    if known.is_empty() {
        return Ok(());
    }
    match std::fs::read(note_path) {
        Ok(on_disk) if known.contains(&note::note_sha256(&on_disk).as_str()) => Ok(()),
        Ok(_) => Err(PipelineError::NoteEdited {
            path: note_path.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PipelineError::ReadFile {
            path: note_path.to_path_buf(),
            source,
        }),
    }
}

fn load_transcript(meeting: &Meeting) -> Result<Transcript, PipelineError> {
    let input = std::fs::read(&meeting.input_path).map_err(|source| PipelineError::ReadFile {
        path: meeting.input_path.clone(),
        source,
    })?;
    match meeting.input_kind {
        InputKind::TranscriptonicPayload => Ok(sources::transcriptonic::parse(&input)?),
    }
}
