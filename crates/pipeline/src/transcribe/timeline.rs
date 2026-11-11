//! The hybrid transcript: caption lines where the captions held, speech-to-text lines in the
//! windows sent. Names the remote speakers from the captions by time overlap, and drops the
//! mic's words that are the remote speakers heard through the user's speakers.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use sources::{Segment, SpeakerNaming, Transcript};
use stem_timing::Stem;
use stt::{Utterance, Word};

use super::audio::CallSpan;
use super::checks;

/// A mic word within this many seconds of the same remote word is that word heard again.
const MAX_ECHO_OFFSET_S: f64 = 0.4;
/// Speech-to-text lines this much wider on each side still count as overlapping a caption
/// line, since the two clocks place a line a little apart.
const SPEAKER_OVERLAP_SLACK_S: f64 = 1.0;

/// The utterances of one request inside one window, on the call's clock.
pub(super) struct SpeechPiece {
    pub stem: Stem,
    /// Tells apart the requests whose speaker numbers are their own.
    pub request: usize,
    pub window_index: usize,
    pub utterances: Vec<Utterance>,
}

/// The provider's speaker within one request: the request, then its speaker number.
type SpeakerKey = (usize, Option<u32>);

/// How long a remote speaker's lines overlap each caption speaker's, in one window.
struct SpeakerOverlap<'a> {
    window_index: usize,
    seconds_by_name: BTreeMap<&'a str, f64>,
}

/// How many of each the assembly made or dropped, for the logs.
pub(super) struct HybridCounts {
    pub caption_lines: usize,
    pub speech_lines: usize,
    pub echo_words: usize,
    pub named_speakers: usize,
    pub numbered_speakers: usize,
}

/// One speech-to-text line before its speaker is named.
struct SpeechLine {
    stem: Stem,
    speaker_key: SpeakerKey,
    window_index: usize,
    utterance: Utterance,
}

/// The captions with their lines in `sent` replaced by the speech in `pieces`. Title,
/// attendees and chat stay from the captions.
pub(super) fn hybrid_transcript(
    captions: &Transcript,
    my_name: Option<&str>,
    sent: &[CallSpan],
    pieces: Vec<SpeechPiece>,
) -> (Transcript, HybridCounts) {
    let my_label = captions
        .my_speaker
        .clone()
        .or_else(|| my_name.map(str::to_owned))
        .unwrap_or_else(|| "Me".to_owned());
    let kept: Vec<Segment> = captions
        .segments
        .iter()
        .flat_map(|segment| outside_sent(segment, sent))
        .collect();
    let mut lines: Vec<SpeechLine> = pieces
        .into_iter()
        .flat_map(|piece| {
            piece
                .utterances
                .into_iter()
                .map(move |utterance| SpeechLine {
                    stem: piece.stem,
                    speaker_key: (piece.request, utterance.speaker),
                    window_index: piece.window_index,
                    utterance,
                })
        })
        .collect();
    let echo_words = drop_echo(&mut lines);
    let names = speaker_names(captions, &my_label, &lines);
    let numbered_speakers = names
        .values()
        .filter(|name| matches!(name, SpeakerName::Numbered(_)))
        .count();
    let named_speakers = names.len() - numbered_speakers;

    let caption_lines = kept.len();
    let speech_lines = lines.len();
    let speech_end_s = lines
        .iter()
        .map(|line| line.utterance.end_s)
        .fold(0.0, f64::max);
    let mut segments = kept;
    segments.extend(lines.into_iter().map(|line| Segment {
        start_s: line.utterance.start_s,
        end_s: line.utterance.end_s,
        speaker: match line.stem {
            Stem::Mic => my_label.clone(),
            Stem::Remote => match names.get(&line.speaker_key) {
                Some(SpeakerName::Caption(name)) => name.clone(),
                Some(SpeakerName::Numbered(number)) => format!("Speaker {number}"),
                None => "Speaker".to_owned(),
            },
        },
        text: line.utterance.text,
    }));
    segments.sort_by(|first, second| first.start_s.total_cmp(&second.start_s));

    let transcript = Transcript {
        source: if caption_lines == 0 {
            sources::Source::Recorder
        } else {
            captions.source
        },
        ended_at: captions.ended_at.max(call_time(captions, speech_end_s)),
        speaker_naming: if numbered_speakers > 0 {
            SpeakerNaming::Diarized
        } else {
            captions.speaker_naming
        },
        my_speaker: Some(my_label),
        segments,
        ..captions.clone()
    };
    let counts = HybridCounts {
        caption_lines,
        speech_lines,
        echo_words,
        named_speakers,
        numbered_speakers,
    };
    (transcript, counts)
}

/// The parts of a caption line outside the windows sent. A line has no word times, so its words
/// are spread evenly over it, and each goes by its middle, the rule speech words follow too: a
/// line across a window edge keeps the words on the captions' side, with its times clipped.
fn outside_sent(segment: &Segment, sent: &[CallSpan]) -> Vec<Segment> {
    let is_sent = |call_s: f64| sent.iter().any(|span| span.contains(call_s));
    let crosses_sent = sent
        .iter()
        .any(|span| span.from_s <= segment.end_s && segment.start_s < span.to_s);
    if !crosses_sent {
        return vec![segment.clone()];
    }
    let words: Vec<&str> = segment.text.split_whitespace().collect();
    if words.is_empty() {
        let middle_s = f64::midpoint(segment.start_s, segment.end_s);
        return if is_sent(middle_s) {
            Vec::new()
        } else {
            vec![segment.clone()]
        };
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a caption line has far fewer than 2^52 words"
    )]
    let seconds_per_word = (segment.end_s - segment.start_s).max(0.0) / words.len() as f64;
    #[expect(
        clippy::cast_precision_loss,
        reason = "a caption line has far fewer than 2^52 words"
    )]
    let word_start_s = |index: usize| segment.start_s + seconds_per_word * index as f64;
    let mut parts = Vec::new();
    let mut run_start: Option<usize> = None;
    for index in 0..=words.len() {
        let is_kept = index < words.len() && !is_sent(word_start_s(index) + seconds_per_word / 2.0);
        match (run_start, is_kept) {
            (None, true) => run_start = Some(index),
            (Some(first), false) => {
                parts.push(Segment {
                    start_s: word_start_s(first),
                    end_s: word_start_s(index),
                    speaker: segment.speaker.clone(),
                    text: words[first..index].join(" "),
                });
                run_start = None;
            }
            (None, false) | (Some(_), true) => {}
        }
    }
    parts
}

/// The utterance's words in each window of `sent`, as one utterance per window, by the middle
/// of each word. Words in no window sent are dropped: the captions there stay. An utterance
/// without word times goes whole by its middle.
pub(super) fn split_by_window(utterance: Utterance, sent: &[CallSpan]) -> Vec<(usize, Utterance)> {
    let window_of = |call_s: f64| sent.iter().position(|span| span.contains(call_s));
    if utterance.words.is_empty() {
        let middle_s = f64::midpoint(utterance.start_s, utterance.end_s);
        return window_of(middle_s)
            .map(|window_index| (window_index, utterance))
            .into_iter()
            .collect();
    }
    let mut by_window: Vec<(usize, Vec<Word>)> = Vec::new();
    for word in utterance.words {
        let Some(window_index) = window_of(centre_s(&word)) else {
            continue;
        };
        match by_window.last_mut() {
            Some((last_index, words)) if *last_index == window_index => words.push(word),
            Some(_) | None => by_window.push((window_index, vec![word])),
        }
    }
    by_window
        .into_iter()
        .filter_map(|(window_index, words)| {
            let start_s = words.first()?.start_s;
            let end_s = words.last()?.end_s;
            let text = words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Some((
                window_index,
                Utterance {
                    start_s,
                    end_s,
                    speaker: utterance.speaker,
                    text,
                    words,
                },
            ))
        })
        .collect()
}

/// The moment `call_s` seconds after the call's start.
fn call_time(captions: &Transcript, call_s: f64) -> Timestamp {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a call lasts far less than i64 nanoseconds"
    )]
    let length = SignedDuration::from_nanos((call_s * 1e9) as i64);
    captions
        .started_at
        .checked_add(length)
        .unwrap_or(captions.ended_at)
}

enum SpeakerName {
    Caption(String),
    /// Numbered within its window: the provider cannot tell the same voice across requests.
    Numbered(u32),
}

/// A name per remote speaker of each request: the caption speaker whose lines overlap theirs
/// most, else the sole other attendee, else a number within the window.
fn speaker_names(
    captions: &Transcript,
    my_label: &str,
    lines: &[SpeechLine],
) -> BTreeMap<SpeakerKey, SpeakerName> {
    let self_names: Vec<&str> = captions
        .attendees
        .iter()
        .filter(|attendee| attendee.is_self)
        .map(|attendee| attendee.name.as_str())
        .chain([my_label])
        .collect();
    let others: Vec<&Segment> = captions
        .segments
        .iter()
        .filter(|segment| !self_names.contains(&segment.speaker.as_str()))
        .filter(|segment| checks::is_speech(&segment.text))
        .collect();
    let mut other_attendees = captions
        .attendees
        .iter()
        .filter(|attendee| !attendee.is_self);
    let sole_other = match (other_attendees.next(), other_attendees.next()) {
        (Some(other), None) => Some(other.name.clone()),
        (Some(_), Some(_)) | (None, _) => None,
    };

    let mut overlaps: BTreeMap<SpeakerKey, SpeakerOverlap<'_>> = BTreeMap::new();
    for line in lines.iter().filter(|line| line.stem == Stem::Remote) {
        let entry = overlaps
            .entry(line.speaker_key)
            .or_insert_with(|| SpeakerOverlap {
                window_index: line.window_index,
                seconds_by_name: BTreeMap::new(),
            });
        let from_s = line.utterance.start_s - SPEAKER_OVERLAP_SLACK_S;
        let to_s = line.utterance.end_s + SPEAKER_OVERLAP_SLACK_S;
        for segment in &others {
            let shared_s = to_s.min(segment.end_s) - from_s.max(segment.start_s);
            if shared_s > 0.0 {
                *entry
                    .seconds_by_name
                    .entry(segment.speaker.as_str())
                    .or_default() += shared_s;
            }
        }
    }
    let mut next_number: BTreeMap<usize, u32> = BTreeMap::new();
    overlaps
        .into_iter()
        .map(|(key, overlap)| {
            let best = overlap
                .seconds_by_name
                .into_iter()
                .max_by(|first, second| first.1.total_cmp(&second.1))
                .map(|(name, _)| name.to_owned());
            let name = match (best, &sole_other) {
                (Some(name), _) => SpeakerName::Caption(name),
                (None, Some(name)) => SpeakerName::Caption(name.clone()),
                (None, None) => {
                    let number = next_number.entry(overlap.window_index).or_insert(0);
                    *number += 1;
                    SpeakerName::Numbered(*number)
                }
            };
            (key, name)
        })
        .collect()
}

/// Removes the mic's words that a remote speaker said at the same moment, heard again through
/// the user's speakers. A lone match among the user's own words stays: it is more likely the
/// user saying the same common word. Returns how many words went.
fn drop_echo(lines: &mut Vec<SpeechLine>) -> usize {
    let mut remote: Vec<(f64, String)> = lines
        .iter()
        .filter(|line| line.stem == Stem::Remote)
        .flat_map(|line| &line.utterance.words)
        .map(|word| (centre_s(word), normalised(&word.text)))
        .filter(|(_, text)| !text.is_empty())
        .collect();
    remote.sort_by(|first, second| first.0.total_cmp(&second.0));
    let is_heard = |word: &Word| {
        let centre = centre_s(word);
        let text = normalised(&word.text);
        let first = remote.partition_point(|(other_s, _)| *other_s < centre - MAX_ECHO_OFFSET_S);
        remote[first..]
            .iter()
            .take_while(|(other_s, _)| *other_s <= centre + MAX_ECHO_OFFSET_S)
            .any(|(_, other)| *other == text)
    };
    let mut dropped = 0;
    for line in lines.iter_mut().filter(|line| line.stem == Stem::Mic) {
        let words = &line.utterance.words;
        if words.is_empty() {
            continue;
        }
        let heard: Vec<bool> = words.iter().map(is_heard).collect();
        let is_echo: Vec<bool> = (0..words.len())
            .map(|index| {
                heard[index]
                    && (words.len() == 1
                        || (index > 0 && heard[index - 1])
                        || heard.get(index + 1).copied().unwrap_or(false))
            })
            .collect();
        let echo_count = is_echo.iter().filter(|is_echo| **is_echo).count();
        if echo_count == 0 {
            continue;
        }
        dropped += echo_count;
        let kept: Vec<Word> = words
            .iter()
            .zip(&is_echo)
            .filter(|(_, is_echo)| !**is_echo)
            .map(|(word, _)| word.clone())
            .collect();
        if let (Some(first), Some(last)) = (kept.first(), kept.last()) {
            line.utterance.start_s = first.start_s;
            line.utterance.end_s = last.end_s;
        }
        line.utterance.text = kept
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        line.utterance.words = kept;
    }
    lines.retain(|line| !line.utterance.text.trim().is_empty());
    dropped
}

fn centre_s(word: &Word) -> f64 {
    f64::midpoint(word.start_s, word.end_s)
}

fn normalised(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
