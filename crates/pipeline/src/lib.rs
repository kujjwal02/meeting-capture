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

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use call_folder::media::{CallMedia, MediaLink, MediaTarget};
use jobs::{
    ErrorPersistence, Extraction, ExtractionAtWrite, ExtractionFailure, InputKind, JobState,
    JobStore, LeasedRun, Meeting, MeetingId, RunOutcome, Step, StepError, StoreError,
};
use sources::Transcript;

use crate::calendar::CalendarMatch;
use crate::extraction::{CheckedNotes, Prompt};
use crate::google::GoogleError;
use crate::intake::read_transcript;
use crate::llm::{ExtractionError, Provider};
use crate::render::{CallRecording, NoteInput};
use crate::settings::{AppDirs, LlmSettings, Settings, SettingsError, SettingsStore};
use crate::vault::{SectionWrite, Vault};

pub use crate::intake::{InputError, Intake, IntakeError, MergeError};

/// One decision event for the transcript the run reads: its source and what it holds. Counts
/// only: no titles, names or text.
fn log_source_decision(transcript: &Transcript) {
    tracing::info!(
        mc.decision.kind = "source_pick",
        mc.decision.chosen = serde_name(&transcript.source),
        mc.transcript.speaker_naming = serde_name(&transcript.speaker_naming),
        mc.transcript.segment_count = transcript.segments.len(),
        mc.transcript.chat_count = transcript.chat_messages.len(),
        mc.transcript.attendee_count = transcript.attendees.len(),
        mc.transcript.has_meeting_url = transcript.meeting_url.is_some(),
        "transcript source picked"
    );
}

// The name serde gives a closed enum, which is the name the stored files use.
fn serde_name(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(other) => other.to_string(),
        Err(error) => error.to_string(),
    }
}

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
    #[error("the note template's mc:begin/mc:end markers do not line up: {0}")]
    TemplateMarkers(String),
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
        let outcome = self.store.run(id, |run| {
            self.run_steps(run, WriteTrigger::StepsOwed).map(drop)
        });
        if outcome.is_err() {
            // The recorder holds the steps' DEBUG lines, which the log file does not.
            let folder = self.app_dirs.call_debug_dir().join(format!("meeting-{id}"));
            if let Err(error) =
                telemetry::dump_recorder_into(&folder, telemetry::DumpReason::JobFailed)
            {
                tracing::warn!(exception.message = %error, "could not dump the flight recorder");
            }
        }
        outcome
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

    /// Rewrites the Recording section of each finished note whose call folder, among
    /// `call_folders`, changed since the note was written: a new `call.mp4`, a purge, or a note
    /// written before the section existed. Uses the stored notes, so it never calls the LLM. A
    /// meeting that still owes a step, or failed, is left to its own run.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the meetings cannot be listed.
    pub fn refresh_recordings(
        &self,
        call_folders: &[PathBuf],
    ) -> Result<RecordingRefresh, PipelineError> {
        let _span = tracing::info_span!("pipeline.refresh_recordings").entered();
        let mut refresh = RecordingRefresh::default();
        let folder_names: Vec<&str> = call_folders
            .iter()
            .filter_map(|folder| folder.file_name()?.to_str())
            .collect();
        for meeting in self.store.written()? {
            let Some(folder_name) = call_folder_name(&meeting) else {
                continue;
            };
            if !folder_names.contains(&folder_name.as_str()) || !is_settled(&meeting) {
                continue;
            }
            let outcome = Cell::new(RecordingWrite::Unchanged);
            // Unrecorded: a rewrite that fails leaves the finished meeting as it was.
            let run = self.store.run_unrecorded(meeting.id, |run| {
                match self.run_steps(run, WriteTrigger::RecordingChanged) {
                    Ok(write) => {
                        outcome.set(write);
                        Ok(())
                    }
                    // The user edited the Recording section itself. Their edit wins.
                    Err(PipelineError::NoteEdited { reason, .. }) => {
                        tracing::warn!(
                            meeting.id = %meeting.id,
                            exception.message = reason,
                            "recording section not refreshed"
                        );
                        outcome.set(RecordingWrite::KeptUserEdits);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            });
            match run {
                Ok(_) => match outcome.get() {
                    RecordingWrite::Unchanged => {}
                    RecordingWrite::Written => refresh.written_count += 1,
                    RecordingWrite::KeptUserEdits => refresh.kept_edits_count += 1,
                    // A capture, a rewind or a merge arrived since the listing. Its run
                    // would have been refused while the refresh held the lease, so run it
                    // here; the run renders the Recording section too.
                    RecordingWrite::StepsOwed => {
                        if let Err(error) = self.run(meeting.id) {
                            tracing::error!(
                                meeting.id = %meeting.id,
                                exception.message = %error,
                                "run after refresh failed"
                            );
                        }
                    }
                },
                Err(error) => {
                    tracing::error!(
                        meeting.id = %meeting.id,
                        exception.message = %error,
                        "recording section refresh failed"
                    );
                    refresh.failed_count += 1;
                }
            }
        }
        tracing::info!(
            note.written_count = refresh.written_count,
            note.kept_edits_count = refresh.kept_edits_count,
            note.failed_count = refresh.failed_count,
            "recording sections refreshed"
        );
        Ok(refresh)
    }

    // The meeting's call folder on this machine, with what it holds. `None` when the source has
    // no call folder or the folder is not here.
    fn call_recording(&self, meeting: &Meeting) -> Option<CallRecording> {
        let folder_name = call_folder_name(meeting)?;
        let folder_link = match MediaLink::new(&folder_name, MediaTarget::Folder) {
            Ok(folder_link) => folder_link,
            Err(error) => {
                tracing::warn!(
                    meeting.id = %meeting.id,
                    exception.message = %error,
                    "call folder cannot be linked"
                );
                return None;
            }
        };
        let folder = folder_link.folder_in(&self.app_dirs.calls_dir());
        match CallMedia::scan(&folder) {
            Ok(media) => Some(CallRecording { folder_link, media }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(
                    file.directory = %folder.display(),
                    exception.message = %error,
                    "call folder unreadable; the note gets no recording links"
                );
                None
            }
        }
    }

    fn run_steps(
        &self,
        run: &LeasedRun<'_>,
        trigger: WriteTrigger,
    ) -> Result<RecordingWrite, PipelineError> {
        let meeting = run.meeting();
        let id = meeting.id;
        // Read again under the lease: a capture or a failure since the listing makes the
        // meeting's own run the one to write.
        if trigger == WriteTrigger::RecordingChanged && !is_settled(meeting) {
            return Ok(RecordingWrite::StepsOwed);
        }
        let transcript = read_transcript(meeting.input_kind, &meeting.input_path)?;
        log_source_decision(&transcript);

        let calendar_match = self.match_step(run, &transcript)?;

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

        let is_write_owed = meeting.finished_step < Step::Written || needs_extraction;
        if is_write_owed || trigger == WriteTrigger::RecordingChanged {
            let _span = tracing::info_span!(
                "pipeline.write",
                meeting.id = %id,
                job.step = Step::Written.as_str()
            )
            .entered();
            let recording = self.call_recording(meeting);
            let note = NoteInput {
                meeting_id: id,
                transcript: &transcript,
                calendar_match: &calendar_match,
                notes,
                extraction_provider,
                my_name: self.settings.my_name.as_deref(),
                time_zone: &self.settings.time_zone,
                recording: recording.as_ref(),
            };
            let rendered = render::note(&note, &self.app_dirs.note_template_file())?;
            if !is_write_owed {
                match vault::section_write(&meeting.note, &rendered, RECORDING_SECTION)? {
                    SectionWrite::Needed => {}
                    SectionWrite::Current | SectionWrite::DeletedByUser | SectionWrite::NoNote => {
                        return Ok(RecordingWrite::Unchanged);
                    }
                }
                // Edits in other sections stay: only this section is replaced.
                if vault::has_user_edits(&meeting.note) {
                    self.vault
                        .write_section(run, &rendered, RECORDING_SECTION)?;
                    tracing::info!("recording section written into an edited note");
                    return Ok(RecordingWrite::Written);
                }
            }
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
            // `note.path`, not `file.path`: the file name is the meeting's title, which a
            // debug report without text shortens to its length.
            tracing::info!(note.path = %note_path.display(), "note written");
            return Ok(RecordingWrite::Written);
        }
        Ok(RecordingWrite::Unchanged)
    }

    // The calendar match, run now or read from the store.
    fn match_step(
        &self,
        run: &LeasedRun<'_>,
        transcript: &Transcript,
    ) -> Result<CalendarMatch, PipelineError> {
        let meeting = run.meeting();
        let id = meeting.id;
        if meeting.finished_step < Step::Matched {
            let _span = tracing::info_span!(
                "pipeline.match",
                meeting.id = %id,
                job.step = Step::Matched.as_str()
            )
            .entered();
            let calendar_match = self.settings_store.google().match_event(transcript)?;
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
            Ok(calendar_match)
        } else {
            let stored = meeting
                .calendar_match_json
                .as_deref()
                .ok_or(PipelineError::MissingCalendarMatch(id))?;
            Ok(serde_json::from_str(stored)?)
        }
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
        let _span = tracing::info_span!(
            "pipeline.extract",
            meeting.id = %run.meeting().id,
            job.step = Step::Extracted.as_str()
        )
        .entered();
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

/// The note section that links the call's media.
const RECORDING_SECTION: &str = "recording";

/// What `Pipeline::refresh_recordings` did.
#[derive(Debug, Default)]
pub struct RecordingRefresh {
    pub written_count: usize,
    /// Notes the user edited in a section the rewrite would change.
    pub kept_edits_count: usize,
    pub failed_count: usize,
}

// Why `run_steps` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteTrigger {
    /// The sweep, a capture or a retry: run each step still owed.
    StepsOwed,
    /// The call folder changed: rewrite the note if its Recording section is out of date.
    RecordingChanged,
}

#[derive(Debug, Clone, Copy)]
enum RecordingWrite {
    Unchanged,
    Written,
    KeptUserEdits,
    /// The meeting owes a step again; its own run writes the note.
    StepsOwed,
}

/// Written, not failed, and owing no extraction: nothing but a rewrite would touch the note.
fn is_settled(meeting: &Meeting) -> bool {
    matches!(meeting.state(), JobState::Written { .. }) && !meeting.is_extraction_owed()
}

/// The call folder a meeting's capture wrote, named after the meeting's input log. Sources
/// without a capture have none.
fn call_folder_name(meeting: &Meeting) -> Option<String> {
    match meeting.input_kind {
        InputKind::TranscriptonicPayload => None,
        InputKind::SlackHuddleLog | InputKind::BrowserCallLog => input_stem(&meeting.input_path),
    }
}

fn input_stem(input_path: &Path) -> Option<String> {
    Some(input_path.file_stem()?.to_str()?.to_owned())
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
