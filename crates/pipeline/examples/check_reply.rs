//! Checks a recorded reply against a numbered transcript, without a meeting in the job store.
//! Prints each `reply error` a repair turn would send, then one line per citation:
//! `<item path> <cited id> -> <segment id> <verdict>`, or `failed`. Exits 1 when the reply does
//! not parse or fails as the last reply.
//!
//! Usage: `cargo run -p pipeline --example check_reply -- <transcript.json> <reply.json>`
//! The transcript is a JSON array of `{id, speaker, start_s, text}`, ids 1, 2, 3 and so on.

use std::collections::HashMap;

use anyhow::{Context, bail};
use pipeline::extraction::{self, Prompt, Turn, Verdict};
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
    let transcript = read_transcript(transcript_path)?;
    let prompt = Prompt::new(&transcript, Some("Ujjwal Kumar"), &[], &[]);
    let reply =
        std::fs::read_to_string(reply_path).with_context(|| format!("reading {reply_path}"))?;
    // A repair turn lists every problem, missing quotes included once the rest is clean.
    if let Verdict::Repair { errors } = extraction::judge(&reply, &prompt, Turn::CanRepair) {
        for error in errors {
            println!("reply error: {error}");
        }
    }
    let last_errors = match extraction::judge(&reply, &prompt, Turn::Last) {
        Verdict::Accepted { .. } => None,
        Verdict::Repair { errors } | Verdict::Rejected { errors, .. } => Some(errors),
    };
    // The trace runs on a rejected reply too, since a rejection is when it explains the most.
    let (notes, checked) = extraction::check_quotes(&reply, &prompt).map_err(anyhow::Error::msg)?;

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
    if let Some(errors) = last_errors {
        bail!("rejected as the last reply: {}", errors.join("; "));
    }
    Ok(())
}

fn read_transcript(transcript_path: &str) -> anyhow::Result<Transcript> {
    let segments: Vec<NumberedSegment> = serde_json::from_slice(
        &std::fs::read(transcript_path).with_context(|| format!("reading {transcript_path}"))?,
    )?;
    let started_at = jiff::Timestamp::UNIX_EPOCH;
    Ok(Transcript {
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
    })
}
