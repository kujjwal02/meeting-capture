//! Turns one LLM reply into meeting notes whose every item cites the transcript.

mod citations;
mod prompt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use citations::{CheckedCitation, CitationVerdict};
pub use prompt::Prompt;

/// The notes in one shape for the reply and the stored result. `C` is `Citation` in the model's
/// reply and `CheckedCitation` once the quotes are checked, so the schema sent to the model and
/// the notes kept in the job store cannot drift apart.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "MeetingNotes")]
pub struct MeetingNotes<C = Citation> {
    /// 3 to 6 sentences on what the call was about and where it ended.
    pub summary: String,
    pub decisions: Vec<Decision<C>>,
    pub action_items: Vec<ActionItem<C>>,
    pub open_questions: Vec<OpenQuestion<C>>,
    /// Only what "me" said they would do, in the first person.
    pub my_commitments: Vec<Commitment<C>>,
}

impl<C> MeetingNotes<C> {
    /// Decisions, action items, open questions and commitments together.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.decisions.len()
            + self.action_items.len()
            + self.open_questions.len()
            + self.my_commitments.len()
    }
}

/// The notes after the quote check. Items whose citations all failed are gone.
pub type CheckedNotes = MeetingNotes<CheckedCitation>;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Decision")]
pub struct Decision<C = Citation> {
    pub decision: String,
    #[schemars(length(min = 1))]
    pub citations: Vec<C>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "ActionItem")]
pub struct ActionItem<C = Citation> {
    pub task: String,
    /// A name from the speaker roster, or null.
    #[schemars(required)]
    pub owner: Option<String>,
    /// The deadline as spoken, or null.
    #[schemars(required)]
    pub due: Option<String>,
    #[schemars(length(min = 1))]
    pub citations: Vec<C>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "OpenQuestion")]
pub struct OpenQuestion<C = Citation> {
    pub question: String,
    #[schemars(length(min = 1))]
    pub citations: Vec<C>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Commitment")]
pub struct Commitment<C = Citation> {
    pub task: String,
    /// Who it was promised to, or null.
    #[schemars(required)]
    pub recipient: Option<String>,
    /// The deadline as spoken, or null.
    #[schemars(required)]
    pub due: Option<String>,
    pub strength: CommitmentStrength,
    #[schemars(length(min = 1))]
    pub citations: Vec<C>,
}

// The levels come from the FollowThrough commitments tool.
/// How firm a commitment is: a promise, a "might" or "will try", or a "we should someday".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentStrength {
    Committed,
    Hedged,
    Aspirational,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    /// The [n] number of the transcript line.
    pub segment_id: u32,
    /// 3 to 15 words copied exactly from that line, caption errors included.
    pub quote: String,
}

// Long enough for any real summary; a longer one means the model is writing an essay.
const MAX_SUMMARY_CHARS: usize = 1500;
// 15 words of long Hinglish words still fit.
const MAX_QUOTE_CHARS: usize = 200;

/// The JSON schema of a reply, for providers that take one.
#[must_use]
pub fn reply_schema() -> serde_json::Value {
    schemars::schema_for!(MeetingNotes).to_value()
}

/// Reads a reply that should be one JSON object. Models that cannot be held to a schema wrap it
/// in a code fence or a sentence, so the text outside the outermost braces is ignored.
///
/// # Errors
///
/// Returns one message naming the path of the first field that does not fit, for a repair turn.
pub fn parse_reply(reply: &str) -> Result<MeetingNotes, String> {
    let object = match (reply.find('{'), reply.rfind('}')) {
        (Some(start), Some(end)) if start < end => &reply[start..=end],
        _ => reply,
    };
    let mut deserializer = serde_json::Deserializer::from_str(object);
    serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|error| format!("{}: {}", error.path(), error.inner()))
}

/// Everything wrong with a parsed reply, each with its path, for a repair turn. Empty when the
/// reply can be checked.
#[must_use]
pub fn reply_errors(notes: &MeetingNotes, prompt: &Prompt) -> Vec<String> {
    let mut errors = Vec::new();
    if notes.summary.trim().is_empty() {
        errors.push("summary must not be empty".to_owned());
    }
    if notes.summary.chars().count() > MAX_SUMMARY_CHARS {
        errors.push(format!(
            "summary must be at most {MAX_SUMMARY_CHARS} characters"
        ));
    }
    let mut check_item = |path: String, text: &str, citations: &[Citation]| {
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
    };
    for (index, item) in notes.decisions.iter().enumerate() {
        check_item(
            format!("decisions[{index}]"),
            &item.decision,
            &item.citations,
        );
    }
    for (index, item) in notes.action_items.iter().enumerate() {
        check_item(
            format!("action_items[{index}]"),
            &item.task,
            &item.citations,
        );
    }
    for (index, item) in notes.open_questions.iter().enumerate() {
        check_item(
            format!("open_questions[{index}]"),
            &item.question,
            &item.citations,
        );
    }
    for (index, item) in notes.my_commitments.iter().enumerate() {
        check_item(
            format!("my_commitments[{index}]"),
            &item.task,
            &item.citations,
        );
    }
    for (index, item) in notes.action_items.iter().enumerate() {
        if let Some(owner) = &item.owner
            && !prompt.is_on_roster(owner)
        {
            errors.push(format!(
                "action_items[{index}].owner {owner:?} is not on the speaker roster; use a roster name or null"
            ));
        }
    }
    errors
}

/// One error per citation whose quote is not in the transcript. Kept apart from `reply_errors`,
/// since a few such quotes still leave usable notes once the check drops them.
#[must_use]
pub fn quote_errors(notes: &MeetingNotes, prompt: &Prompt) -> Vec<String> {
    citations::missing_quotes(notes, prompt)
}

/// Checks every quote against the transcript and points each citation at the line that holds
/// it. A citation whose quote is nowhere in the call is dropped, and so is an item left with none.
#[must_use]
pub fn check_citations(notes: MeetingNotes, prompt: &Prompt) -> CheckedNotes {
    citations::check(notes, prompt)
}
