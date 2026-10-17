//! Calendar match, LLM extraction and the vault write. No Tauri dependency.

pub mod calendar;
pub mod extraction;
pub mod google;
mod intake;
pub mod llm;
mod render;
pub mod settings;
mod vault;

pub use vault::has_user_edits;

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use jiff::Timestamp;
use jobs::{
    Extraction, ExtractionFailure, JobStore, LeasedRun, MeetingId, RunOutcome, Step, StepError,
    StoreError,
};
use sources::Transcript;

use crate::calendar::{CalendarMatch, GoogleAccounts};
use crate::extraction::{CheckedNotes, Prompt};
use crate::google::GoogleError;
use crate::intake::read_transcript;
use crate::llm::{ExtractionError, Provider};
use crate::render::NoteInput;
use crate::settings::{AppDirs, LlmSettings, Settings};
use crate::vault::Vault;

pub use crate::intake::{InputError, Intake, IntakeError};

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
    Input(#[from] InputError),
    #[error("stored step output is not valid JSON: {0}")]
    StepOutput(#[from] serde_json::Error),
    #[error("calendar match failed: {0}")]
    Calendar(#[from] GoogleError),
    #[error("meeting {0} finished the calendar match but has no stored match")]
    MissingCalendarMatch(MeetingId),
    #[error("note template failed: {0}")]
    Template(#[from] minijinja::Error),
    #[error("could not write note frontmatter: {0}")]
    Frontmatter(String),
    #[error("every note name for this meeting is taken by another file in the vault")]
    NoteNameTaken,
    #[error(
        "note {path} cannot take the new notes: {reason}. Undo that edit, or overwrite the note with \"Overwrite my edits\" in the tray or `mc reprocess <id> --discard-edits`"
    )]
    NoteEdited { path: PathBuf, reason: String },
    #[error("the pipeline crashed: {0}")]
    Panicked(String),
    #[error("could not write note {path}: {source}")]
    WriteNote {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl StepError for PipelineError {
    fn panicked(message: String) -> Self {
        PipelineError::Panicked(message)
    }
}

// After each failed extraction, the next try waits this long. A failure after the last one fails
// the meeting. Spaced to outlast a rate-limit window or a laptop that went offline.
/// How many times a failed extraction is tried again before the meeting fails.
pub const EXTRACTION_RETRIES: u32 = 3;
const EXTRACTION_RETRY_DELAYS: [Duration; EXTRACTION_RETRIES as usize] = [
    Duration::from_mins(5),
    Duration::from_mins(30),
    Duration::from_hours(2),
];

pub struct Pipeline {
    app_dirs: AppDirs,
    settings: Settings,
    store: JobStore,
    google: GoogleAccounts,
    /// Replaced from the settings window, so a new provider applies to the next run.
    llm: RwLock<LlmSettings>,
    vault: Vault,
}

impl Pipeline {
    #[must_use]
    pub fn new(app_dirs: AppDirs, settings: Settings, store: JobStore) -> Self {
        let google = GoogleAccounts::new(&settings.google);
        let llm = RwLock::new(settings.llm.clone());
        let vault = Vault::new(settings.meetings_dir(), app_dirs.note_staging_dir());
        Self {
            app_dirs,
            settings,
            store,
            google,
            llm,
            vault,
        }
    }

    /// The LLM settings the next run uses.
    #[must_use]
    pub fn llm(&self) -> LlmSettings {
        self.llm
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Applies new LLM settings to the next run. When a provider is set, notes written without
    /// notes are queued for extraction, and the count is returned.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the meetings cannot be queued.
    pub fn replace_llm(&self, llm: LlmSettings) -> Result<usize, PipelineError> {
        let has_provider = llm.provider.is_some();
        *self.llm.write().unwrap_or_else(PoisonError::into_inner) = llm;
        if !has_provider {
            return Ok(0);
        }
        let queued = self.store.schedule_missing_extractions()?;
        tracing::info!(meeting.count = queued, "LLM settings applied");
        Ok(queued)
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The signed-in Google accounts. Changes from the settings window apply here at once.
    #[must_use]
    pub fn google(&self) -> &GoogleAccounts {
        &self.google
    }

    #[must_use]
    pub fn store(&self) -> &JobStore {
        &self.store
    }

    /// The extraction prompt for a meeting, from its stored input and calendar match.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError` when the meeting, its input or its stored match cannot be read.
    pub fn extraction_prompt(&self, id: MeetingId) -> Result<Prompt, PipelineError> {
        let meeting = self.store.get(id)?;
        let transcript = read_transcript(meeting.input_kind, &meeting.input_path)?;
        let calendar_match: CalendarMatch = match meeting.calendar_match_json.as_deref() {
            Some(stored) => serde_json::from_str(stored)?,
            None => CalendarMatch { event: None },
        };
        Ok(self.prompt(&transcript, &calendar_match))
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
        self.store.run(id, |run| self.run_steps(run))
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

    fn run_steps(&self, run: &LeasedRun<'_>) -> Result<(), PipelineError> {
        let meeting = run.meeting();
        let id = meeting.id;
        let transcript = read_transcript(meeting.input_kind, &meeting.input_path)?;

        let calendar_match = if meeting.finished_step < Step::Matched {
            let _span = tracing::info_span!("pipeline.match", meeting.id = %id).entered();
            let calendar_match = self.google.match_event(&transcript)?;
            let title = calendar_match
                .event
                .as_ref()
                .and_then(|event| event.title.as_deref())
                .or(transcript.title.as_deref());
            run.record_match(title, &serde_json::to_string(&calendar_match)?)?;
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

        // A written meeting is extracted again when a retry is due, or on a manual retry after
        // extraction failed for good.
        let needs_extraction = meeting.finished_step < Step::Extracted
            || meeting.next_extraction_at.is_some()
            || (meeting.failure.is_some() && meeting.extraction_error.is_some());
        let extraction = if needs_extraction {
            self.extract_step(run, &transcript, &calendar_match)?
        } else {
            match meeting.extraction_json.as_deref() {
                Some(stored) => ExtractOutcome::Notes {
                    notes: serde_json::from_str(stored)?,
                    provider: meeting.extraction_provider.clone().unwrap_or_default(),
                },
                None => ExtractOutcome::NoNotes,
            }
        };
        let (notes, extraction_provider) = match &extraction {
            ExtractOutcome::Notes { notes, provider } => (Some(notes), Some(provider.as_str())),
            ExtractOutcome::NoNotes | ExtractOutcome::Failed { .. } => (None, None),
        };

        if meeting.finished_step < Step::Written || needs_extraction {
            let _span = tracing::info_span!("pipeline.write", meeting.id = %id).entered();
            let note = NoteInput {
                meeting_id: id,
                transcript: &transcript,
                calendar_match: &calendar_match,
                notes,
                extraction_provider,
                my_name: self.settings.my_name.as_deref(),
                time_zone: &self.settings.time_zone,
            };
            let rendered = render::note(&note, &self.app_dirs.note_template_file())?;
            let note_path = self
                .vault
                .write(run, &note.started_at(), note.title(), &rendered)?;
            tracing::info!(file.path = %note_path.display(), "note written");
        }

        if let ExtractOutcome::Failed { provider, error } = &extraction {
            schedule_extraction_retry(run, provider, error)?;
        }
        Ok(())
    }

    // Runs the LLM and records the notes. A failure is returned, not recorded: the note is
    // written first, so the transcript reaches the vault either way.
    fn extract_step(
        &self,
        run: &LeasedRun<'_>,
        transcript: &Transcript,
        calendar_match: &CalendarMatch,
    ) -> Result<ExtractOutcome, PipelineError> {
        let _span =
            tracing::info_span!("pipeline.extract", meeting.id = %run.meeting().id).entered();
        let prompt = self.prompt(transcript, calendar_match);
        match self.extract(&prompt) {
            Ok(Some((notes, provider))) => {
                let notes_json = serde_json::to_string(&notes)?;
                run.record_extraction(Some(Extraction {
                    notes_json: &notes_json,
                    provider: &provider,
                }))?;
                tracing::info!(gen_ai.request.model = %provider, "extraction done");
                Ok(ExtractOutcome::Notes { notes, provider })
            }
            Ok(None) => {
                run.record_extraction(None)?;
                tracing::info!("no LLM provider is set; the note gets the transcript only");
                Ok(ExtractOutcome::NoNotes)
            }
            Err((provider, error)) => {
                tracing::warn!(exception.message = %error, "extraction failed");
                Ok(ExtractOutcome::Failed { provider, error })
            }
        }
    }

    fn prompt(&self, transcript: &Transcript, calendar_match: &CalendarMatch) -> Prompt {
        Prompt::new(
            transcript,
            self.settings.my_name.as_deref(),
            &render::attendees(transcript, calendar_match),
            &self.llm().glossary,
        )
    }

    /// The checked notes and the `<provider>/<model>` that wrote them, or `None` when no
    /// provider is set. The provider is built per run, so a new API key applies at once.
    fn extract(
        &self,
        prompt: &Prompt,
    ) -> Result<Option<(CheckedNotes, String)>, (String, ExtractionError)> {
        let llm = self.llm();
        let Some(label) = llm.label() else {
            return Ok(None);
        };
        let provider = Provider::from_settings(&llm, &self.app_dirs)
            .map_err(|error| (label.clone(), error))?
            .expect("a provider is set, since the settings give a label");
        let notes = provider
            .extract(prompt)
            .map_err(|error| (label.clone(), error))?;
        Ok(Some((notes, label)))
    }
}

enum ExtractOutcome {
    Notes {
        notes: CheckedNotes,
        provider: String,
    },
    /// No provider is set, or a stored run had none.
    NoNotes,
    Failed {
        provider: String,
        error: ExtractionError,
    },
}

fn schedule_extraction_retry(
    run: &LeasedRun<'_>,
    provider: &str,
    error: &ExtractionError,
) -> Result<(), PipelineError> {
    let meeting = run.meeting();
    // A manual retry after the last failure starts the count again.
    let attempts = match meeting.failure {
        Some(_) => 1,
        None => meeting.extraction_attempts + 1,
    };
    // A missing key or CLI, a refused sign-in or a reply over the output cap fails the
    // same way every time, so it fails the meeting at once and the tray says why.
    let retry_at = if error.is_lasting() {
        None
    } else {
        extraction_retry_delay(attempts).map(|delay| Timestamp::now() + delay)
    };
    run.record_extraction_failure(ExtractionFailure {
        provider,
        error: &error.to_string(),
        attempts,
        retry_at,
    })?;
    Ok(())
}

// `MEETING_CAPTURE_RETRY_DELAY_S` replaces every delay, so a dev run sees the whole schedule in
// minutes.
fn extraction_retry_delay(attempts: u32) -> Option<Duration> {
    let index = usize::try_from(attempts).ok()?.checked_sub(1)?;
    let delay = *EXTRACTION_RETRY_DELAYS.get(index)?;
    let dev_delay = std::env::var("MEETING_CAPTURE_RETRY_DELAY_S")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs);
    Some(dev_delay.unwrap_or(delay))
}
