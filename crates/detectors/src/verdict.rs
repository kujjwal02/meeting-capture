//! Names the layer that failed a call, from its problem history.

use jiff::{SignedDuration, Timestamp};

use crate::host::{ProblemSpan, span_end};
use crate::problem::{Layer, Problem};

/// A problem's share of a call, beside the verdict's root.
#[derive(Debug, Clone, Copy)]
pub struct VerdictPart {
    pub layer: Layer,
    pub problem: Problem,
    pub duration: SignedDuration,
}

/// The layer that failed a call: the root of its worst episode.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub layer: Layer,
    pub problem: Problem,
    pub raised_at: Timestamp,
    /// The root problem's own time.
    pub duration: SignedDuration,
    pub is_noticed: bool,
    /// Every other problem of the call, with its summed time, longest first.
    pub others: Vec<VerdictPart>,
}

/// Overlapping spans: one fault and the symptoms it caused.
struct Episode<'a> {
    spans: Vec<&'a ProblemSpan>,
    start: Timestamp,
    end: Timestamp,
}

/// The failing layer of a call whose problems are `history`. A span still open ends at
/// `call_end`: the call's end, or the bundle's last line.
#[must_use]
pub fn verdict(history: &[ProblemSpan], call_end: Timestamp) -> Option<Verdict> {
    let mut spans: Vec<&ProblemSpan> = history.iter().collect();
    spans.sort_by_key(|span| (span.raised_at, span.problem.layer()));
    let mut episodes: Vec<Episode<'_>> = Vec::new();
    for span in spans {
        let end = span_end(span, call_end);
        match episodes.last_mut() {
            Some(episode) if span.raised_at <= episode.end => {
                episode.end = episode.end.max(end);
                episode.spans.push(span);
            }
            Some(_) | None => episodes.push(Episode {
                spans: vec![span],
                start: span.raised_at,
                end,
            }),
        }
    }
    let worst = episodes.iter().max_by_key(|episode| {
        let is_noticed = episode.spans.iter().any(|span| span.noticed_at.is_some());
        (is_noticed, episode.end.duration_since(episode.start))
    })?;
    // Spans are sorted by raise time, then by depth, so the first is the root.
    let root = worst.spans.first()?;
    let mut others: Vec<VerdictPart> = Vec::new();
    for span in history {
        if std::ptr::eq(span, *root) {
            continue;
        }
        let duration = span_end(span, call_end).duration_since(span.raised_at);
        match others.iter_mut().find(|part| part.problem == span.problem) {
            Some(part) => part.duration += duration,
            None => others.push(VerdictPart {
                layer: span.problem.layer(),
                problem: span.problem,
                duration,
            }),
        }
    }
    others.sort_by_key(|part| std::cmp::Reverse(part.duration));
    Some(Verdict {
        layer: root.problem.layer(),
        problem: root.problem,
        raised_at: root.raised_at,
        duration: span_end(root, call_end).duration_since(root.raised_at),
        is_noticed: root.noticed_at.is_some(),
        others,
    })
}
