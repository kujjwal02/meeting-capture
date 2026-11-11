//! Requests to the speech-to-text provider, one at a time. Each reply is saved in `stt/` with
//! the trimmed audio it answers, so a dropped connection loses only the request in flight and a
//! later run resumes after the last saved reply.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use stem_timing::Stem;
use stt::{FailureClass, SpeechToText, StemVoices, SttError};

use super::audio::{AudioCut, CallAudio, CallSpan, samples_sha256};
use super::{TranscribeError, read_json, stt_dir, write_json};

/// Tries per request within one run. The job's network schedule takes over after the last.
const TRIES_PER_REQUEST: usize = 3;
/// The waits between tries within one run.
const RETRY_WAITS: [Duration; TRIES_PER_REQUEST - 1] =
    [Duration::from_secs(10), Duration::from_secs(30)];
/// The run stops starting requests this long before the job store stops renewing its lease,
/// so no request outlives the lease.
const LEASE_MARGIN: Duration = Duration::from_mins(1);
/// Less audio than this is not worth a request.
const MIN_REQUEST_S: f64 = 0.5;

/// Below this many tagged words a stem gives no language signal.
const MIN_LANGUAGE_WORDS: usize = 20;
/// A stem with this share of words not tagged English or more makes the call fall back.
pub(super) const MIN_NON_ENGLISH_SHARE: f64 = 0.2;
const ENGLISH_TAG: &str = "en";

/// A reply as saved in `stt/`, with what it was asked with.
#[derive(Serialize, Deserialize)]
struct SavedReply {
    request_key: String,
    /// The trimmed audio sent, which also maps the reply's times back to the call.
    cut: AudioCut,
    /// SHA-256 of the Ogg Opus file sent.
    audio_sha256: String,
    /// SHA-256 of the samples the cut read, so a stem file rewritten to the same size makes the
    /// reply stale. Empty in a reply saved before it was recorded, which then never matches.
    #[serde(default)]
    samples_sha256: String,
    reply: serde_json::Value,
}

/// What one request answered: the reply and the audio it answers.
pub(super) struct Answered {
    pub cut: AudioCut,
    pub reply: serde_json::Value,
}

/// Sends requests for one run, one at a time.
pub(super) struct Uploader<'a> {
    provider: &'a stt::Provider,
    folder: &'a Path,
    deadline: Instant,
    /// A reply was saved in this run.
    has_progressed: bool,
    pub billed_s: f64,
    pub reused_s: f64,
}

impl<'a> Uploader<'a> {
    /// `lease_renewal_ends_at` is when the job store stops renewing the run's lease.
    pub(super) fn new(
        provider: &'a stt::Provider,
        folder: &'a Path,
        lease_renewal_ends_at: Instant,
    ) -> Self {
        Self {
            provider,
            folder,
            deadline: lease_renewal_ends_at
                .checked_sub(LEASE_MARGIN)
                .unwrap_or_else(Instant::now),
            has_progressed: false,
            billed_s: 0.0,
            reused_s: 0.0,
        }
    }

    pub(super) fn folder(&self) -> &Path {
        self.folder
    }

    /// The reply for `cut` saved as `file_name`, else a new request. `None` when the cut holds
    /// too little audio to send. `read` is what later runs read from the reply: a reply it
    /// rejects is never saved, and a saved one it rejects is asked again.
    fn reply(
        &mut self,
        file_name: &str,
        request_key: &str,
        cut: AudioCut,
        send: impl Fn(&stt::Provider, &stt::EncodedAudio) -> Result<serde_json::Value, SttError>,
        read: impl Fn(&serde_json::Value) -> Result<(), SttError>,
    ) -> Result<Option<Answered>, TranscribeError> {
        if cut.seconds() < MIN_REQUEST_S {
            return Ok(None);
        }
        let saved_path = stt_dir(self.folder).join(file_name);
        let saved = read_json::<SavedReply>(&saved_path)
            .inspect_err(|error| tracing::warn!(exception.message = %error, "saved reply unreadable; asking again"))
            .ok()
            .flatten();
        let samples = cut.samples(self.folder)?;
        let samples_sha256 = samples_sha256(&samples);
        if let Some(saved) = saved
            && saved.request_key == request_key
            && saved.cut == cut
            && saved.samples_sha256 == samples_sha256
            && read(&saved.reply)
                .inspect_err(|error| tracing::warn!(exception.message = %error, "saved reply unreadable; asking again"))
                .is_ok()
        {
            tracing::debug!(file.name = file_name, "reply read from stt/");
            self.reused_s += cut.seconds();
            return Ok(Some(Answered {
                cut,
                reply: saved.reply,
            }));
        }
        let audio = stt::ogg_opus::encode(&samples)?;
        let _span = tracing::info_span!(
            "stt.request",
            file.name = file_name,
            mc.stt.audio_s = cut.seconds(),
            mc.stt.audio_bytes = audio.bytes.len(),
        )
        .entered();
        let reply = self.send_with_retries(&audio, &send)?;
        // A reply saved unread would pass for a cache hit and fail every later run.
        read(&reply)?;
        let saved = SavedReply {
            request_key: request_key.to_owned(),
            cut,
            audio_sha256: audio.sha256,
            samples_sha256,
            reply,
        };
        // A cache entry keyed by its audio, so it is saved even if the run lost its lease.
        write_json(&stt_dir(self.folder), file_name, &saved)?;
        self.has_progressed = true;
        self.billed_s += saved.cut.seconds();
        Ok(Some(Answered {
            cut: saved.cut,
            reply: saved.reply,
        }))
    }

    fn send_with_retries(
        &self,
        audio: &stt::EncodedAudio,
        send: &impl Fn(&stt::Provider, &stt::EncodedAudio) -> Result<serde_json::Value, SttError>,
    ) -> Result<serde_json::Value, TranscribeError> {
        let mut try_number = 1;
        loop {
            if Instant::now() + self.provider.request_timeout(audio.bytes.len()) > self.deadline {
                return Err(TranscribeError::RunTimeSpent {
                    has_progressed: self.has_progressed,
                });
            }
            let started = Instant::now();
            match send(self.provider, audio) {
                Ok(reply) => {
                    tracing::info!(
                        mc.stt.try_number = try_number,
                        mc.stt.upload_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "request answered"
                    );
                    return Ok(reply);
                }
                Err(error) => match error.class() {
                    FailureClass::Passing => {
                        tracing::warn!(
                            exception.message = %error,
                            mc.stt.try_number = try_number,
                            "request failed on the network"
                        );
                        let Some(wait) = RETRY_WAITS.get(try_number - 1) else {
                            return Err(TranscribeError::Unreachable {
                                source: error,
                                has_progressed: self.has_progressed,
                            });
                        };
                        std::thread::sleep(*wait);
                        try_number += 1;
                    }
                    FailureClass::Refused | FailureClass::Local => return Err(error.into()),
                },
            }
        }
    }

    /// The transcript reply for `stem` over `span`, leaving out the `excluded` spans, saved as
    /// `w<window>-<stem>.json`.
    pub(super) fn window_reply(
        &mut self,
        audio: &CallAudio,
        window_index: usize,
        stem: Stem,
        span: CallSpan,
        excluded: &[CallSpan],
    ) -> Result<Option<Answered>, TranscribeError> {
        let voices = voices_of(stem);
        let request_key = self.provider.request_key(voices);
        let cut = audio.cut(stem, span, excluded);
        let window_s = span.to_s - span.from_s;
        let file_name = window_file_name(window_index, stem);
        let kind = self.provider.kind();
        let answered = self.reply(
            &file_name,
            &request_key,
            cut,
            |provider, encoded| provider.transcribe(encoded, voices),
            |reply| kind.utterances(reply).map(drop),
        )?;
        if let Some(answered) = &answered {
            tracing::info!(
                file.name = file_name,
                mc.stt.audio_s = answered.cut.seconds(),
                mc.stt.window_s = window_s,
                "window audio sent or reused"
            );
        }
        Ok(answered)
    }

    /// The language check: up to two 30 s clips per stem with the most and loudest speech,
    /// sent in the provider's multilingual mode. `None` per stem with no speech to send.
    pub(super) fn language_check(
        &mut self,
        audio: &CallAudio,
    ) -> Result<Vec<StemLanguage>, TranscribeError> {
        let mut stems = Vec::new();
        for stem in [Stem::Mic, Stem::Remote] {
            let spans = audio.loudest_clips(stem);
            let Some(cut) = spans
                .iter()
                .map(|span| audio.cut(stem, *span, &[]))
                .reduce(AudioCut::join)
            else {
                continue;
            };
            let voices = voices_of(stem);
            let request_key = self.provider.language_check_key(voices);
            let file_name = clips_file_name(stem);
            let kind = self.provider.kind();
            // The transcript reads a clip reply's utterances too.
            let Some(answered) = self.reply(
                &file_name,
                &request_key,
                cut,
                |provider, encoded| provider.language_check(encoded, voices),
                |reply| {
                    kind.utterances(reply)?;
                    kind.word_languages(reply).map(drop)
                },
            )?
            else {
                continue;
            };
            let words = self.provider.kind().word_languages(&answered.reply)?;
            stems.push(StemLanguage::read(stem, spans, &answered, &words));
        }
        Ok(stems)
    }
}

/// What the language check heard on one stem.
pub(super) struct StemLanguage {
    pub stem: Stem,
    /// The clips sent, with their own counts.
    pub clips: Vec<ClipLanguage>,
    pub word_count: usize,
    pub non_english_count: usize,
}

pub(super) struct ClipLanguage {
    pub span: CallSpan,
    pub word_count: usize,
    pub non_english_count: usize,
}

impl ClipLanguage {
    /// Whether under the fallback share of the clip's words are not English.
    pub(super) fn is_mostly_english(&self) -> bool {
        share(self.non_english_count, self.word_count) < MIN_NON_ENGLISH_SHARE
    }
}

impl StemLanguage {
    fn read(
        stem: Stem,
        spans: Vec<CallSpan>,
        answered: &Answered,
        words: &[stt::TaggedWord],
    ) -> Self {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut clips: Vec<ClipLanguage> = spans
            .into_iter()
            .map(|span| ClipLanguage {
                span,
                word_count: 0,
                non_english_count: 0,
            })
            .collect();
        for word in words {
            *counts.entry(word.language.as_str()).or_default() += 1;
            let call_s = answered.cut.call_s(word.start_s);
            let is_english = word.language == ENGLISH_TAG;
            if let Some(clip) = clips.iter_mut().find(|clip| clip.span.contains(call_s)) {
                clip.word_count += 1;
                clip.non_english_count += usize::from(!is_english);
            }
        }
        let word_count = words.len();
        let non_english_count = words
            .iter()
            .filter(|word| word.language != ENGLISH_TAG)
            .count();
        // Deepgram can tag Hindi as another Indian language, so every count is logged.
        let language_counts = counts
            .iter()
            .map(|(language, count)| format!("{language}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            mc.stt.stem = stem.as_str(),
            mc.stt.word_count = word_count,
            mc.stt.non_english_share = share(non_english_count, word_count),
            mc.stt.language_counts = %language_counts,
            "language check read"
        );
        Self {
            stem,
            clips,
            word_count,
            non_english_count,
        }
    }

    /// Whether this stem makes the call fall back.
    pub(super) fn is_mostly_english(&self) -> bool {
        self.word_count < MIN_LANGUAGE_WORDS
            || share(self.non_english_count, self.word_count) < MIN_NON_ENGLISH_SHARE
    }
}

/// The saved transcript reply for `stem` in window `window_index`.
pub(super) fn window_file_name(window_index: usize, stem: Stem) -> String {
    format!("w{window_index:03}-{}.json", stem.as_str())
}

/// The saved clips reply for `stem`, as the language check left it.
pub(super) fn clips_file_name(stem: Stem) -> String {
    format!("clips-{}.json", stem.as_str())
}

/// A saved reply's audio and reply, with no request: for a finished step.
pub(super) fn saved_answer(folder: &Path, file_name: &str) -> Result<Answered, TranscribeError> {
    let path = stt_dir(folder).join(file_name);
    let saved = read_json::<SavedReply>(&path)?.ok_or(TranscribeError::StaleReplies)?;
    if !saved.cut.has_current_sizes(folder)
        || samples_sha256(&saved.cut.samples(folder)?) != saved.samples_sha256
    {
        return Err(TranscribeError::StaleReplies);
    }
    Ok(Answered {
        cut: saved.cut,
        reply: saved.reply,
    })
}

pub(super) fn voices_of(stem: Stem) -> StemVoices {
    match stem {
        Stem::Mic => StemVoices::OnePerson,
        Stem::Remote => StemVoices::Several,
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "word counts stay far below 2^52"
)]
fn share(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 / whole as f64
}
