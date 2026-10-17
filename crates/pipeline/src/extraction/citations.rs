//! The quote check: finds each cited quote in the transcript and points the citation at it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::prompt::{LineSegment, Prompt};
use super::{ActionItem, CheckedNotes, Citation, Commitment, Decision, MeetingNotes, OpenQuestion};

/// A citation after the check. `segment_id` and `start_s` name the transcript segment that holds
/// the quote, which may differ from the line the model cited.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckedCitation {
    pub segment_id: u32,
    pub start_s: f64,
    pub quote: String,
    pub verdict: CitationVerdict,
}

/// Where the quote was found. A quote found nowhere drops its citation, so it has no verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CitationVerdict {
    /// Word for word in the cited line.
    Exact,
    /// Word for word in the line just before or after the cited one.
    Neighbour,
    /// At least `MIN_FUZZY_COVERAGE` of its words in the cited line or a neighbour.
    Fuzzy,
    /// Found elsewhere in the call, exactly or fuzzily.
    Moved,
}

// Accepts a dropped stutter ("I'll probably I'll share" quoted as "I'll probably share") and
// rejects a paraphrase. Tune on the first five real meetings.
const MIN_FUZZY_COVERAGE: f64 = 0.75;
// Two words match by chance too often to accept anything but an exact match.
const MIN_FUZZY_WORDS: usize = 3;

// Words that captions add or drop at random, and the model then does the same in its quotes.
const FILLERS: [&str; 10] = [
    "um", "uh", "hmm", "like", "matlab", "haan", "ok", "okay", "yeah", "so",
];

/// One error per citation whose quote is not in the transcript, for a repair turn.
pub(super) fn missing_quotes(notes: &MeetingNotes, prompt: &Prompt) -> Vec<String> {
    let checker = Checker::new(prompt);
    let mut errors = Vec::new();
    let mut check_item = |path: String, citations: &[Citation]| {
        for (index, citation) in citations.iter().enumerate() {
            if checker.locate(citation).is_none() {
                errors.push(format!(
                    "{path}.citations[{index}].quote is not in the transcript; copy 3 to 15 words exactly as line {} has them",
                    citation.segment_id
                ));
            }
        }
    };
    for (index, item) in notes.decisions.iter().enumerate() {
        check_item(format!("decisions[{index}]"), &item.citations);
    }
    for (index, item) in notes.action_items.iter().enumerate() {
        check_item(format!("action_items[{index}]"), &item.citations);
    }
    for (index, item) in notes.open_questions.iter().enumerate() {
        check_item(format!("open_questions[{index}]"), &item.citations);
    }
    for (index, item) in notes.my_commitments.iter().enumerate() {
        check_item(format!("my_commitments[{index}]"), &item.citations);
    }
    errors
}

pub(super) fn check(notes: MeetingNotes, prompt: &Prompt) -> CheckedNotes {
    let mut checker = Checker::new(prompt);
    let decisions = notes
        .decisions
        .into_iter()
        .filter_map(|item| {
            Some(Decision {
                citations: checker.check_item(item.citations)?,
                decision: item.decision,
            })
        })
        .collect();
    let action_items = notes
        .action_items
        .into_iter()
        .filter_map(|item| {
            Some(ActionItem {
                citations: checker.check_item(item.citations)?,
                task: item.task,
                owner: item.owner,
                due: item.due,
            })
        })
        .collect();
    let open_questions = notes
        .open_questions
        .into_iter()
        .filter_map(|item| {
            Some(OpenQuestion {
                citations: checker.check_item(item.citations)?,
                question: item.question,
            })
        })
        .collect();
    let my_commitments = notes
        .my_commitments
        .into_iter()
        .filter_map(|item| {
            Some(Commitment {
                citations: checker.check_item(item.citations)?,
                task: item.task,
                recipient: item.recipient,
                due: item.due,
                strength: item.strength,
            })
        })
        .collect();
    checker.log_counts();
    MeetingNotes {
        summary: notes.summary,
        decisions,
        action_items,
        open_questions,
        my_commitments,
    }
}

struct Checker<'a> {
    prompt: &'a Prompt,
    // Each line's words, normalised once.
    line_words: Vec<Vec<String>>,
    verdict_counts: HashMap<CitationVerdict, usize>,
    failed_citations: usize,
    dropped_items: usize,
}

impl<'a> Checker<'a> {
    fn new(prompt: &'a Prompt) -> Self {
        Self {
            prompt,
            line_words: prompt
                .lines()
                .iter()
                .map(|line| words(&line.text))
                .collect(),
            verdict_counts: HashMap::new(),
            failed_citations: 0,
            dropped_items: 0,
        }
    }

    /// The item's citations that hold up, or `None` when none do and the item must go.
    fn check_item(&mut self, citations: Vec<Citation>) -> Option<Vec<CheckedCitation>> {
        let checked: Vec<CheckedCitation> = citations
            .into_iter()
            .filter_map(|citation| self.check_citation(citation))
            .collect();
        if checked.is_empty() {
            self.dropped_items += 1;
            return None;
        }
        Some(checked)
    }

    fn check_citation(&mut self, citation: Citation) -> Option<CheckedCitation> {
        let Some((verdict, line_index)) = self.locate(&citation) else {
            self.failed_citations += 1;
            tracing::debug!(
                extraction.segment_id = citation.segment_id,
                extraction.quote = %citation.quote,
                "quote not found in the transcript; citation dropped"
            );
            return None;
        };
        *self.verdict_counts.entry(verdict).or_default() += 1;
        let segment = best_segment(
            &self.prompt.lines()[line_index].segments,
            &words(&citation.quote),
        );
        Some(CheckedCitation {
            segment_id: segment.id,
            start_s: segment.start_s,
            quote: citation.quote,
            verdict,
        })
    }

    fn locate(&self, citation: &Citation) -> Option<(CitationVerdict, usize)> {
        let quote = words(&citation.quote);
        if quote.is_empty() {
            return None;
        }
        let is_fuzzy_allowed = quote.len() >= MIN_FUZZY_WORDS;
        // A word or two occurs by chance in most lines, so a short quote counts only as a whole
        // line, such as "Agreed".
        let contains = |index: usize| {
            let line = &self.line_words[index];
            if is_fuzzy_allowed {
                holds_phrase(line, &quote)
            } else {
                *line == quote
            }
        };
        let line_count = self.line_words.len();

        if let Some(cited) = self.prompt.line_index(citation.segment_id) {
            let neighbours = [
                cited.checked_sub(1),
                Some(cited + 1).filter(|&next| next < line_count),
            ];
            if contains(cited) {
                return Some((CitationVerdict::Exact, cited));
            }
            if let Some(index) = neighbours
                .into_iter()
                .flatten()
                .find(|&index| contains(index))
            {
                return Some((CitationVerdict::Neighbour, index));
            }
            if is_fuzzy_allowed {
                let nearby = std::iter::once(cited).chain(neighbours.into_iter().flatten());
                if let Some(index) = self.best_coverage(&quote, nearby) {
                    return Some((CitationVerdict::Fuzzy, index));
                }
            }
        }
        if let Some(index) = (0..line_count).find(|&index| contains(index)) {
            return Some((CitationVerdict::Moved, index));
        }
        if is_fuzzy_allowed {
            return self
                .best_coverage(&quote, 0..line_count)
                .map(|index| (CitationVerdict::Moved, index));
        }
        None
    }

    /// The line among `candidates` that covers most of the quote, if any reaches the threshold.
    fn best_coverage(
        &self,
        quote: &[String],
        candidates: impl Iterator<Item = usize>,
    ) -> Option<usize> {
        candidates
            .map(|index| (index, coverage(quote, &self.line_words[index])))
            .filter(|&(_, covered)| covered >= MIN_FUZZY_COVERAGE)
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .map(|(index, _)| index)
    }

    fn log_counts(&self) {
        let count = |verdict| self.verdict_counts.get(&verdict).copied().unwrap_or(0);
        tracing::info!(
            extraction.citations.exact = count(CitationVerdict::Exact),
            extraction.citations.neighbour = count(CitationVerdict::Neighbour),
            extraction.citations.fuzzy = count(CitationVerdict::Fuzzy),
            extraction.citations.moved = count(CitationVerdict::Moved),
            extraction.citations.failed = self.failed_citations,
            extraction.items.dropped = self.dropped_items,
            "quotes checked"
        );
    }
}

// A merged line holds several segments; the citation's time comes from the one with the quote.
fn best_segment<'a>(segments: &'a [LineSegment], quote: &[String]) -> &'a LineSegment {
    segments
        .iter()
        .find(|segment| holds_phrase(&words(&segment.text), quote))
        .or_else(|| {
            segments.iter().max_by(|left, right| {
                coverage(quote, &words(&left.text)).total_cmp(&coverage(quote, &words(&right.text)))
            })
        })
        .expect("a prompt line has at least one segment")
}

// Whole words in order, so "he s" never matches inside "the sea".
fn holds_phrase(line: &[String], quote: &[String]) -> bool {
    !quote.is_empty() && line.windows(quote.len()).any(|window| window == quote)
}

/// Lower case, punctuation as spaces, fillers gone.
fn words(text: &str) -> Vec<String> {
    let spaced: String = text
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character
            } else {
                ' '
            }
        })
        .collect();
    spaced
        .split_whitespace()
        .filter(|word| !FILLERS.contains(word))
        .map(str::to_owned)
        .collect()
}

/// The share of the quote's words found in `line`, each line word used once.
fn coverage(quote: &[String], line: &[String]) -> f64 {
    let mut unused: HashMap<&str, usize> = HashMap::new();
    for word in line {
        *unused.entry(word.as_str()).or_default() += 1;
    }
    let hits = quote
        .iter()
        .filter(|word| match unused.get_mut(word.as_str()) {
            Some(left) if *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        })
        .count();
    #[expect(clippy::cast_precision_loss, reason = "word counts are far below 2^52")]
    let share = hits as f64 / quote.len() as f64;
    share
}
