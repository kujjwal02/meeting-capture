//! The caption checks that cost nothing: too few lines, failed, and in another language, for
//! the whole call or one window of it.

use jiff::SignedDuration;
use jobs::InputKind;
use sources::Transcript;
use sources::slack_huddle::HuddleHealth;

use super::FallbackReason;
use crate::InputError;
use crate::intake::MIN_CAPTION_SEGMENTS;

/// Slack writes these into a huddle's captions as the person's line. They are no speech.
const HUDDLE_EVENT_TEXTS: [&str; 4] = [
    "joined the huddle.",
    "left the huddle.",
    "started screen sharing.",
    "stopped screen sharing.",
];

/// Whether a caption line is speech, not a Slack huddle event.
pub(super) fn is_speech(text: &str) -> bool {
    !HUDDLE_EVENT_TEXTS.contains(&text.trim())
}

/// Meet reported captions unavailable for longer than this...
const MIN_REFUSAL: SignedDuration = SignedDuration::from_mins(2);
/// ...and fewer caption events than this arrived per minute of it. Working captions send
/// hundreds per minute, as Meet often reports captions unavailable while its data channels
/// keep sending them.
const MAX_CAPTION_EVENTS_PER_REFUSED_MIN: f64 = 2.0;

/// Over this share of caption letters outside the Latin script means another language.
const MAX_NON_LATIN_SHARE: f64 = 0.2;

/// The first call-level check the captions fail, or `None` when they pass all of them.
pub(super) fn first_failed(
    kind: InputKind,
    log: &[u8],
    captions: &Transcript,
) -> Result<Option<FallbackReason>, InputError> {
    if captions.segments.is_empty() {
        return Ok(Some(FallbackReason::Empty));
    }
    if captions.segments.len() < MIN_CAPTION_SEGMENTS {
        return Ok(Some(FallbackReason::TooFew));
    }
    if have_failed(kind, log, captions)? {
        return Ok(Some(FallbackReason::Failed));
    }
    let lines: Vec<&str> = captions
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .filter(|text| is_speech(text))
        .collect();
    if is_other_language(&lines) {
        return Ok(Some(FallbackReason::OtherLanguage));
    }
    Ok(None)
}

fn have_failed(kind: InputKind, log: &[u8], captions: &Transcript) -> Result<bool, InputError> {
    match kind {
        InputKind::BrowserCallLog => {
            let lines = sources::browser_call::read_lines(log)?;
            let refusal = sources::browser_call::caption_refusal(&lines);
            let refused_min = refusal.refused_for.as_secs_f64() / 60.0;
            #[expect(
                clippy::cast_precision_loss,
                reason = "a call has far fewer than 2^52 caption events"
            )]
            let events_per_min = refusal.caption_event_count as f64 / refused_min.max(1.0);
            tracing::debug!(
                mc.captions.refused_s = refusal.refused_for.as_secs(),
                mc.captions.refused_event_count = refusal.caption_event_count,
                "meet caption refusal"
            );
            Ok(refusal.refused_for > MIN_REFUSAL
                && events_per_min < MAX_CAPTION_EVENTS_PER_REFUSED_MIN)
        }
        InputKind::SlackHuddleLog => {
            Ok(captions
                .slack_huddle
                .as_ref()
                .is_some_and(|huddle| match huddle.health {
                    HuddleHealth::CaptionsOff | HuddleHealth::Broken => true,
                    HuddleHealth::Healthy | HuddleHealth::Untested | HuddleHealth::Degraded => {
                        false
                    }
                }))
        }
        // TranscripTonic has no call folder, so it never reaches the checks.
        InputKind::TranscriptonicPayload => Ok(false),
    }
}

/// Whether `lines` are mostly outside the Latin script, or read reliably as a language other
/// than English.
pub(super) fn is_other_language(lines: &[&str]) -> bool {
    let text = lines.join("\n");
    let (letter_count, non_latin_count) = text
        .chars()
        .filter(|character| character.is_alphabetic())
        .fold((0_u32, 0_u32), |(letters, non_latin), character| {
            (
                letters.saturating_add(1),
                non_latin.saturating_add(u32::from(!is_latin(character))),
            )
        });
    let non_latin_share = if letter_count == 0 {
        0.0
    } else {
        f64::from(non_latin_count) / f64::from(letter_count)
    };
    let detected = whatlang::detect(&text);
    let is_reliably_other = detected
        .as_ref()
        .is_some_and(|info| info.is_reliable() && info.lang() != whatlang::Lang::Eng);
    tracing::debug!(
        mc.captions.non_latin_share = non_latin_share,
        mc.captions.language = detected
            .as_ref()
            .map_or("unknown", |info| info.lang().code()),
        mc.captions.language_is_reliable = detected.is_some_and(|info| info.is_reliable()),
        "caption language"
    );
    non_latin_share > MAX_NON_LATIN_SHARE || is_reliably_other
}

// Basic Latin, Latin-1, Latin Extended-A and -B, and Latin Extended Additional.
fn is_latin(character: char) -> bool {
    character.is_ascii_alphabetic()
        || ('\u{00C0}'..='\u{024F}').contains(&character)
        || ('\u{1E00}'..='\u{1EFF}').contains(&character)
}
