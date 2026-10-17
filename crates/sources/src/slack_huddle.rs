//! The Slack huddle log: one JSON line per thing the huddle watcher saw in Slack's huddle window,
//! appended while the huddle runs. The log is the raw input; `parse` turns it into a transcript.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{Attendee, Platform, Segment, SlackHuddle, Source, SpeakerNaming, Transcript};

/// The log format version, written on the `started` line.
pub const SCHEMA: u32 = 1;

/// One line of the huddle log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LogLine {
    /// The first line. `channel` is a channel name, or the other people's names in a DM.
    Started {
        schema: u32,
        seen_at: Timestamp,
        slack_version: String,
        layout_profile: String,
        window_title: String,
        channel: String,
        workspace: String,
    },
    Health {
        seen_at: Timestamp,
        state: HuddleHealth,
        reason: String,
    },
    /// A participant tile appeared.
    Tile {
        seen_at: Timestamp,
        slack_user_id: String,
        name: String,
        is_self: bool,
    },
    /// A participant tile disappeared.
    TileGone {
        seen_at: Timestamp,
        slack_user_id: String,
    },
    /// The panel header names the caption language, such as "English (US)".
    Language {
        seen_at: Timestamp,
        language: String,
    },
    /// A caption row appeared or its text grew. The last text for a row id is final.
    Row {
        seen_at: Timestamp,
        row: String,
        speaker: String,
        text: String,
        /// The row was already in the panel when the watch attached, so `seen_at` is not when
        /// it was said.
        backlog: bool,
    },
    /// A line of the fading caption overlay, read only while the caption panel is missing.
    Overlay {
        seen_at: Timestamp,
        speaker: String,
        text: String,
    },
    /// A layout profile matched the huddle window after the start, replacing the one on the
    /// `started` line.
    LayoutProfile {
        seen_at: Timestamp,
        layout_profile: String,
    },
    /// The watcher pressed the Captions tab to show the caption panel.
    CaptionsTabPressed { seen_at: Timestamp },
    /// The huddle window closed.
    Ended { seen_at: Timestamp },
}

impl LogLine {
    #[must_use]
    pub fn seen_at(&self) -> Timestamp {
        match self {
            LogLine::Started { seen_at, .. }
            | LogLine::Health { seen_at, .. }
            | LogLine::Tile { seen_at, .. }
            | LogLine::TileGone { seen_at, .. }
            | LogLine::Language { seen_at, .. }
            | LogLine::Row { seen_at, .. }
            | LogLine::Overlay { seen_at, .. }
            | LogLine::LayoutProfile { seen_at, .. }
            | LogLine::CaptionsTabPressed { seen_at }
            | LogLine::Ended { seen_at } => *seen_at,
        }
    }
}

/// How well the watcher could read a huddle, worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HuddleHealth {
    /// Caption rows and tiles found by a layout profile tested on this Slack version.
    Healthy,
    /// A layout profile matched, but it was not tested on this Slack version.
    Untested,
    /// Only part was found: the overlay without the panel, the panel by structure alone, or no
    /// tiles.
    Degraded,
    /// The huddle window has no captions.
    CaptionsOff,
    /// No layout profile could read the huddle window.
    Broken,
}

impl HuddleHealth {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HuddleHealth::Healthy => "healthy",
            HuddleHealth::Untested => "untested",
            HuddleHealth::Degraded => "degraded",
            HuddleHealth::CaptionsOff => "captions-off",
            HuddleHealth::Broken => "broken",
        }
    }

    /// Whether the note may miss lines or names, so the user should hear about it.
    #[must_use]
    pub fn is_warning(self) -> bool {
        match self {
            HuddleHealth::Healthy | HuddleHealth::Untested => false,
            HuddleHealth::Degraded | HuddleHealth::CaptionsOff | HuddleHealth::Broken => true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("huddle log line {line} is not valid: {source}")]
    InvalidLine {
        line: usize,
        source: serde_json::Error,
    },
    #[error("huddle log does not start with a `started` line")]
    NoStartedLine,
    #[error("huddle log schema {found} is not supported; this app reads schema {SCHEMA}")]
    Schema { found: u32 },
}

/// Reads every line of a huddle log. A torn last line, from a crash mid-write, is skipped.
///
/// # Errors
///
/// Returns `ParseError::InvalidLine` naming the first bad line other than a torn last one.
pub fn read_lines(log: &[u8]) -> Result<Vec<LogLine>, ParseError> {
    let text = String::from_utf8_lossy(log);
    let is_torn = !text.is_empty() && !text.ends_with('\n');
    let raw_lines: Vec<&str> = text.lines().collect();
    let mut lines = Vec::with_capacity(raw_lines.len());
    for (index, raw) in raw_lines.iter().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(raw) {
            Ok(line) => lines.push(line),
            Err(_) if is_torn && index + 1 == raw_lines.len() => {}
            Err(source) => {
                return Err(ParseError::InvalidLine {
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(lines)
}

// An overlay line repeats a panel row when the same person said it near the row's time. Slack
// keeps filling the panel while it is hidden, and the watcher reads those rows when the panel
// comes back, up to about 2 minutes after the overlay showed the words.
const OVERLAY_BEFORE_ROW: SignedDuration = SignedDuration::from_mins(3);
const OVERLAY_AFTER_ROW: SignedDuration = SignedDuration::from_secs(30);

/// Parses a huddle log into a transcript.
///
/// # Errors
///
/// Returns `ParseError` for a bad line, a missing `started` line or another schema.
pub fn parse(log: &[u8]) -> Result<Transcript, ParseError> {
    let lines = read_lines(log)?;
    let Some(LogLine::Started {
        schema,
        seen_at: started_at,
        slack_version,
        mut layout_profile,
        channel,
        workspace,
        ..
    }) = lines.first().cloned()
    else {
        return Err(ParseError::NoStartedLine);
    };
    if schema != SCHEMA {
        return Err(ParseError::Schema { found: schema });
    }
    let ended_at = lines
        .iter()
        .map(LogLine::seen_at)
        .max()
        .unwrap_or(started_at);

    let Collected {
        rows,
        overlays,
        attendees,
        worst_health,
        language,
        later_layout_profile,
    } = collect(&lines);
    if let Some(later) = later_layout_profile {
        layout_profile = later;
    }
    let rows = merge_overlays(rows, overlays);
    let segments = segments(rows, started_at, ended_at);

    let my_speaker = attendees
        .iter()
        .find(|attendee| attendee.is_self)
        .map(|attendee| attendee.name.clone());
    let title = huddle_title(&channel);
    Ok(Transcript {
        source: Source::SlackHuddle,
        started_at,
        ended_at,
        platform: Platform::Slack,
        meeting_url: None,
        title: Some(title),
        speaker_naming: SpeakerNaming::Real,
        my_speaker,
        attendees,
        segments,
        chat_messages: Vec::new(),
        slack_huddle: Some(SlackHuddle {
            workspace,
            channel,
            slack_version,
            layout_profile,
            health: worst_health,
            language,
        }),
    })
}

struct CaptionRow {
    first_seen_at: Timestamp,
    last_seen_at: Timestamp,
    speaker: String,
    text: String,
    is_backlog: bool,
    is_overlay: bool,
}

struct OverlayLine {
    seen_at: Timestamp,
    speaker: String,
    text: String,
}

struct Collected {
    rows: Vec<CaptionRow>,
    overlays: Vec<OverlayLine>,
    attendees: Vec<Attendee>,
    worst_health: HuddleHealth,
    language: Option<String>,
    later_layout_profile: Option<String>,
}

fn collect(lines: &[LogLine]) -> Collected {
    let mut collected = Collected {
        rows: Vec::new(),
        overlays: Vec::new(),
        attendees: Vec::new(),
        worst_health: HuddleHealth::Healthy,
        language: None,
        later_layout_profile: None,
    };
    let mut row_index: BTreeMap<&str, usize> = BTreeMap::new();
    // Backlog rows come first. One after a live row was flagged by mistake, and its time is
    // close enough to keep.
    let mut has_live_row = false;
    for line in lines {
        match line {
            LogLine::Row {
                seen_at,
                row,
                speaker,
                text,
                backlog,
            } => {
                if let Some(&index) = row_index.get(row.as_str()) {
                    let known = &mut collected.rows[index];
                    known.speaker.clone_from(speaker);
                    known.text.clone_from(text);
                    known.last_seen_at = *seen_at;
                } else {
                    has_live_row |= !backlog;
                    row_index.insert(row, collected.rows.len());
                    collected.rows.push(CaptionRow {
                        first_seen_at: *seen_at,
                        last_seen_at: *seen_at,
                        speaker: speaker.clone(),
                        text: text.clone(),
                        is_backlog: *backlog && !has_live_row,
                        is_overlay: false,
                    });
                }
            }
            LogLine::Overlay {
                seen_at,
                speaker,
                text,
            } => collected.overlays.push(OverlayLine {
                seen_at: *seen_at,
                speaker: speaker.clone(),
                text: text.clone(),
            }),
            LogLine::Tile {
                slack_user_id,
                name,
                is_self,
                ..
            } => {
                let is_known = collected
                    .attendees
                    .iter()
                    .any(|known| known.platform_id.as_deref() == Some(slack_user_id));
                if !is_known {
                    collected.attendees.push(Attendee {
                        name: name.clone(),
                        is_self: *is_self,
                        platform_id: Some(slack_user_id.clone()),
                    });
                }
            }
            LogLine::Health { state, .. } => {
                collected.worst_health = collected.worst_health.max(*state);
            }
            LogLine::Language { language, .. } => collected.language = Some(language.clone()),
            LogLine::LayoutProfile { layout_profile, .. } => {
                collected.later_layout_profile = Some(layout_profile.clone());
            }
            LogLine::Started { .. }
            | LogLine::TileGone { .. }
            | LogLine::CaptionsTabPressed { .. }
            | LogLine::Ended { .. } => {}
        }
    }
    collected
}

// Overlay lines fill gaps while the panel was hidden; the rest repeat part of a panel row, since
// a row holds a whole speaker turn. The newest overlay line grows in place like a row, so a longer
// version replaces the shorter.
fn merge_overlays(mut rows: Vec<CaptionRow>, overlays: Vec<OverlayLine>) -> Vec<CaptionRow> {
    for overlay in overlays {
        let text_key = normalised(&overlay.text);
        let repeats_row = rows.iter().any(|row| {
            !row.is_overlay
                && row.speaker == overlay.speaker
                && overlay.seen_at >= row.first_seen_at - OVERLAY_BEFORE_ROW
                && overlay.seen_at <= row.last_seen_at + OVERLAY_AFTER_ROW
                && has_words(&normalised(&row.text), &text_key)
        });
        if repeats_row {
            continue;
        }
        let grown = rows.iter_mut().rev().find(|row| {
            row.is_overlay
                && row.speaker == overlay.speaker
                && overlay.seen_at.duration_since(row.last_seen_at).abs() <= OVERLAY_AFTER_ROW
                && text_key.starts_with(&normalised(&row.text))
        });
        if let Some(row) = grown {
            row.text = overlay.text;
            row.last_seen_at = overlay.seen_at;
        } else {
            rows.push(CaptionRow {
                first_seen_at: overlay.seen_at,
                last_seen_at: overlay.seen_at,
                speaker: overlay.speaker,
                text: overlay.text,
                is_backlog: false,
                is_overlay: true,
            });
        }
    }
    rows.retain(|row| !row.text.trim().is_empty());
    rows.sort_by_key(|row| (!row.is_backlog, row.first_seen_at));
    rows
}

fn segments(rows: Vec<CaptionRow>, started_at: Timestamp, ended_at: Timestamp) -> Vec<Segment> {
    let offset_s = |at: Timestamp| {
        at.duration_since(started_at)
            .max(SignedDuration::ZERO)
            .as_secs_f64()
    };
    // Backlog rows share the time the watch attached. They are spread evenly before the first
    // live row, so they keep their order.
    let backlog_count = rows.iter().filter(|row| row.is_backlog).count();
    let first_live_s = rows
        .iter()
        .find(|row| !row.is_backlog)
        .map_or_else(|| offset_s(ended_at), |row| offset_s(row.first_seen_at));
    #[expect(clippy::cast_precision_loss, reason = "a huddle has few rows")]
    let backlog_step_s = first_live_s / (backlog_count.max(1) as f64);
    #[expect(clippy::cast_precision_loss, reason = "a huddle has few rows")]
    let starts_s: Vec<f64> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if row.is_backlog {
                index as f64 * backlog_step_s
            } else {
                offset_s(row.first_seen_at)
            }
        })
        .collect();
    let call_length_s = offset_s(ended_at);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| Segment {
            start_s: starts_s[index],
            end_s: starts_s
                .get(index + 1)
                .copied()
                .unwrap_or(call_length_s)
                .max(starts_s[index]),
            speaker: row.speaker,
            text: row.text,
        })
        .collect()
}

/// Whether `words` shows in `text` as whole words. Both are normalised.
fn has_words(text: &str, words: &str) -> bool {
    !words.is_empty() && format!(" {text} ").contains(&format!(" {words} "))
}

fn normalised(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_alphanumeric() || character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a huddle's `channel` is a DM, which Slack titles with the other people's names.
/// Slack channel names have no spaces.
#[must_use]
pub fn is_dm(channel: &str) -> bool {
    channel.contains(char::is_whitespace)
}

/// "Huddle in #team-review" for a channel, "Huddle with <names>" for a DM.
#[must_use]
pub fn huddle_title(channel: &str) -> String {
    if is_dm(channel) {
        format!("Huddle with {channel}")
    } else {
        format!("Huddle in #{channel}")
    }
}

/// The log's file name: its start time, with no channel name, since a channel name can name a
/// client.
#[must_use]
pub fn log_file_name(started_at: Timestamp) -> String {
    // No colons: Windows rejects them in file names.
    format!(
        "{}-slack-huddle.jsonl",
        started_at.strftime("%Y%m%dT%H%M%S%.3fZ")
    )
}

/// Splits a huddle window title, `<channel> - <workspace> - Slack`, into channel and workspace.
/// The workspace is read from the right, since a channel or DM title can hold " - " too.
#[must_use]
pub fn split_window_title(title: &str) -> Option<(String, String)> {
    let rest = title.strip_suffix(" - Slack")?;
    let (channel, workspace) = rest.rsplit_once(" - ")?;
    let channel = channel.trim();
    let workspace = workspace.trim();
    (!channel.is_empty() && !workspace.is_empty())
        .then(|| (channel.to_owned(), workspace.to_owned()))
}
