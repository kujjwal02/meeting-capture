//! Checks a recorded reply against a numbered transcript, without a meeting in the job store.
//! Prints one line per citation: `<item path> <cited id> -> <segment id> <verdict>`, or `failed`.
//!
//! Usage: `cargo run -p pipeline --example check_reply -- <transcript.json> <reply.json>`
//! The transcript is a JSON array of `{id, speaker, start_s, text}`, ids 1, 2, 3 and so on.

use std::collections::HashMap;

use anyhow::{Context, bail};
use pipeline::extraction::{self, Prompt};
use serde::Deserialize;
use sources::{Platform, Segment, Source, SpeakerNaming, Transcript};

#[derive(Deserialize)]
struct NumberedSegment {
    speaker: String,
    start_s: f64,
    text: String,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, transcript_path, reply_path] = &args[..] else {
        bail!("usage: check_reply <transcript.json> <reply.json>");
    };
    let segments: Vec<NumberedSegment> = serde_json::from_slice(
        &std::fs::read(transcript_path).with_context(|| format!("reading {transcript_path}"))?,
    )?;
    let started_at = jiff::Timestamp::UNIX_EPOCH;
    let transcript = Transcript {
        source: Source::Transcriptonic,
        started_at,
        ended_at: started_at,
        platform: Platform::Meet,
        meeting_url: None,
        title: None,
        speaker_naming: SpeakerNaming::Real,
        // TranscripTonic's label for the local user.
        my_speaker: Some("You".to_owned()),
        attendees: Vec::new(),
        segments: segments
            .into_iter()
            .map(|segment| Segment {
                start_s: segment.start_s,
                end_s: segment.start_s,
                speaker: segment.speaker,
                text: segment.text,
            })
            .collect(),
        chat_messages: Vec::new(),
        slack_huddle: None,
    };
    let prompt = Prompt::new(&transcript, Some("Ujjwal Kumar"), &[], &[]);
    let reply =
        std::fs::read_to_string(reply_path).with_context(|| format!("reading {reply_path}"))?;
    let notes = extraction::parse_reply(&reply).map_err(anyhow::Error::msg)?;
    print_reply_errors(&notes, &prompt);

    // Tag every citation with its path, so dropped ones can be told apart after the check.
    let mut cited: Vec<(String, u32, String)> = Vec::new();
    let mut tag = |field: &str, index: usize, citations: &[extraction::Citation]| {
        for (c_index, citation) in citations.iter().enumerate() {
            cited.push((
                format!("{field}[{index}][{c_index}]"),
                citation.segment_id,
                citation.quote.clone(),
            ));
        }
    };
    notes
        .decisions
        .iter()
        .enumerate()
        .for_each(|(i, item)| tag("decisions", i, &item.citations));
    notes
        .action_items
        .iter()
        .enumerate()
        .for_each(|(i, item)| tag("action_items", i, &item.citations));
    notes
        .open_questions
        .iter()
        .enumerate()
        .for_each(|(i, item)| tag("open_questions", i, &item.citations));
    notes
        .my_commitments
        .iter()
        .enumerate()
        .for_each(|(i, item)| tag("my_commitments", i, &item.citations));

    let checked = extraction::check_citations(notes, &prompt);
    let mut found: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    let all_checked = checked
        .decisions
        .iter()
        .flat_map(|item| &item.citations)
        .chain(checked.action_items.iter().flat_map(|item| &item.citations))
        .chain(
            checked
                .open_questions
                .iter()
                .flat_map(|item| &item.citations),
        )
        .chain(
            checked
                .my_commitments
                .iter()
                .flat_map(|item| &item.citations),
        );
    for citation in all_checked {
        found.entry(citation.quote.clone()).or_default().push((
            citation.segment_id,
            serde_json::to_value(citation.verdict)?
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ));
    }
    for (path, segment_id, quote) in cited {
        match found.get_mut(&quote).and_then(Vec::pop) {
            Some((moved_to, verdict)) => println!("{path} {segment_id} -> {moved_to} {verdict}"),
            None => println!("{path} {segment_id} failed"),
        }
    }
    Ok(())
}

fn print_reply_errors(notes: &extraction::MeetingNotes, prompt: &Prompt) {
    let mut errors = extraction::reply_errors(notes, prompt);
    errors.extend(extraction::caption_fix_errors(notes, prompt));
    for error in errors {
        println!("reply error: {error}");
    }
}
