//! Calendar match, LLM extraction and the vault write. No Tauri dependency.

pub mod calendar;
pub mod extraction;
pub mod google;
mod intake;
pub mod llm;
mod render;
pub mod settings;
mod sweep;
mod vault;

pub use vault::has_user_edits;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use jobs::{
    ErrorPersistence, Extraction, ExtractionAtWrite, ExtractionFailure, JobStore, LeasedRun,
    MeetingId, RunOutcome, Step, StepError, StoreError,
};
use sources::Transcript;

use crate::calendar::CalendarMatch;
use crate::extraction::{CheckedNotes, Prompt};
use crate::google::GoogleError;
use crate::intake::read_transcript;
use crate::llm::{ExtractionError, Provider};
use crate::render::NoteInput;
use crate::settings::{AppDirs, LlmSettings, Settings, SettingsError, SettingsStore};
use crate::vault::Vault;

pub use crate::intake::{InputError, Intake, IntakeError, MergeError};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
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

pub struct Pipeline {
    app_dirs: AppDirs,
    settings: Settings,
    /// The LLM and Google settings, which the settings window changes while the app runs.
    settings_store: Arc<SettingsStore>,
    store: JobStore,
    vault: Vault,
    /// Set while a sweep runs, so a second sweep, from the timer or a settings save, skips
    /// instead of queueing.
    is_sweeping: AtomicBool,
    /// Set while a retry of failed meetings runs, so a second "Retry failed" click skips instead
    /// of running the same meetings again.
    is_retrying: AtomicBool,
}

impl Pipeline {
    /// Loads the settings into `settings_store`, then opens the job store and the vault. A new
    /// vault folder applies after a restart.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Settings` naming the file or the invalid field, or
    /// `PipelineError::Store` when the job store cannot be opened.
    pub fn open(settings_store: Arc<SettingsStore>) -> Result<Self, PipelineError> {
        let settings = settings_store.load()?;
        let app_dirs = settings_store.app_dirs().clone();
        let store = JobStore::open(&app_dirs.job_store_file())?;
        let vault = Vault::new(settings.meetings_dir(), app_dirs.note_staging_dir());
        Ok(Self {
            app_dirs,
            settings,
            settings_store,
            store,
            vault,
            is_sweeping: AtomicBool::new(false),
            is_retrying: AtomicBool::new(false),
        })
    }

    /// The LLM settings the next run uses.
    fn llm(&self) -> LlmSettings {
        self.settings_store.llm()
    }

    /// After an LLM settings edit: when a provider is set, notes written without notes are
    /// queued for extraction, and the count is returned.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the meetings cannot be queued.
    pub fn queue_missing_extractions(&self) -> Result<usize, PipelineError> {
        if self.llm().provider().is_none() {
            return Ok(0);
        }
        let queued = self.store.schedule_missing_extractions()?;
        tracing::info!(meeting.count = queued, "LLM settings applied");
        Ok(queued)
    }

    /// The settings that apply at launch.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
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
        Ok(self.prompt(&transcript, &calendar_match, &self.llm()))
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
            let calendar_match = self.settings_store.google().match_event(&transcript)?;
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

        let needs_extraction = meeting.is_extraction_owed();
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
            let extraction_at_write = match &extraction {
                ExtractOutcome::Notes { .. } | ExtractOutcome::NoNotes => {
                    ExtractionAtWrite::Recorded
                }
                ExtractOutcome::Failed {
                    provider,
                    error_message,
                    persistence,
                } => ExtractionAtWrite::Failed(ExtractionFailure {
                    provider,
                    error: error_message,
                    persistence: *persistence,
                }),
            };
            let note_path = self.vault.write(
                run,
                &note.started_at(),
                note.title(),
                &rendered,
                extraction_at_write,
            )?;
            tracing::info!(file.path = %note_path.display(), "note written");
        }
        Ok(())
    }

    // Runs the LLM and records the notes. A failure is returned, not recorded: the note write
    // records it in the same statement, so the transcript reaches the vault either way and a
    // crash cannot leave a written note with no owed extraction.
    fn extract_step(
        &self,
        run: &LeasedRun<'_>,
        transcript: &Transcript,
        calendar_match: &CalendarMatch,
    ) -> Result<ExtractOutcome, PipelineError> {
        let _span =
            tracing::info_span!("pipeline.extract", meeting.id = %run.meeting().id).entered();
        // One read, so the prompt and the provider come from the same settings edit.
        let llm = self.llm();
        let prompt = self.prompt(transcript, calendar_match, &llm);
        match self.extract(&llm, &prompt) {
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
                Ok(ExtractOutcome::Failed {
                    provider,
                    error_message: error.to_string(),
                    persistence: error.persistence(),
                })
            }
        }
    }

    fn prompt(
        &self,
        transcript: &Transcript,
        calendar_match: &CalendarMatch,
        llm: &LlmSettings,
    ) -> Prompt {
        Prompt::new(
            transcript,
            self.settings.my_name.as_deref(),
            &render::attendees(transcript, self.settings.my_name.as_deref())
                .into_iter()
                .chain(render::invited(calendar_match))
                .collect::<Vec<_>>(),
            &llm.glossary,
        )
    }

    /// The checked notes and the `<provider>/<model>` that wrote them, or `None` when no
    /// provider is set. The provider is built per run, so a new API key applies at once.
    fn extract(
        &self,
        llm: &LlmSettings,
        prompt: &Prompt,
    ) -> Result<Option<(CheckedNotes, String)>, (String, ExtractionError)> {
        let Some(label) = llm.label() else {
            return Ok(None);
        };
        let provider = Provider::from_settings(llm, &self.app_dirs)
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
        /// The error's text, as the job store records it.
        error_message: String,
        persistence: ErrorPersistence,
    },
}
