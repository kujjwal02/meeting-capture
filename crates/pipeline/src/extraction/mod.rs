//! Turns one LLM reply into meeting notes whose every item cites the transcript.

mod citations;
mod prompt;
mod reply;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use citations::{CheckedCitation, CitationVerdict};
pub use prompt::Prompt;
pub(crate) use reply::Reply;
pub use reply::{Turn, Verdict, check_quotes, judge};

/// The notes in one shape for the reply and the stored result. `C` is `Citation` in the model's
/// reply and `CheckedCitation` once the quotes are checked, so the schema sent to the model and
/// the notes kept in the job store cannot drift apart.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound(deserialize = "C: Deserialize<'de>"))]
#[schemars(rename = "MeetingNotes")]
pub struct MeetingNotes<C = Citation> {
    /// 3 to 6 sentences on what the call was about and where it ended.
    pub summary: String,
    pub decisions: Vec<Decision<C>>,
    pub action_items: Vec<ActionItem<C>>,
    pub open_questions: Vec<OpenQuestion<C>>,
    /// What "me" said they would do, and what "me" said should be done with no other owner, in
    /// the first person.
    pub my_commitments: Vec<Commitment<C>>,
    /// Captioned words that look misheard, with what the speaker most likely said. Notes stored
    /// before this field existed read as an empty list.
    #[serde(default)]
    #[schemars(required)]
    pub caption_fixes: Vec<CaptionFix<C>>,
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

/// A guess at what the captions misheard. The citation's quote holds `heard`, so the quote check
/// proves the words were captioned that way.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "CaptionFix")]
pub struct CaptionFix<C = Citation> {
    /// The words as captioned, copied from the line.
    pub heard: String,
    /// What the speaker most likely said: the words only.
    pub likely: String,
    /// True for a guess from little evidence, such as a name the call never says plainly.
    #[serde(default)]
    #[schemars(required)]
    pub is_unsure: bool,
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
    #[schemars(schema_with = "segment_id_schema")]
    pub segment_id: u32,
    /// 3 to 15 words copied exactly from that line, caption errors included.
    pub quote: String,
}

// schemars would add `format: uint32`, which is not a standard format and which strict schema
// checkers reject. The parser still holds the id to a `u32`.
fn segment_id_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type": "integer"})
}

/// The JSON schema of a reply, for Claude Code. It refuses the `$schema` URI schemars adds.
#[must_use]
pub fn reply_schema() -> serde_json::Value {
    let mut schema = schemars::schema_for!(MeetingNotes).to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
    }
    schema
}
