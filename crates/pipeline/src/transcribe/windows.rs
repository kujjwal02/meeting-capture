//! Which 5-minute windows of the call go to speech-to-text. Free checks first: a window whose
//! captions are sparse or in another language goes. A call-level flag (the language check, the
//! LLM check, or failed captions) sends every window except those with dense captions whose own
//! language-check clip heard English.

use serde::{Deserialize, Serialize};
use sources::Transcript;

use super::FallbackReason;
use super::audio::{CallAudio, CallSpan};
use super::checks;
use super::requests::StemLanguage;

/// Fewer caption words than this per minute of speech means the captions dropped speech.
/// Calls with working captions measured 42 to 216.
pub(super) const MIN_CAPTION_WORDS_PER_VOICED_MIN: f64 = 30.0;
/// A window over this many caption words per minute of speech keeps its captions under a
/// call-level flag, unless its own language-check clip heard other languages.
const MIN_TRUSTED_WORDS_PER_VOICED_MIN: f64 = 100.0;
/// A window with less speech than this has nothing to transcribe.
const MIN_VOICED_MIN: f64 = 0.1;

/// One window and what the checks decided for it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct WindowPlan {
    pub index: usize,
    pub span: CallSpan,
    pub caption_words: usize,
    pub voiced_min: f64,
    /// Why the window goes to speech-to-text, or `None` when its captions stay.
    pub send: Option<FallbackReason>,
}

impl WindowPlan {
    /// Whether the window holds enough speech to be worth a request.
    pub(super) fn has_speech(&self) -> bool {
        self.voiced_min >= MIN_VOICED_MIN
    }

    pub(super) fn words_per_voiced_min(&self) -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "word counts stay far below 2^52"
        )]
        let words = self.caption_words as f64;
        words / self.voiced_min.max(MIN_VOICED_MIN)
    }
}

/// Every window of the call, decided with `call_flag` and the language check's clips.
pub(super) fn plan(
    captions: &Transcript,
    audio: &CallAudio,
    call_flag: Option<FallbackReason>,
    languages: &[StemLanguage],
) -> Vec<WindowPlan> {
    let caption_end_s = captions
        .segments
        .iter()
        .map(|segment| segment.end_s)
        .fold(0.0, f64::max);
    let end_s = audio.end_s().max(caption_end_s);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a call has far fewer windows than usize holds, and end_s is not negative"
    )]
    let window_count = (end_s / super::audio::WINDOW_S).ceil().max(1.0) as usize;
    let audio_start_s = audio.start_s();
    (0..window_count)
        .map(|index| {
            let span = CallSpan::window(index, audio_start_s);
            let lines: Vec<&str> = captions
                .segments
                .iter()
                // By the middle, as the transcript keeps or drops a line.
                .filter(|segment| span.contains(f64::midpoint(segment.start_s, segment.end_s)))
                .map(|segment| segment.text.as_str())
                .filter(|text| checks::is_speech(text))
                .collect();
            let caption_words = lines
                .iter()
                .map(|line| line.split_whitespace().count())
                .sum();
            let mut window = WindowPlan {
                index,
                span,
                caption_words,
                voiced_min: audio.voiced_minutes(span),
                send: None,
            };
            window.send = decide(&window, &lines, call_flag, languages);
            window
        })
        .collect()
}

fn decide(
    window: &WindowPlan,
    lines: &[&str],
    call_flag: Option<FallbackReason>,
    languages: &[StemLanguage],
) -> Option<FallbackReason> {
    if !window.has_speech() {
        return None;
    }
    let words_per_min = window.words_per_voiced_min();
    match call_flag {
        // No captions to keep, or the user asked for all of it.
        Some(flag @ (FallbackReason::Empty | FallbackReason::TooFew | FallbackReason::Forced)) => {
            return Some(flag);
        }
        Some(
            FallbackReason::Failed
            | FallbackReason::Sparse
            | FallbackReason::OtherLanguage
            | FallbackReason::NonEnglishSpeech
            | FallbackReason::Garbled,
        )
        | None => {}
    }
    if words_per_min < MIN_CAPTION_WORDS_PER_VOICED_MIN {
        return Some(FallbackReason::Sparse);
    }
    if checks::is_other_language(lines) {
        return Some(FallbackReason::OtherLanguage);
    }
    let flag = call_flag?;
    let own_clips_are_english = languages
        .iter()
        .flat_map(|language| &language.clips)
        .filter(|clip| clip.span.overlaps(window.span))
        .all(super::requests::ClipLanguage::is_mostly_english);
    let is_trusted = words_per_min > MIN_TRUSTED_WORDS_PER_VOICED_MIN && own_clips_are_english;
    (!is_trusted).then_some(flag)
}
