//! The verdict on one reply: the reply check, the repair rules for its turn, and the quote check.

use super::citations;
use super::prompt::Prompt;
use super::{CaptionFix, CheckedNotes, Citation, MeetingNotes};

// Long enough for any real summary; a longer one means the model is writing an essay.
const MAX_SUMMARY_CHARS: usize = 1500;
// 15 words of long Hinglish words still fit.
const MAX_QUOTE_CHARS: usize = 200;

/// Whether the model gets another turn to fix this reply.
#[derive(Debug, Clone, Copy)]
pub enum Turn {
    /// A repair turn can follow, so every fixable error goes back to the model.
    CanRepair,
    /// No turn follows: bad caption fixes and quotes not in the transcript are dropped instead.
    Last,
}

/// What to do with one reply.
#[derive(Debug)]
pub enum Verdict {
    /// The notes, with every quote checked. `dropped_caption_fixes` holds the error of each bad
    /// caption fix a `Turn::Last` reply lost.
    Accepted {
        notes: CheckedNotes,
        dropped_caption_fixes: Vec<String>,
    },
    /// Send these errors back for another try. Only a `Turn::CanRepair` reply gets this.
    Repair { errors: Vec<String> },
    /// No notes from this reply and no further turn, whatever the turn was.
    /// `dropped_caption_fixes` is as for `Accepted`.
    Rejected {
        errors: Vec<String>,
        dropped_caption_fixes: Vec<String>,
    },
}

/// One reply, parsed or not.
pub(crate) enum Reply {
    Parsed(MeetingNotes),
    /// Why the reply is not a `MeetingNotes`, with the path of the field.
    Unparsed(String),
}

/// Judges one reply text against the prompt, by the rules for this turn.
#[must_use]
pub fn judge(reply: &str, prompt: &Prompt, turn: Turn) -> Verdict {
    Reply::read(reply).judge(prompt, turn)
}

/// Reads one reply text and checks its quotes, with no verdict. Returns the notes as the model
/// sent them and as checked, so a debugging tool can trace each citation, even in a reply that
/// `judge` rejects.
///
/// # Errors
///
/// Returns why the reply is not a `MeetingNotes`, with the path of the field.
pub fn check_quotes(reply: &str, prompt: &Prompt) -> Result<(MeetingNotes, CheckedNotes), String> {
    match Reply::read(reply) {
        Reply::Parsed(notes) => Ok((notes.clone(), citations::check(notes, prompt))),
        Reply::Unparsed(error) => Err(error),
    }
}

impl Reply {
    /// Reads a reply that should be one JSON object. Models that cannot be held to a schema wrap
    /// it in a code fence or a sentence, so the text outside the outermost braces is ignored.
    pub(crate) fn read(reply: &str) -> Self {
        let object = match (reply.find('{'), reply.rfind('}')) {
            (Some(start), Some(end)) if start < end => &reply[start..=end],
            _ => reply,
        };
        Self::deserialize(&mut serde_json::Deserializer::from_str(object))
    }

    /// Reads a reply that the provider already decoded as JSON.
    pub(crate) fn from_value(reply: serde_json::Value) -> Self {
        Self::deserialize(reply)
    }

    fn deserialize<'de>(deserializer: impl serde::Deserializer<'de>) -> Self {
        match serde_path_to_error::deserialize(deserializer) {
            Ok(notes) => Self::Parsed(notes),
            Err(error) => Self::Unparsed(format!("{}: {}", error.path(), error.inner())),
        }
    }

    /// Judges this reply against the prompt, by the rules for this turn.
    pub(crate) fn judge(self, prompt: &Prompt, turn: Turn) -> Verdict {
        let mut dropped_caption_fixes = Vec::new();
        let errors = match self {
            Self::Parsed(mut notes) => {
                let mut errors = reply_errors(&notes, prompt);
                match turn {
                    Turn::CanRepair => {
                        errors.extend(caption_fix_errors(&notes, prompt));
                        if errors.is_empty() {
                            errors = citations::missing_quotes(&notes, prompt);
                        }
                    }
                    // The notes stand without a bad caption fix, so the last reply drops it
                    // rather than fail.
                    Turn::Last => {
                        dropped_caption_fixes = drop_bad_caption_fixes(&mut notes, prompt);
                    }
                }
                if errors.is_empty() {
                    return keep_checked(notes, prompt, dropped_caption_fixes);
                }
                errors
            }
            Self::Unparsed(error) => vec![format!("the reply is not valid: {error}")],
        };
        match turn {
            Turn::CanRepair => Verdict::Repair { errors },
            Turn::Last => Verdict::Rejected {
                errors,
                dropped_caption_fixes,
            },
        }
    }
}

// A reply is kept with its bad quotes dropped, unless that drops most items.
fn keep_checked(
    notes: MeetingNotes,
    prompt: &Prompt,
    dropped_caption_fixes: Vec<String>,
) -> Verdict {
    let item_count = notes.item_count();
    let checked = citations::check(notes, prompt);
    let dropped_count = item_count - checked.item_count();
    if dropped_count * 2 > item_count {
        return Verdict::Rejected {
            errors: vec![format!(
                "{dropped_count} of {item_count} items cite only quotes that are not in the transcript"
            )],
            dropped_caption_fixes,
        };
    }
    Verdict::Accepted {
        notes: checked,
        dropped_caption_fixes,
    }
}

/// Everything wrong with a parsed reply outside `caption_fixes`, each with its path.
fn reply_errors(notes: &MeetingNotes, prompt: &Prompt) -> Vec<String> {
    let mut errors = Vec::new();
    if notes.summary.trim().is_empty() {
        errors.push("summary must not be empty".to_owned());
    }
    if notes.summary.chars().count() > MAX_SUMMARY_CHARS {
        errors.push(format!(
            "summary must be at most {MAX_SUMMARY_CHARS} characters"
        ));
    }
    for (index, item) in notes.decisions.iter().enumerate() {
        errors.extend(item_errors(
            &format!("decisions[{index}]"),
            &item.decision,
            &item.citations,
            prompt,
        ));
    }
    for (index, item) in notes.action_items.iter().enumerate() {
        errors.extend(item_errors(
            &format!("action_items[{index}]"),
            &item.task,
            &item.citations,
            prompt,
        ));
    }
    for (index, item) in notes.open_questions.iter().enumerate() {
        errors.extend(item_errors(
            &format!("open_questions[{index}]"),
            &item.question,
            &item.citations,
            prompt,
        ));
    }
    for (index, item) in notes.my_commitments.iter().enumerate() {
        errors.extend(item_errors(
            &format!("my_commitments[{index}]"),
            &item.task,
            &item.citations,
            prompt,
        ));
    }
    for (index, item) in notes.action_items.iter().enumerate() {
        if let Some(owner) = item
            .owner
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            && !prompt.is_on_roster(owner)
        {
            errors.push(format!(
                "action_items[{index}].owner {owner:?} is not on the speaker roster; use a roster name or null"
            ));
        }
    }
    errors
}

/// Everything wrong with the reply's `caption_fixes`, each with its path. Kept apart from
/// `reply_errors`, since the notes stand without a bad guess.
fn caption_fix_errors(notes: &MeetingNotes, prompt: &Prompt) -> Vec<String> {
    notes
        .caption_fixes
        .iter()
        .enumerate()
        .flat_map(|(index, fix)| fix_errors(index, fix, prompt))
        .collect()
}

/// Removes each caption fix with an error and returns the errors.
fn drop_bad_caption_fixes(notes: &mut MeetingNotes, prompt: &Prompt) -> Vec<String> {
    let mut errors = Vec::new();
    let mut index = 0;
    notes.caption_fixes.retain(|fix| {
        let fix_errors = fix_errors(index, fix, prompt);
        index += 1;
        let is_good = fix_errors.is_empty();
        errors.extend(fix_errors);
        is_good
    });
    errors
}

fn item_errors(path: &str, text: &str, citations: &[Citation], prompt: &Prompt) -> Vec<String> {
    let mut errors = Vec::new();
    if text.trim().is_empty() {
        errors.push(format!("{path}: the text must not be empty"));
    }
    if citations.is_empty() {
        errors.push(format!("{path}.citations must have at least one citation"));
    }
    for (index, citation) in citations.iter().enumerate() {
        let path = format!("{path}.citations[{index}]");
        if !prompt.has_segment(citation.segment_id) {
            errors.push(format!(
                "{path}.segment_id {} is not a line in the transcript",
                citation.segment_id
            ));
        }
        let quote_chars = citation.quote.chars().count();
        if citation.quote.trim().is_empty() || quote_chars > MAX_QUOTE_CHARS {
            errors.push(format!(
                "{path}.quote must be 3 to 15 words copied from the line"
            ));
        }
    }
    errors
}

fn fix_errors(index: usize, fix: &CaptionFix, prompt: &Prompt) -> Vec<String> {
    let path = format!("caption_fixes[{index}]");
    let mut errors = item_errors(&path, &fix.heard, &fix.citations, prompt);
    if fix.likely.trim().is_empty() || citations::same_words(&fix.heard, &fix.likely) {
        errors.push(format!(
            "{path}.likely must be your guess at the words, different from heard"
        ));
    }
    if fix.likely.chars().count() > MAX_QUOTE_CHARS {
        errors.push(format!(
            "{path}.likely must be at most {MAX_QUOTE_CHARS} characters"
        ));
    }
    if fix.likely.contains(['(', ')', '?']) {
        errors.push(format!(
            "{path}.likely must be one guess with no comment; set is_unsure for doubt"
        ));
    }
    for (citation_index, citation) in fix.citations.iter().enumerate() {
        if !citations::quote_holds(&citation.quote, &fix.heard) {
            errors.push(format!(
                "{path}.citations[{citation_index}].quote must contain the heard words {:?}",
                fix.heard
            ));
        }
    }
    errors
}
