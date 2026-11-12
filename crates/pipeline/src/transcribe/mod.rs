//! The transcribe step. The call is cut into 5-minute windows. Captions stay the transcript of
//! each window they cover well; the rest go to a speech-to-text provider, one stem and window per
//! request, with silences cut out.
//!
//! Free checks decide first: empty, too few or failed captions, too few caption words per
//! minute of speech, and caption text in another script or language. Then two call-level
//! checks: a language check on the loudest speech of each stem, and the LLM's read of the
//! captions. Either one sends every window but those with dense captions that its clips heard
//! as English.
//!
//! A call with caption lines gets its note from the captions at once, marked pending. The
//! requests go out in the next run, which rewrites the note. A provider that cannot be reached
//! puts the meeting in a network wait, and the sweep tries again on a schedule.
//!
//! The step's output lives in the call folder's `stt/`, not the job store: each reply with the
//! audio it answers, the caption verdict, and `choice.json`, what the step settled on. A later
//! run reads them and makes no request.
//!
//! Once the Drive archive has taken a folder's stems, the saved replies are the transcript: a
//! rerun reads them without checking them against the stems, and sends nothing. The archive
//! deletes stems only under `audio.lock`, so the step decides whether they are gone, and reads
//! saved replies against them, under that lock.

mod audio;
mod checks;
pub(crate) mod coherence;
mod requests;
mod timeline;
mod windows;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use call_folder::drive_record::DriveStanding;
use jobs::{JobState, LeasedRun, Meeting, MeetingId, NetworkFault, RunOutcome, Step, StepRetry};
use serde::{Deserialize, Serialize};
use sources::Transcript;
use stem_timing::Stem;
use stt::{FailureClass, SpeechToText, Utterance};

use crate::intake::MIN_CAPTION_SEGMENTS;
use crate::llm::Provider as LlmProvider;
use crate::settings::TranscriptionSettings;
use crate::{InputError, Pipeline, PipelineError, call_folder_name};
pub(crate) use audio::CallSpan;
use audio::{AudioCut, CallAudio};
use coherence::{CaptionSample, CaptionVerdict, SavedVerdict};
use requests::{StemLanguage, Uploader};
use timeline::SpeechPiece;
use windows::WindowPlan;

/// The folder in a call folder that holds the step's output.
const STT_DIR_NAME: &str = "stt";
/// What the step settled on.
const CHOICE_FILE_NAME: &str = "choice.json";
const VERDICT_FILE_NAME: &str = "coherence.json";
/// Written by `mc transcribe`: every window goes to speech-to-text whatever its captions.
const FORCED_FILE_NAME: &str = "forced";

/// How long the step waits for the audio writer to close the call's last segment. The call
/// log ends a moment before the capture process stops.
const AUDIO_SETTLE_WAIT: Duration = Duration::from_secs(60);

/// Tries in a row that find `audio.lock` held before the meeting fails. A deletion holds the
/// lock for seconds, and each try waits `AUDIO_SETTLE_WAIT` at 5-minute sweeps, so 12 cover an
/// hour: a lock held that long belongs to a stuck process, which a person must look at.
const AUDIO_BUSY_TRY_LIMIT: u32 = 12;

/// Shorter stems hold no speech worth a request.
const MIN_STEM_SAMPLES: u64 = stem_writer::OUTPUT_RATE_HZ as u64 / 2;

/// Why lines of a meeting's transcript came from speech-to-text instead of its captions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FallbackReason {
    /// The call has no caption lines.
    Empty,
    /// The call has fewer caption lines than intake keeps a call for.
    TooFew,
    /// The call app reported its captions broken or refused.
    Failed,
    /// A window has too few caption words for its speech.
    Sparse,
    /// The caption text is in another script or language.
    OtherLanguage,
    /// The language check heard speech that is not English.
    NonEnglishSpeech,
    /// The LLM read the captions as garbled.
    Garbled,
    /// `mc transcribe` asked for it.
    Forced,
}

impl FallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooFew => "too-few",
            Self::Failed => "failed",
            Self::Sparse => "sparse",
            Self::OtherLanguage => "other-language",
            Self::NonEnglishSpeech => "non-english-speech",
            Self::Garbled => "garbled",
            Self::Forced => "forced",
        }
    }
}

/// `choice.json`: what the step settled on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transcript", rename_all = "kebab-case")]
pub(crate) enum SavedChoice {
    /// The captions passed every check, or no provider is set.
    Captions,
    /// The note holds the captions while speech-to-text is owed. The next run sends it.
    Pending {
        /// `<provider>/<model>`.
        provider_label: String,
        /// `None` until the language check reaches the provider.
        reason: Option<FallbackReason>,
    },
    /// Some or all of the transcript came from speech-to-text.
    Speech(SpeechChoice),
    /// The provider refused the key, the account or the request, or the audio failed on this
    /// machine, so the captions stay.
    CaptionsKept {
        provider_label: String,
        reason: Option<FallbackReason>,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SpeechChoice {
    /// `<provider>/<model>`.
    pub provider_label: String,
    pub provider: stt::ProviderKind,
    pub reason: FallbackReason,
    /// The windows whose caption lines the speech replaced.
    pub sent: Vec<CallSpan>,
    /// The replies in `stt/` that hold the speech of those windows.
    pub replies: Vec<SpeechReply>,
    /// Minutes of the call whose lines came from speech-to-text.
    pub speech_min: f64,
    /// Caption lines remain outside the windows sent.
    pub has_caption_lines: bool,
}

/// One saved reply the transcript reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SpeechReply {
    pub file_name: String,
    pub stem: Stem,
}

/// The transcript a run uses, and what the step settled on.
pub(crate) struct SettledTranscript {
    pub transcript: Transcript,
    pub choice: SavedChoice,
}

impl SettledTranscript {
    fn captions(transcript: Transcript) -> Self {
        Self {
            transcript,
            choice: SavedChoice::Captions,
        }
    }

    /// Whether the transcript is the captions while speech-to-text is owed.
    pub(crate) fn is_pending(&self) -> bool {
        match self.choice {
            SavedChoice::Pending { .. } => true,
            SavedChoice::Captions | SavedChoice::Speech(_) | SavedChoice::CaptionsKept { .. } => {
                false
            }
        }
    }

    /// Why speech-to-text replaced caption lines, or would, for the logs.
    pub(crate) fn reason(&self) -> &'static str {
        match &self.choice {
            SavedChoice::Captions => "captions",
            SavedChoice::Pending { reason, .. } | SavedChoice::CaptionsKept { reason, .. } => {
                reason.map_or("unchecked", FallbackReason::as_str)
            }
            SavedChoice::Speech(speech) => speech.reason.as_str(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error(transparent)]
    SpeechToText(#[from] stt::SttError),
    #[error("{source}. The meeting waits for the network")]
    Unreachable {
        source: stt::SttError,
        /// A reply was saved in this run before the provider stopped answering.
        has_progressed: bool,
    },
    #[error(
        "speech-to-text used up this run's time; the next run resumes after the last saved reply"
    )]
    RunTimeSpent { has_progressed: bool },
    #[error("the saved speech-to-text replies do not match the call audio as it is now")]
    StaleReplies,
    #[error("could not place the call's stems: {0}")]
    Placement(#[from] call_finalize::FinalizeError),
    #[error("could not read or write {path}: {source}")]
    Saved {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a saved speech-to-text file: {source}")]
    SavedFormat {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "no speech-to-text provider is set; add a [transcription] table with provider and api_key to settings"
    )]
    NoProvider,
    #[error("meeting {0} has no call audio to transcribe")]
    NoCallAudio(MeetingId),
    #[error("the call in {0} is still recording; retry when it ends")]
    StillRecording(PathBuf),
    #[error("{0} has an audio segment that repair could not close; retry once the file is free")]
    OpenSegment(PathBuf),
    #[error("could not read or repair the call audio: {0}")]
    Stems(#[from] stem_writer::StemWriterError),
    #[error("the stems of meeting {0} are on Google Drive; run `mc restore {0}` first")]
    StemsOnDrive(MeetingId),
    #[error("stems archived; only the mixed recording remains")]
    StemsMixedOnly(MeetingId),
    #[error("{0} is busy with a capture or the Drive archive; the next sweep tries again")]
    AudioBusy(PathBuf),
    #[error(
        "audio.lock in {folder} stayed held through {tries} tries in a row; find the process that holds it, then retry"
    )]
    AudioLockStuck { folder: PathBuf, tries: u32 },
}

impl TranscribeError {
    /// How the meeting runs again after this error.
    pub(crate) fn retry(&self) -> StepRetry {
        match self {
            Self::Unreachable {
                source,
                has_progressed,
            } => StepRetry::AfterNetworkWait {
                has_progressed: *has_progressed,
                fault: network_fault(source),
            },
            // A run that saved replies before its time ran out is moving: the next one goes on
            // at once. One that saved none spent its time on a slow link, so it waits.
            Self::RunTimeSpent { has_progressed } => {
                if *has_progressed {
                    StepRetry::Resume
                } else {
                    StepRetry::AfterNetworkWait {
                        has_progressed: false,
                        fault: NetworkFault::Link,
                    }
                }
            }
            // The lock goes once the deletion or the capture ends; nothing failed.
            Self::AudioBusy(_) => StepRetry::WhenFree,
            Self::SpeechToText(_)
            | Self::AudioLockStuck { .. }
            | Self::StaleReplies
            | Self::Placement(_)
            | Self::Saved { .. }
            | Self::SavedFormat { .. }
            | Self::NoProvider
            | Self::NoCallAudio(_)
            | Self::StillRecording(_)
            | Self::OpenSegment(_)
            | Self::Stems(_)
            | Self::StemsOnDrive(_)
            | Self::StemsMixedOnly(_) => StepRetry::Manual,
        }
    }

    /// Whether the provider refused the key, the account or the request.
    fn is_refusal(&self) -> bool {
        match self {
            Self::SpeechToText(error) => error.class() == FailureClass::Refused,
            Self::Unreachable { .. }
            | Self::RunTimeSpent { .. }
            | Self::StaleReplies
            | Self::Placement(_)
            | Self::Saved { .. }
            | Self::SavedFormat { .. }
            | Self::NoProvider
            | Self::NoCallAudio(_)
            | Self::StillRecording(_)
            | Self::OpenSegment(_)
            | Self::Stems(_)
            | Self::StemsOnDrive(_)
            | Self::StemsMixedOnly(_)
            | Self::AudioBusy(_)
            | Self::AudioLockStuck { .. } => false,
        }
    }

    /// Whether the provider could not be reached, or the run ran out of time for it: a later
    /// run may get the transcript.
    fn is_network_wait(&self) -> bool {
        match self.retry() {
            StepRetry::AfterNetworkWait { .. } | StepRetry::Resume | StepRetry::WhenFree => true,
            StepRetry::Manual => false,
        }
    }
}

/// Which side failed a request the provider did not answer with a transcript.
fn network_fault(error: &stt::SttError) -> NetworkFault {
    match error {
        stt::SttError::ServerError { .. } | stt::SttError::RateLimited { .. } => {
            NetworkFault::Service
        }
        stt::SttError::Timeout { .. }
        | stt::SttError::Network { .. }
        | stt::SttError::KeyRefused { .. }
        | stt::SttError::KeyMalformed { .. }
        | stt::SttError::OutOfCredit { .. }
        | stt::SttError::RequestRejected { .. }
        | stt::SttError::ReadStem { .. }
        | stt::SttError::Encode { .. }
        | stt::SttError::UnreadableReply { .. }
        // Refused-class faults never reach the network schedule, but keep the match total.
        | stt::SttError::WisprSession { .. }
        | stt::SttError::EngineNotBuilt { .. } => NetworkFault::Link,
    }
}

/// What the step can send to speech-to-text.
struct Fallback {
    provider: stt::Provider,
    folder: PathBuf,
}

/// Which run of the step this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The step's first run. A call with caption lines gets its note now, pending.
    First,
    /// The run after a pending note. Every request goes out now.
    Upgrade,
}

impl Pipeline {
    /// The meeting's call folder when it exists on this machine.
    pub(crate) fn call_folder(&self, meeting: &Meeting) -> Option<PathBuf> {
        let folder = self.app_dirs.calls_dir().join(call_folder_name(meeting)?);
        folder.is_dir().then_some(folder)
    }

    /// Whether intake should keep a call with too few captions: a provider is set and the
    /// call has stems to transcribe.
    pub(crate) fn can_transcribe_log(&self, log_path: &Path) -> bool {
        let Some(folder_name) = crate::input_stem(log_path) else {
            return false;
        };
        self.settings_store.transcription().provider.is_some()
            && has_stems(&self.app_dirs.calls_dir().join(folder_name))
    }

    /// The transcript a run uses. While the transcribe step is owed, runs the checks, saves
    /// the choice and records the step. After a pending choice, sends the requests and saves the
    /// new choice. After any other, reads the saved choice and makes no request; saved replies
    /// that no longer match the call audio send the meeting back to its first step.
    pub(crate) fn transcribe_step(
        &self,
        run: &LeasedRun<'_>,
        captions: Transcript,
    ) -> Result<SettledTranscript, PipelineError> {
        self.settle_transcript(run, captions)
            .map_err(|error| fail_stuck_audio_lock(run.meeting(), error))
    }

    fn settle_transcript(
        &self,
        run: &LeasedRun<'_>,
        captions: Transcript,
    ) -> Result<SettledTranscript, PipelineError> {
        let meeting = run.meeting();
        let _span = tracing::info_span!(
            "pipeline.transcribe",
            meeting.id = %meeting.id,
            job.step = Step::Transcribed.as_str()
        )
        .entered();
        if meeting.finished_step < Step::Transcribed {
            if let Some(folder) = self.call_folder(meeting)
                && let Some(choice) = archived_choice(&folder)?
            {
                // The stems are on Drive or gone, so nothing could be checked or sent again.
                let settled =
                    saved_transcript(&folder, captions, self.settings.my_name.as_deref(), choice)?;
                tracing::info!(meeting.id = %meeting.id, "stems archived; saved transcript reused");
                run.confirm_current_input()?;
                write_json(&stt_dir(&folder), CHOICE_FILE_NAME, &settled.choice)?;
                run.record_transcription()?;
                return Ok(settled);
            }
            let settled = self.transcribe(run, captions, Phase::First)?;
            if let Some(folder) = self.call_folder(meeting) {
                run.confirm_current_input()?;
                write_json(&stt_dir(&folder), CHOICE_FILE_NAME, &settled.choice)?;
            }
            run.record_transcription()?;
            return Ok(settled);
        }
        let Some(folder) = self.call_folder(meeting) else {
            return Ok(SettledTranscript::captions(captions));
        };
        let choice = match read_json::<SavedChoice>(&stt_dir(&folder).join(CHOICE_FILE_NAME)) {
            Ok(Some(choice)) => choice,
            Ok(None) => return Ok(SettledTranscript::captions(captions)),
            Err(error) => {
                tracing::warn!(exception.message = %error, "saved transcript choice unreadable; running every step again");
                return Err(run.restart().into());
            }
        };
        match choice {
            SavedChoice::Pending { .. } => {
                let settled = self.transcribe(run, captions, Phase::Upgrade)?;
                run.confirm_current_input()?;
                write_json(&stt_dir(&folder), CHOICE_FILE_NAME, &settled.choice)?;
                Ok(settled)
            }
            SavedChoice::Captions | SavedChoice::Speech(_) | SavedChoice::CaptionsKept { .. } => {
                match saved_transcript(&folder, captions, self.settings.my_name.as_deref(), choice)
                {
                    Ok(settled) => Ok(settled),
                    // The note cites lines by number, so captions in place of the speech would
                    // point its citations at the wrong lines.
                    Err(error) => {
                        tracing::warn!(exception.message = %error, "saved speech-to-text replies unusable; running every step again");
                        Err(run.restart().into())
                    }
                }
            }
        }
    }

    /// The transcript the transcribe step settled on, from the saved choice, for a reader with
    /// no run. Makes no request: unusable saved replies leave the captions, with a warning.
    pub(crate) fn settled_transcript(
        &self,
        meeting: &Meeting,
        captions: Transcript,
    ) -> SettledTranscript {
        let Some(folder) = self.call_folder(meeting) else {
            return SettledTranscript::captions(captions);
        };
        let choice = match read_json::<SavedChoice>(&stt_dir(&folder).join(CHOICE_FILE_NAME)) {
            Ok(Some(choice)) => choice,
            Ok(None) => return SettledTranscript::captions(captions),
            Err(error) => {
                tracing::warn!(exception.message = %error, "saved transcript choice unreadable; using the captions");
                return SettledTranscript::captions(captions);
            }
        };
        match saved_transcript(
            &folder,
            captions.clone(),
            self.settings.my_name.as_deref(),
            choice,
        ) {
            Ok(settled) => settled,
            Err(error) => {
                tracing::warn!(exception.message = %error, "saved speech-to-text replies unusable; using the captions");
                SettledTranscript::captions(captions)
            }
        }
    }

    /// `mc transcribe`: marks the meeting's call for speech-to-text whatever its captions, then
    /// runs every step again. The mark stays, so a later rerun keeps the speech transcript.
    ///
    /// # Errors
    ///
    /// Returns `TranscribeError::NoProvider` with no provider in settings,
    /// `TranscribeError::NoCallAudio` when the meeting has no call folder with stems,
    /// `TranscribeError::StemsOnDrive` or `TranscribeError::StemsMixedOnly` when the Drive
    /// archive took them, and the errors of `reprocess`.
    pub fn transcribe_again(
        &self,
        id: MeetingId,
        discard_note_edits: bool,
    ) -> Result<RunOutcome, PipelineError> {
        let meeting = self.store.get(id)?;
        if self.settings_store.transcription().provider.is_none() {
            return Err(TranscribeError::NoProvider.into());
        }
        let folder = self
            .call_folder(&meeting)
            .filter(|folder| has_stems(folder))
            .ok_or(TranscribeError::NoCallAudio(id))?;
        match stems_standing(&folder) {
            StemsStanding::Local => {}
            StemsStanding::OnDrive => return Err(TranscribeError::StemsOnDrive(id).into()),
            StemsStanding::MixedOnly => return Err(TranscribeError::StemsMixedOnly(id).into()),
        }
        write_file(
            &stt_dir(&folder),
            FORCED_FILE_NAME,
            jiff::Timestamp::now().to_string().as_bytes(),
        )?;
        tracing::info!(meeting.id = %id, "speech-to-text forced");
        self.reprocess(id, discard_note_edits)
    }

    fn transcribe(
        &self,
        run: &LeasedRun<'_>,
        captions: Transcript,
        phase: Phase,
    ) -> Result<SettledTranscript, PipelineError> {
        let meeting = run.meeting();
        let settings = self.settings_store.transcription();
        let folder = self.call_folder(meeting);
        let is_forced = folder
            .as_ref()
            .is_some_and(|folder| stt_dir(folder).join(FORCED_FILE_NAME).is_file());
        let fallback = fallback(meeting, &settings, folder)?;
        let free_flag = free_flag(meeting, &captions, is_forced, fallback.is_some())?;
        let Some(fallback) = fallback else {
            log_caption_check(free_flag, "captions", 0, 0);
            return Ok(SettledTranscript::captions(captions));
        };
        let has_caption_lines = captions.segments.len() >= MIN_CAPTION_SEGMENTS;
        let stopped = Stopped {
            captions: &captions,
            provider_label: fallback.provider.label(),
            has_caption_lines,
            is_forced: free_flag == Some(FallbackReason::Forced),
        };

        let audio = match call_audio(meeting, &fallback, &captions) {
            Ok(audio) => audio,
            Err(error) => return stopped.keep_captions(free_flag, error),
        };
        let free_plan = windows::plan(&captions, &audio, free_flag, &[]);
        let free_reason = free_flag.or_else(|| free_plan.iter().find_map(|window| window.send));
        if phase == Phase::First && has_caption_lines && free_reason.is_some() {
            return Ok(stopped.pending(free_reason));
        }

        let mut uploader =
            Uploader::new(&fallback.provider, &fallback.folder, run.renewal_ends_at());
        let needs_call_checks = free_plan
            .iter()
            .any(|window| window.send.is_none() && window.has_speech())
            && !matches!(
                free_flag,
                Some(FallbackReason::Empty | FallbackReason::TooFew | FallbackReason::Forced)
            );
        let (call_flag, languages) = if needs_call_checks {
            match self.call_checks(&mut uploader, &audio, &captions, free_flag) {
                Ok(found) => found,
                Err(error)
                    if error.is_network_wait() && phase == Phase::First && has_caption_lines =>
                {
                    tracing::warn!(exception.message = %error, "language check could not reach the provider");
                    return Ok(stopped.pending(free_reason));
                }
                Err(error) => return stopped.keep_captions(free_reason, error),
            }
        } else {
            (free_flag, Vec::new())
        };
        let plan = windows::plan(&captions, &audio, call_flag, &languages);
        let sent: Vec<&WindowPlan> = plan.iter().filter(|window| window.send.is_some()).collect();
        let Some(window_reason) = sent.iter().find_map(|window| window.send) else {
            log_caption_check(None, "captions", plan.len(), 0);
            return Ok(SettledTranscript::captions(captions));
        };
        let reason = call_flag.unwrap_or(window_reason);
        log_caption_check(Some(reason), "speech_to_text", plan.len(), sent.len());
        log_plan(&plan, &audio, &languages);
        if phase == Phase::First && has_caption_lines {
            return Ok(stopped.pending(Some(reason)));
        }

        let replies = match send_windows(&mut uploader, &audio, &sent, &languages) {
            Ok(replies) => replies,
            Err(error) => return stopped.keep_captions(Some(reason), error),
        };
        let call_end_s = audio.end_s();
        let speech_s: f64 = sent
            .iter()
            .map(|window| (window.span.to_s.min(call_end_s) - window.span.from_s).max(0.0))
            .sum();
        tracing::info!(
            mc.stt.billed_s = uploader.billed_s,
            mc.stt.reused_s = uploader.reused_s,
            mc.stt.call_s = call_end_s,
            mc.stt.window_count = plan.len(),
            mc.stt.sent_window_count = sent.len(),
            "speech-to-text requests done"
        );
        let choice = SavedChoice::Speech(SpeechChoice {
            provider_label: stopped.provider_label.clone(),
            provider: fallback.provider.kind(),
            reason,
            sent: sent.iter().map(|window| window.span).collect(),
            replies,
            speech_min: speech_s / 60.0,
            has_caption_lines: false,
        });
        match saved_transcript(
            &fallback.folder,
            captions.clone(),
            self.settings.my_name.as_deref(),
            choice,
        ) {
            Ok(settled) => Ok(settled),
            Err(error) => stopped.keep_captions(Some(reason), error),
        }
    }

    /// The call-level checks after the free ones: the language check, then the LLM's read of
    /// the captions. Returns the call's flag and what the language check heard.
    fn call_checks(
        &self,
        uploader: &mut Uploader<'_>,
        audio: &CallAudio,
        captions: &Transcript,
        free_flag: Option<FallbackReason>,
    ) -> Result<(Option<FallbackReason>, Vec<StemLanguage>), TranscribeError> {
        let languages = uploader.language_check(audio)?;
        if free_flag.is_some() {
            return Ok((free_flag, languages));
        }
        if !languages.iter().all(StemLanguage::is_mostly_english) {
            return Ok((Some(FallbackReason::NonEnglishSpeech), languages));
        }
        // The LLM check costs a call, so only captions that passed every other check pay it.
        Ok((self.garbled_check(uploader.folder(), captions), languages))
    }

    /// `Some(Garbled)` or `Some(OtherLanguage)` when the LLM reads the captions so. A saved
    /// verdict for the same sample is reused. No LLM, or an LLM failure, keeps the captions.
    fn garbled_check(&self, folder: &Path, captions: &Transcript) -> Option<FallbackReason> {
        let sample = CaptionSample::of(captions);
        if sample.line_count() == 0 {
            return None;
        }
        let verdict_path = stt_dir(folder).join(VERDICT_FILE_NAME);
        let question_sha256 = sample.sha256();
        let saved = read_json::<SavedVerdict>(&verdict_path)
            .inspect_err(|error| tracing::warn!(exception.message = %error, "saved caption verdict unreadable"))
            .ok()
            .flatten()
            .filter(|saved| saved.question_sha256 == question_sha256);
        let verdict = if let Some(saved) = saved {
            tracing::info!(
                mc.stt.verdict_reused = true,
                "caption verdict read from stt/"
            );
            saved.verdict
        } else {
            let llm = self.llm();
            let provider = match LlmProvider::from_settings(&llm, &self.app_dirs) {
                Ok(Some(provider)) => provider,
                Ok(None) => {
                    tracing::debug!("no LLM provider; the garbled-caption check is skipped");
                    return None;
                }
                Err(error) => {
                    tracing::warn!(exception.message = %error, "garbled-caption check skipped");
                    return None;
                }
            };
            let judgement = match provider.judge_captions(&sample.message()) {
                Ok(judgement) => judgement,
                Err(error) => {
                    tracing::warn!(exception.message = %error, "garbled-caption check failed; keeping the captions");
                    return None;
                }
            };
            let saved = SavedVerdict {
                question_sha256,
                judged_by: provider.label().to_owned(),
                verdict: judgement.verdict,
            };
            if let Err(error) = write_json(&stt_dir(folder), VERDICT_FILE_NAME, &saved) {
                tracing::warn!(exception.message = %error, "caption verdict not saved");
            }
            judgement.verdict
        };
        tracing::info!(
            mc.stt.verdict = serde_name(verdict),
            mc.stt.sample_line_count = sample.line_count(),
            "caption coherence judged"
        );
        match verdict {
            CaptionVerdict::Coherent => None,
            CaptionVerdict::Garbled => Some(FallbackReason::Garbled),
            CaptionVerdict::OtherLanguage => Some(FallbackReason::OtherLanguage),
        }
    }
}

/// Sends each stem of each window in `sent`, leaving out the language check's clips, which
/// already hold the speech of their seconds. Returns the replies the transcript reads.
fn send_windows(
    uploader: &mut Uploader<'_>,
    audio: &CallAudio,
    sent: &[&WindowPlan],
    languages: &[StemLanguage],
) -> Result<Vec<SpeechReply>, TranscribeError> {
    let mut replies = Vec::new();
    for window in sent {
        for stem in [Stem::Mic, Stem::Remote] {
            let clips: Vec<CallSpan> = languages
                .iter()
                .filter(|language| language.stem == stem)
                .flat_map(|language| language.clips.iter().map(|clip| clip.span))
                .collect();
            if uploader
                .window_reply(audio, window.index, stem, window.span, &clips)?
                .is_some()
            {
                replies.push(SpeechReply {
                    file_name: requests::window_file_name(window.index, stem),
                    stem,
                });
            }
        }
    }
    for language in languages {
        let is_in_sent = language
            .clips
            .iter()
            .any(|clip| sent.iter().any(|window| window.span.overlaps(clip.span)));
        if is_in_sent {
            replies.push(SpeechReply {
                file_name: requests::clips_file_name(language.stem),
                stem: language.stem,
            });
        }
    }
    Ok(replies)
}

/// The first call-level check the captions fail. A forced call with somewhere to send it
/// skips them.
fn free_flag(
    meeting: &Meeting,
    captions: &Transcript,
    is_forced: bool,
    can_fall_back: bool,
) -> Result<Option<FallbackReason>, PipelineError> {
    if is_forced && can_fall_back {
        return Ok(Some(FallbackReason::Forced));
    }
    if is_forced {
        // The mark outlives a provider removed since `mc transcribe`.
        tracing::warn!(
            "speech-to-text was forced, but no provider or call audio is there; checking the captions"
        );
    }
    let log = std::fs::read(&meeting.input_path).map_err(|source| InputError::Read {
        path: meeting.input_path.clone(),
        source,
    })?;
    Ok(checks::first_failed(meeting.input_kind, &log, captions)?)
}

/// What a run settles on when it stops before every request is answered.
struct Stopped<'a> {
    captions: &'a Transcript,
    provider_label: String,
    has_caption_lines: bool,
    is_forced: bool,
}

impl Stopped<'_> {
    /// The captions now, while speech-to-text is owed.
    fn pending(&self, reason: Option<FallbackReason>) -> SettledTranscript {
        tracing::info!(
            mc.decision.kind = "caption_check",
            mc.decision.chosen = "pending",
            mc.decision.reason = reason.map_or("unchecked", FallbackReason::as_str),
            "the note gets the captions now; speech-to-text runs next"
        );
        SettledTranscript {
            transcript: self.captions.clone(),
            choice: SavedChoice::Pending {
                provider_label: self.provider_label.clone(),
                reason,
            },
        }
    }

    /// The captions after a refusal or a local error, such as audio that cannot be read or
    /// placed, when they have lines; else the error. Neither passes on a retry, and good
    /// captions beat a failed meeting. A network wait is the error: the next try may work. A
    /// forced call asked for speech, so it fails instead.
    fn keep_captions(
        &self,
        reason: Option<FallbackReason>,
        error: TranscribeError,
    ) -> Result<SettledTranscript, PipelineError> {
        if !self.has_caption_lines || self.is_forced || error.is_network_wait() {
            return Err(error.into());
        }
        if error.is_refusal() {
            tracing::warn!(exception.message = %error, "speech-to-text refused; keeping the captions");
        } else {
            tracing::warn!(exception.message = %error, "speech-to-text failed on this machine; keeping the captions");
        }
        Ok(SettledTranscript {
            transcript: self.captions.clone(),
            choice: SavedChoice::CaptionsKept {
                provider_label: self.provider_label.clone(),
                reason,
                error: error.to_string(),
            },
        })
    }
}

/// Each window's decision, and the seconds of audio the windows sent would bill against the
/// call's length, before any request.
fn log_plan(plan: &[WindowPlan], audio: &CallAudio, languages: &[StemLanguage]) {
    let mut planned_s = 0.0;
    for window in plan {
        let mut window_audio_s = 0.0;
        if window.send.is_some() {
            for stem in [Stem::Mic, Stem::Remote] {
                let clips: Vec<CallSpan> = languages
                    .iter()
                    .filter(|language| language.stem == stem)
                    .flat_map(|language| language.clips.iter().map(|clip| clip.span))
                    .collect();
                window_audio_s += audio.cut(stem, window.span, &clips).seconds();
            }
        }
        planned_s += window_audio_s;
        tracing::debug!(
            mc.stt.window_index = window.index,
            mc.stt.caption_words_per_voiced_min = window.words_per_voiced_min(),
            mc.stt.voiced_min = window.voiced_min,
            mc.decision.reason = window.send.map_or("captions", FallbackReason::as_str),
            mc.stt.audio_s = window_audio_s,
            "window decided"
        );
    }
    tracing::info!(
        mc.stt.planned_audio_s = planned_s,
        mc.stt.call_s = audio.end_s(),
        "speech-to-text planned"
    );
}

/// One decision event for the caption checks. Counts only.
fn log_caption_check(
    reason: Option<FallbackReason>,
    chosen: &'static str,
    window_count: usize,
    sent_window_count: usize,
) {
    tracing::info!(
        mc.decision.kind = "caption_check",
        mc.decision.chosen = chosen,
        mc.decision.reason = reason.map_or("none", FallbackReason::as_str),
        mc.stt.window_count = window_count,
        mc.stt.sent_window_count = sent_window_count,
        "captions checked"
    );
}

/// The transcript `choice` names, read from the saved replies with no request.
fn saved_transcript(
    folder: &Path,
    captions: Transcript,
    my_name: Option<&str>,
    choice: SavedChoice,
) -> Result<SettledTranscript, TranscribeError> {
    let mut speech = match choice {
        SavedChoice::Speech(speech) => speech,
        SavedChoice::Captions | SavedChoice::Pending { .. } | SavedChoice::CaptionsKept { .. } => {
            return Ok(SettledTranscript {
                transcript: captions,
                choice,
            });
        }
    };
    // Held while the replies are checked against the stems, so the archive deletes none
    // mid-read; once they are gone, the check is skipped.
    let _audio_lock = take_audio_lock(folder)?;
    let standing = stems_standing(folder);
    let mut pieces = Vec::new();
    for (request, saved) in speech.replies.iter().enumerate() {
        let answered = requests::saved_answer(folder, &saved.file_name, standing)?;
        let mut by_window: BTreeMap<usize, Vec<Utterance>> = BTreeMap::new();
        for utterance in speech.provider.utterances(&answered.reply)? {
            let utterance = on_call_clock(utterance, &answered.cut);
            // A clip reply also covers windows whose captions stay.
            for (window_index, part) in timeline::split_by_window(utterance, &speech.sent) {
                by_window.entry(window_index).or_default().push(part);
            }
        }
        pieces.extend(
            by_window
                .into_iter()
                .map(|(window_index, utterances)| SpeechPiece {
                    stem: saved.stem,
                    request,
                    window_index,
                    utterances,
                }),
        );
    }
    let (transcript, counts) =
        timeline::hybrid_transcript(&captions, my_name, &speech.sent, pieces);
    tracing::info!(
        mc.transcript.caption_line_count = counts.caption_lines,
        mc.transcript.speech_line_count = counts.speech_lines,
        mc.stt.echo_word_count = counts.echo_words,
        mc.stt.named_speaker_count = counts.named_speakers,
        mc.stt.numbered_speaker_count = counts.numbered_speakers,
        "transcript assembled"
    );
    speech.has_caption_lines = counts.caption_lines > 0;
    Ok(SettledTranscript {
        transcript,
        choice: SavedChoice::Speech(speech),
    })
}

/// `utterance` with its times moved from the audio sent to the call's clock.
fn on_call_clock(mut utterance: Utterance, cut: &AudioCut) -> Utterance {
    utterance.start_s = cut.call_s(utterance.start_s);
    utterance.end_s = cut.call_s(utterance.end_s).max(utterance.start_s);
    for word in &mut utterance.words {
        word.start_s = cut.call_s(word.start_s);
        word.end_s = cut.call_s(word.end_s).max(word.start_s);
    }
    utterance
}

/// The provider and call folder a fallback would use, or `None` with the reason logged.
fn fallback(
    meeting: &Meeting,
    settings: &TranscriptionSettings,
    folder: Option<PathBuf>,
) -> Result<Option<Fallback>, PipelineError> {
    let (Some(kind), Some(model), Some(language)) =
        (settings.provider, settings.model(), settings.language())
    else {
        tracing::debug!("no speech-to-text provider; captions only");
        return Ok(None);
    };
    let Some(folder) = folder.filter(|folder| has_stems(folder)) else {
        tracing::debug!(
            meeting.id = %meeting.id,
            "no call folder with stems; captions only"
        );
        return Ok(None);
    };
    match stems_standing(&folder) {
        StemsStanding::Local => {}
        StemsStanding::OnDrive | StemsStanding::MixedOnly => {
            tracing::info!(
                meeting.id = %meeting.id,
                "stems archived and no saved transcript; captions only"
            );
            return Ok(None);
        }
    }
    if !kind.is_available() {
        tracing::warn!(
            provider = kind.as_str(),
            "the transcription provider is not available in this build on this machine; captions only"
        );
        return Ok(None);
    }
    if kind.requires_api_key() && settings.api_key.is_none() {
        tracing::warn!(
            provider = kind.as_str(),
            "transcription.provider is set without transcription.api_key; captions only"
        );
        return Ok(None);
    }
    let provider = stt::Provider::new(
        kind,
        settings.api_key.clone(),
        model.to_owned(),
        language.to_owned(),
    )
    .map_err(TranscribeError::from)?;
    Ok(Some(Fallback { provider, folder }))
}

/// The call's stems, framed on the captions' clock, once the audio writer has let go of them.
fn call_audio(
    meeting: &Meeting,
    fallback: &Fallback,
    captions: &Transcript,
) -> Result<CallAudio, TranscribeError> {
    // A meeting waiting for the network tries at each sweep once the schedule is spent, so a
    // closed link stops here, before the stems are read again.
    if matches!(meeting.state(), JobState::NetworkWait { .. }) {
        fallback
            .provider
            .check_reachable()
            .map_err(|source| TranscribeError::Unreachable {
                source,
                has_progressed: false,
            })?;
    }
    let folder = &fallback.folder;
    wait_for_audio(folder)?;
    let timeline = call_finalize::plan_folder(folder)?;
    let call_offset_s = timeline.started_at.map_or(0.0, |started_at| {
        started_at.duration_since(captions.started_at).as_secs_f64()
    });
    CallAudio::read(&timeline, call_offset_s, MIN_STEM_SAMPLES)
}

/// Waits for the audio writer to let go of the folder, so the last segment is closed, and
/// repairs a segment a dead writer left open.
fn wait_for_audio(folder: &Path) -> Result<(), TranscribeError> {
    let lock = call_folder::FolderLock::take_within(
        folder,
        stem_writer::LOCK_FILE_NAME,
        AUDIO_SETTLE_WAIT,
    )
    .map_err(|source| TranscribeError::Saved {
        path: folder.join(stem_writer::LOCK_FILE_NAME),
        source,
    })?;
    let Some(lock) = lock else {
        return Err(TranscribeError::StillRecording(folder.to_path_buf()));
    };
    // The launch repair skips a folder while this step holds its lock, and the plan leaves an
    // open segment out, so without this repair a crash's audio would be missing for good.
    if has_open_segment(folder)? {
        let repair = stem_writer::repair_under_lock(folder, &lock)?;
        tracing::info!(
            file.directory = %folder.display(),
            mc.repair.outcome = repair_outcome(&repair),
            "open audio segment repaired before transcription"
        );
    }
    // Released at once: a rejoin's writer may need the folder.
    drop(lock);
    if has_open_segment(folder)? {
        return Err(TranscribeError::OpenSegment(folder.to_path_buf()));
    }
    Ok(())
}

/// Whether `audio.json` lists a segment no writer closed and no repair recovered.
fn has_open_segment(folder: &Path) -> Result<bool, TranscribeError> {
    let manifest = stem_writer::AudioManifest::load(folder)?;
    Ok(manifest
        .segments
        .iter()
        .any(|segment| segment.state == stem_writer::SegmentState::Open))
}

fn repair_outcome(repair: &stem_writer::FolderRepair) -> &'static str {
    match repair {
        stem_writer::FolderRepair::Busy => "busy",
        stem_writer::FolderRepair::Clean => "clean",
        stem_writer::FolderRepair::Repaired { .. } => "repaired",
    }
}

/// Where a call folder's stems stand against the Drive archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StemsStanding {
    /// Every stem `audio.json` lists is here, or the folder is not archived.
    Local,
    /// The archive deleted stems that Drive holds as FLAC; `mc restore` brings them back.
    OnDrive,
    /// The archive deleted stems that live on only in `call.mp4`'s mix.
    MixedOnly,
}

impl StemsStanding {
    /// Whether the saved replies stand without the stems they were read from.
    pub(super) fn is_archived(self) -> bool {
        match self {
            Self::Local => false,
            Self::OnDrive | Self::MixedOnly => true,
        }
    }
}

/// Whether the archive took any stem `audio.json` lists. A record that cannot be read counts as
/// on Drive, so `mc restore` reports what is wrong with it.
fn stems_standing(folder: &Path) -> StemsStanding {
    if !call_folder::drive_record::is_archived(folder) {
        return StemsStanding::Local;
    }
    let Ok(manifest) = stem_writer::AudioManifest::load(folder) else {
        return StemsStanding::Local;
    };
    let missing_names: Vec<&str> = manifest
        .segments
        .iter()
        .flat_map(|segment| &segment.stems)
        .map(|stem| stem.file.as_str())
        .filter(|file_name| !folder.join(file_name).is_file())
        .collect();
    if missing_names.is_empty() {
        return StemsStanding::Local;
    }
    match crate::archive::DriveRecord::read(folder) {
        Ok(Some(record))
            if missing_names
                .iter()
                .any(|name| record.mixed_stem_names.iter().any(|mixed| mixed == name)) =>
        {
            StemsStanding::MixedOnly
        }
        Ok(_) => StemsStanding::OnDrive,
        Err(error) => {
            tracing::warn!(exception.message = %error, "drive.json unreadable; stems count as on Drive");
            StemsStanding::OnDrive
        }
    }
}

/// `AudioBusy` once `AUDIO_BUSY_TRY_LIMIT` tries in a row found `audio.lock` held, this one
/// included, becomes a failure the tray shows, so a stuck lock never hides behind retries.
fn fail_stuck_audio_lock(meeting: &Meeting, error: PipelineError) -> PipelineError {
    if let PipelineError::Transcribe(TranscribeError::AudioBusy(folder)) = &error {
        let tries = meeting.tries_while_held(&error.to_string()) + 1;
        if tries >= AUDIO_BUSY_TRY_LIMIT {
            return TranscribeError::AudioLockStuck {
                folder: folder.clone(),
                tries,
            }
            .into();
        }
    }
    error
}

/// `audio.lock`, waited for as `wait_for_audio` does. The archive takes it with no wait, so
/// holding it keeps every stem in place. Still held after the wait, the run stops and the next
/// sweep tries again, since what it read without the lock could vanish mid-read.
fn take_audio_lock(folder: &Path) -> Result<call_folder::FolderLock, TranscribeError> {
    call_folder::FolderLock::take_within(folder, stem_writer::LOCK_FILE_NAME, AUDIO_SETTLE_WAIT)
        .map_err(|source| TranscribeError::Saved {
            path: folder.join(stem_writer::LOCK_FILE_NAME),
            source,
        })?
        .ok_or_else(|| TranscribeError::AudioBusy(folder.to_path_buf()))
}

/// The settled choice a run reuses because the archive took the stems it was made from. Decided
/// under `audio.lock`: a deletion under way finishes first, and the run's meeting has left
/// `Written`, so none starts after it.
fn archived_choice(folder: &Path) -> Result<Option<SavedChoice>, TranscribeError> {
    // With no record, an archive run would still have to upload, then plan again under the
    // locks, where this run's meeting holds it.
    if DriveStanding::of(folder) == DriveStanding::Local {
        return Ok(None);
    }
    let _audio_lock = take_audio_lock(folder)?;
    if !stems_standing(folder).is_archived() {
        return Ok(None);
    }
    Ok(
        match read_json::<SavedChoice>(&stt_dir(folder).join(CHOICE_FILE_NAME))? {
            Some(SavedChoice::Pending { .. }) | None => None,
            Some(
                choice @ (SavedChoice::Captions
                | SavedChoice::Speech(_)
                | SavedChoice::CaptionsKept { .. }),
            ) => Some(choice),
        },
    )
}

/// Whether `audio.json` lists a stem with samples.
fn has_stems(folder: &Path) -> bool {
    if !folder.join(stem_writer::MANIFEST_FILE_NAME).is_file() {
        return false;
    }
    match stem_writer::AudioManifest::load(folder) {
        Ok(manifest) => manifest
            .segments
            .iter()
            .flat_map(|segment| &segment.stems)
            .any(|stem| stem.samples_written >= MIN_STEM_SAMPLES),
        Err(error) => {
            tracing::warn!(exception.message = %error, "audio.json unreadable");
            false
        }
    }
}

/// What a call folder's saved transcript choice says, for the archive, which must keep the
/// stems while speech-to-text is owed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavedTranscriptStanding {
    /// No choice is saved, or it is final.
    Settled,
    /// The note holds the captions while speech-to-text is owed.
    Pending,
    Unreadable,
}

/// Whether each saved speech-to-text reply still matches the stems it was read from, for the
/// archive, which checks under the folder's locks before it deletes them. Once a stem is gone
/// the check is skipped: the run that deleted the first one checked every reply. Anything
/// unreadable counts as no match.
pub(crate) fn saved_replies_match_stems(folder: &Path) -> bool {
    if stems_standing(folder).is_archived() {
        return true;
    }
    let speech = match read_json::<SavedChoice>(&stt_dir(folder).join(CHOICE_FILE_NAME)) {
        Ok(Some(SavedChoice::Speech(speech))) => speech,
        Ok(
            None
            | Some(
                SavedChoice::Captions
                | SavedChoice::Pending { .. }
                | SavedChoice::CaptionsKept { .. },
            ),
        ) => return true,
        Err(error) => {
            tracing::warn!(exception.message = %error, "saved transcript choice unreadable");
            return false;
        }
    };
    for saved in &speech.replies {
        if let Err(error) = requests::saved_answer(folder, &saved.file_name, StemsStanding::Local) {
            tracing::warn!(
                file.name = saved.file_name,
                exception.message = %error,
                "saved reply does not match the stems"
            );
            return false;
        }
    }
    true
}

pub(crate) fn saved_transcript_standing(folder: &Path) -> SavedTranscriptStanding {
    match read_json::<SavedChoice>(&stt_dir(folder).join(CHOICE_FILE_NAME)) {
        Ok(Some(SavedChoice::Pending { .. })) => SavedTranscriptStanding::Pending,
        Ok(
            None
            | Some(SavedChoice::Captions | SavedChoice::Speech(_) | SavedChoice::CaptionsKept { .. }),
        ) => SavedTranscriptStanding::Settled,
        Err(error) => {
            tracing::warn!(exception.message = %error, "saved transcript choice unreadable");
            SavedTranscriptStanding::Unreadable
        }
    }
}

pub(super) fn stt_dir(folder: &Path) -> PathBuf {
    folder.join(STT_DIR_NAME)
}

fn serde_name(verdict: CaptionVerdict) -> &'static str {
    match verdict {
        CaptionVerdict::Coherent => "coherent",
        CaptionVerdict::Garbled => "garbled",
        CaptionVerdict::OtherLanguage => "other_language",
    }
}

/// `None` when the file is not there.
pub(super) fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, TranscribeError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TranscribeError::Saved {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| TranscribeError::SavedFormat {
            path: path.to_path_buf(),
            source,
        })
}

pub(super) fn write_json(
    dir: &Path,
    file_name: &str,
    value: &impl Serialize,
) -> Result<(), TranscribeError> {
    let text = serde_json::to_vec_pretty(value).map_err(|source| TranscribeError::SavedFormat {
        path: dir.join(file_name),
        source,
    })?;
    write_file(dir, file_name, &text)
}

/// Written beside, then renamed, so a reader never sees half a file. Synced before and after
/// the rename: a billed reply or `choice.json` lost to a power cut costs a request or leaves the
/// note pointing at a transcript the folder no longer names.
fn write_file(dir: &Path, file_name: &str, contents: &[u8]) -> Result<(), TranscribeError> {
    let path = dir.join(file_name);
    let saved_error = |source| TranscribeError::Saved {
        path: path.clone(),
        source,
    };
    crate::vault::durable_write::create_dir_durably(dir).map_err(saved_error)?;
    let mut staged = tempfile::NamedTempFile::new_in(dir).map_err(saved_error)?;
    std::io::Write::write_all(&mut staged, contents).map_err(saved_error)?;
    staged.as_file().sync_all().map_err(saved_error)?;
    staged
        .persist(&path)
        .map_err(|error| saved_error(error.error))?;
    crate::vault::durable_write::sync_dir(dir).map_err(saved_error)
}
