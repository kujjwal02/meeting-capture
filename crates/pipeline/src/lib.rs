//! Calendar match, LLM extraction and the vault write. No Tauri dependency.

pub mod calendar;
pub mod extraction;
pub mod google;
pub mod llm;
mod note;
pub mod settings;
mod splice;

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jobs::{
    Extraction, ExtractionFailure, InputKind, JobStore, LeaseRelease, Meeting, MeetingId,
    NotePathClaim, RunLease, Step, StoreError,
};
use sources::Transcript;

use crate::calendar::{CalendarMatch, GoogleAccounts};
use crate::extraction::{CheckedNotes, Prompt};
use crate::google::GoogleError;
use crate::llm::{ExtractionError, Provider};
use crate::note::NoteInput;
use crate::settings::{AppDirs, LlmSettings, Settings};

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
    #[error(transparent)]
    ParseSlackHuddle(#[from] sources::slack_huddle::ParseError),
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

/// How a call to `Pipeline::run` ended.
#[derive(Debug)]
pub enum RunOutcome {
    Finished(Box<Meeting>),
    /// Another run, in this process or another, holds the meeting. It picks up any new capture
    /// before it lets go.
    AlreadyRunning,
}

// Longer than any step should take, LLM call included.
const MAX_RUN_TIME: Duration = Duration::from_mins(15);

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
}

impl Pipeline {
    #[must_use]
    pub fn new(app_dirs: AppDirs, settings: Settings, store: JobStore) -> Self {
        let google = GoogleAccounts::new(&settings.google);
        let llm = RwLock::new(settings.llm.clone());
        Self {
            app_dirs,
            settings,
            store,
            google,
            llm,
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

    /// Whether the meeting's note differs from every write the pipeline made, so a rerun has to
    /// splice into it or overwrite it.
    #[must_use]
    pub fn has_edited_note(&self, meeting: &Meeting) -> bool {
        let known = known_note_sha256s(meeting);
        let Some(note_path) = &meeting.note_path else {
            return false;
        };
        !known.is_empty()
            && std::fs::read(note_path)
                .is_ok_and(|on_disk| !known.contains(&note::note_sha256(&on_disk).as_str()))
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
        let transcript = load_transcript(&meeting)?;
        let calendar_match: CalendarMatch = match meeting.calendar_match_json.as_deref() {
            Some(stored) => serde_json::from_str(stored)?,
            None => CalendarMatch { event: None },
        };
        Ok(self.prompt(&transcript, &calendar_match))
    }

    /// Records a meeting whose input of `kind` is already saved at `input_path`. Recording a
    /// meeting already in the store, such as a re-posted payload, reuses that meeting and its note.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the write fails.
    pub fn record(
        &self,
        kind: InputKind,
        input_path: &Path,
        transcript: &Transcript,
    ) -> Result<MeetingId, PipelineError> {
        Ok(self.store.upsert_captured(
            kind,
            input_path,
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
        let (stop_renewing, is_run_over) = std::sync::mpsc::channel::<()>();
        let renewed_lease = lease.clone();
        let mut lease = LeaseGuard {
            store: &self.store,
            lease: Some(lease),
        };
        std::thread::scope(|scope| {
            scope.spawn(move || self.keep_lease_alive(&renewed_lease, &is_run_over));
            let outcome = self.run_leased(&mut lease);
            drop(stop_renewing);
            outcome
        })
    }

    // A step such as the LLM call can outlast the lease. Renewing from a second thread keeps the
    // lease short, so a crashed process frees its meetings within `jobs::LEASE_EXPIRY`. Renewal
    // stops after `MAX_RUN_TIME`, so a hung run frees its meeting too; the note write checks the
    // lease, so the hung run cannot overwrite a later run's note.
    fn keep_lease_alive(&self, lease: &RunLease, is_run_over: &std::sync::mpsc::Receiver<()>) {
        let _span =
            tracing::info_span!("pipeline.lease", meeting.id = %lease.meeting_id()).entered();
        let renew_until = Instant::now() + MAX_RUN_TIME;
        while let Err(RecvTimeoutError::Timeout) = is_run_over.recv_timeout(jobs::LEASE_EXPIRY / 4)
        {
            if Instant::now() > renew_until {
                tracing::error!("run is still going after 15 minutes; letting the lease expire");
                return;
            }
            match self.store.renew_lease(lease) {
                Ok(()) => {}
                // The run gave the lease back between two checks.
                Err(StoreError::LeaseLost(_)) => return,
                Err(error) => {
                    tracing::warn!(exception.message = %error, "could not renew the run lease");
                }
            }
        }
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
            // A panic in a step becomes a recorded failure, so the sweep does not rerun the
            // meeting forever and the tray shows the cause.
            let steps = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.run_steps(lease.held(), &meeting)
            }))
            .unwrap_or_else(|panic| Err(PipelineError::Panicked(panic_message(panic.as_ref()))));
            let release = match steps {
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
            let calendar_match = self.google.match_event(&transcript)?;
            let title = calendar_match
                .event
                .as_ref()
                .and_then(|event| event.title.as_deref())
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

        // A written meeting is extracted again when a retry is due, or on a manual retry after
        // extraction failed for good.
        let needs_extraction = meeting.finished_step < Step::Extracted
            || meeting.next_extraction_at.is_some()
            || (meeting.failure.is_some() && meeting.extraction_error.is_some());
        let extraction = if needs_extraction {
            self.extract_step(lease, revision, &transcript, &calendar_match)?
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
            let rendered = note::render(&note, &self.app_dirs.note_template_file())?;
            let note_path = match &meeting.note_path {
                Some(note_path) => note_path.clone(),
                None => self.claim_note_path(lease, revision, &note)?,
            };
            let rendered_section_sha256s =
                splice::section_sha256s(&rendered).map_err(|reason| PipelineError::NoteEdited {
                    path: note_path.clone(),
                    reason: format!("in the template, {reason}"),
                })?;
            let FittedNote {
                contents,
                edited_sha256,
                section_sha256s,
            } = fit_to_note(&note_path, meeting, rendered, rendered_section_sha256s)?;
            let note_sha256 = note::note_sha256(contents.as_bytes());
            // Recorded first, so after a crash or a re-post mid-write the next run still knows
            // the note on disk is its own.
            self.store
                .record_pending_note(lease, revision, &note_sha256)?;
            note::write_atomically(
                &self.app_dirs.note_staging_dir(),
                &note_path,
                &contents,
                || {
                    self.store.renew_lease(lease)?;
                    check_note_unchanged(&note_path, meeting, edited_sha256.as_deref())
                },
            )?;
            self.store
                .mark_written(lease, revision, &note_sha256, &section_sha256s)?;
            tracing::info!(file.path = %note_path.display(), "note written");
        }

        if let ExtractOutcome::Failed { provider, error } = &extraction {
            self.schedule_extraction_retry(lease, meeting, provider, error)?;
        }
        Ok(())
    }

    // Runs the LLM and records the notes. A failure is returned, not recorded: the note is
    // written first, so the transcript reaches the vault either way.
    fn extract_step(
        &self,
        lease: &RunLease,
        revision: i64,
        transcript: &Transcript,
        calendar_match: &CalendarMatch,
    ) -> Result<ExtractOutcome, PipelineError> {
        let _span =
            tracing::info_span!("pipeline.extract", meeting.id = %lease.meeting_id()).entered();
        let prompt = self.prompt(transcript, calendar_match);
        match self.extract(&prompt) {
            Ok(Some((notes, provider))) => {
                let notes_json = serde_json::to_string(&notes)?;
                self.store.record_extraction(
                    lease,
                    revision,
                    Some(Extraction {
                        notes_json: &notes_json,
                        provider: &provider,
                    }),
                )?;
                tracing::info!(gen_ai.request.model = %provider, "extraction done");
                Ok(ExtractOutcome::Notes { notes, provider })
            }
            Ok(None) => {
                self.store.record_extraction(lease, revision, None)?;
                tracing::info!("no LLM provider is set; the note gets the transcript only");
                Ok(ExtractOutcome::NoNotes)
            }
            Err((provider, error)) => {
                tracing::warn!(exception.message = %error, "extraction failed");
                Ok(ExtractOutcome::Failed { provider, error })
            }
        }
    }

    fn schedule_extraction_retry(
        &self,
        lease: &RunLease,
        meeting: &Meeting,
        provider: &str,
        error: &ExtractionError,
    ) -> Result<(), PipelineError> {
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
        self.store.record_extraction_failure(
            lease,
            meeting.input_revision,
            ExtractionFailure {
                provider,
                error: &error.to_string(),
                attempts,
                retry_at,
            },
        )?;
        Ok(())
    }

    fn prompt(&self, transcript: &Transcript, calendar_match: &CalendarMatch) -> Prompt {
        Prompt::new(
            transcript,
            self.settings.my_name.as_deref(),
            &note::attendees(transcript, calendar_match),
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
fn known_note_sha256s(meeting: &Meeting) -> Vec<&str> {
    meeting
        .note_sha256
        .iter()
        .chain(&meeting.pending_note_sha256s)
        .map(String::as_str)
        .collect()
}

struct FittedNote {
    contents: String,
    /// The hash of the edited note the splice started from, which the write checks again just
    /// before the rename.
    edited_sha256: Option<String>,
    /// Stored after the write, so the next rerun can tell the app's sections from the user's.
    section_sha256s: BTreeMap<String, String>,
}

// The contents to write: the render spliced into the note on disk, so text the user added outside
// the sections survives every rerun, not only the first.
fn fit_to_note(
    note_path: &Path,
    meeting: &Meeting,
    rendered: String,
    rendered_section_sha256s: BTreeMap<String, String>,
) -> Result<FittedNote, PipelineError> {
    let as_rendered = |rendered: String, section_sha256s: BTreeMap<String, String>| FittedNote {
        contents: rendered,
        edited_sha256: None,
        section_sha256s,
    };
    let known = known_note_sha256s(meeting);
    // No hash: a new note, or edits the user chose to discard.
    if known.is_empty() {
        return Ok(as_rendered(rendered, rendered_section_sha256s));
    }
    let on_disk = match std::fs::read_to_string(note_path) {
        Ok(on_disk) => on_disk,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(as_rendered(rendered, rendered_section_sha256s));
        }
        Err(source) => {
            return Err(PipelineError::ReadFile {
                path: note_path.to_path_buf(),
                source,
            });
        }
    };
    let on_disk_sha256 = note::note_sha256(on_disk.as_bytes());
    let is_as_written = known.contains(&on_disk_sha256.as_str());
    let note_edited = |reason: String| PipelineError::NoteEdited {
        path: note_path.to_path_buf(),
        reason,
    };
    // A custom template without markers leaves nothing to splice into.
    if rendered_section_sha256s.is_empty() {
        return if is_as_written {
            Ok(as_rendered(rendered, rendered_section_sha256s))
        } else {
            Err(note_edited(
                "the note template has no mc:begin/mc:end markers, so the app cannot keep your edits"
                    .to_owned(),
            ))
        };
    }
    let mut written = meeting.note_section_sha256s.clone();
    if is_as_written {
        let on_disk_section_sha256s = splice::section_sha256s(&on_disk)
            .map_err(|reason| note_edited(format!("in the note, {reason}")))?;
        // Written from a template without markers: every section would land after the old text.
        if on_disk_section_sha256s.is_empty() {
            return Ok(as_rendered(rendered, rendered_section_sha256s));
        }
        // A note the pipeline wrote holds its sections as written, which covers a write that
        // crashed before the store recorded its section hashes.
        written.extend(on_disk_section_sha256s);
    }
    let spliced = splice::splice(&on_disk, &rendered, &written).map_err(note_edited)?;
    let mut section_sha256s = splice::section_sha256s(&spliced)
        .map_err(|reason| note_edited(format!("in the spliced note, {reason}")))?;
    // A section the user deleted keeps its hash, so later reruns leave it out too.
    for (name, sha256) in written {
        if rendered_section_sha256s.contains_key(&name) {
            section_sha256s.entry(name).or_insert(sha256);
        }
    }
    Ok(FittedNote {
        contents: spliced,
        edited_sha256: Some(on_disk_sha256),
        section_sha256s,
    })
}

// Run just before the rename: the note must still be one the pipeline wrote, or the edited note
// the splice started from.
fn check_note_unchanged(
    note_path: &Path,
    meeting: &Meeting,
    edited_sha256: Option<&str>,
) -> Result<(), PipelineError> {
    let mut expected = known_note_sha256s(meeting);
    expected.extend(edited_sha256);
    if expected.is_empty() {
        return Ok(());
    }
    match std::fs::read(note_path) {
        Ok(on_disk) if expected.contains(&note::note_sha256(&on_disk).as_str()) => Ok(()),
        Ok(_) => Err(PipelineError::NoteEdited {
            path: note_path.to_path_buf(),
            reason: "it changed while the run was writing; the next run will try again".to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PipelineError::ReadFile {
            path: note_path.to_path_buf(),
            source,
        }),
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(message), _) => (*message).to_owned(),
        (None, Some(message)) => message.clone(),
        (None, None) => "a panic with no message".to_owned(),
    }
}

fn load_transcript(meeting: &Meeting) -> Result<Transcript, PipelineError> {
    let input = std::fs::read(&meeting.input_path).map_err(|source| PipelineError::ReadFile {
        path: meeting.input_path.clone(),
        source,
    })?;
    match meeting.input_kind {
        InputKind::TranscriptonicPayload => Ok(sources::transcriptonic::parse(&input)?),
        InputKind::SlackHuddleLog => Ok(sources::slack_huddle::parse(&input)?),
    }
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
