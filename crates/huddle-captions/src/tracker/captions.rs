//! Caption list rows, overlay lines, tiles and the caption language, each logged once it changes.

use std::collections::{BTreeMap, HashMap, VecDeque};

use jiff::Timestamp;
use sources::slack_huddle::LogLine;

use super::{Caption, ListFind, RowSight, Tile};
use crate::log::HuddleLog;

// A row that grows is one of the last few; earlier rows are final.
const LIVE_ROWS: usize = 3;
const OVERLAY_MEMORY: usize = 40;

#[derive(Debug, Default)]
pub(super) struct Book {
    row_captions: HashMap<String, Caption>,
    row_edits: u64,
    overlay_logged: VecDeque<Caption>,
    overlay_live: Option<LiveOverlay>,
    overlay_lines: u64,
    /// Changes to the live overlay line before it is final.
    overlay_live_edits: u64,
    tiles: BTreeMap<String, Tile>,
    language: Option<String>,
}

#[derive(Debug)]
struct LiveOverlay {
    caption: Caption,
    first_seen_at: Timestamp,
}

impl Book {
    pub(super) fn caption_lines(&self) -> u64 {
        self.row_captions.len() as u64 + self.overlay_lines
    }

    /// Grows with every new or changed caption line, so one person talking on in one line
    /// still counts as captions arriving.
    pub(super) fn caption_changes(&self) -> u64 {
        self.caption_lines() + self.row_edits + self.overlay_live_edits
    }

    pub(super) fn has_tiles(&self) -> bool {
        !self.tiles.is_empty()
    }

    pub(super) fn tile_names(&self) -> impl Iterator<Item = &str> {
        self.tiles.values().map(|tile| tile.name.as_str())
    }

    pub(super) fn saw_tiles(
        &mut self,
        log: &mut HuddleLog,
        seen: BTreeMap<String, Tile>,
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        // Every huddle shows at least the user's own tile. None at all is a window still being
        // built, such as a reopened one, not everyone leaving.
        if seen.is_empty() {
            return Ok(());
        }
        for (user_id, tile) in &seen {
            if self.tiles.get(user_id) != Some(tile) {
                log.append(&LogLine::Tile {
                    seen_at,
                    slack_user_id: user_id.clone(),
                    name: tile.name.clone(),
                    is_self: tile.is_self,
                })?;
            }
        }
        for user_id in self.tiles.keys() {
            if !seen.contains_key(user_id) {
                log.append(&LogLine::TileGone {
                    seen_at,
                    slack_user_id: user_id.clone(),
                })?;
            }
        }
        self.tiles = seen;
        Ok(())
    }

    pub(super) fn saw_rows(
        &mut self,
        log: &mut HuddleLog,
        row_ids: &[Option<String>],
        found: Option<ListFind>,
        mut read_row: impl FnMut(usize) -> RowSight,
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        let is_list_new = found.is_some();
        // Rows found when the panel comes back were said during the watch.
        let is_backlog = match found {
            Some(ListFind::First) => true,
            Some(ListFind::Again) | None => false,
        };
        let live_from = row_ids.len().saturating_sub(LIVE_ROWS);
        for (index, id) in row_ids.iter().enumerate() {
            let Some(id) = id else {
                continue;
            };
            // Rows above the last few are final, except in a panel found again: those grew while
            // it was hidden.
            if !is_list_new && index < live_from && self.row_captions.contains_key(id) {
                continue;
            }
            let caption = match read_row(index) {
                RowSight::Language(language) => {
                    if self.language.as_deref() != Some(language.as_str()) {
                        self.language = Some(language.clone());
                        log.append(&LogLine::Language { seen_at, language })?;
                    }
                    continue;
                }
                RowSight::Caption(caption) => caption,
                RowSight::Unreadable => continue,
            };
            let previous = self.row_captions.get(id);
            if previous == Some(&caption) {
                continue;
            }
            if previous.is_some() {
                self.row_edits += 1;
            }
            log.append(&LogLine::Row {
                seen_at,
                row: id.clone(),
                speaker: caption.speaker.clone(),
                text: caption.text.clone(),
                backlog: is_backlog && previous.is_none(),
            })?;
            self.row_captions.insert(id.clone(), caption);
        }
        Ok(())
    }

    // Slack rewrites the newest overlay line while the person talks, sometimes changing earlier
    // words. A line is logged once it is final: when a newer line shows below it, or when the
    // overlay is no longer read.
    pub(super) fn saw_overlay(
        &mut self,
        log: &mut HuddleLog,
        lines: &[Caption],
        seen_at: Timestamp,
    ) -> std::io::Result<()> {
        let Some((newest, finals)) = lines.split_last() else {
            return self.flush_overlay(log);
        };
        let live = self.overlay_live.take();
        // The live line's speaker got a new final line, so the live line was finished.
        let mut is_live_finished = false;
        for line in finals {
            let is_live_speaker = live
                .as_ref()
                .is_some_and(|live| live.caption.speaker == line.speaker);
            let line_seen_at = match &live {
                Some(live) if is_live_speaker && !is_live_finished => live.first_seen_at,
                Some(_) | None => seen_at,
            };
            if self.log_overlay(log, line.clone(), line_seen_at)? && is_live_speaker {
                is_live_finished = true;
            }
        }
        let live = match live {
            Some(mut live) if !is_live_finished && live.caption.speaker == newest.speaker => {
                if live.caption != *newest {
                    self.overlay_live_edits += 1;
                    live.caption = newest.clone();
                }
                live
            }
            previous => {
                // Another speaker took over and the old line never showed as final.
                if let Some(previous) = previous
                    && !is_live_finished
                {
                    self.log_overlay(log, previous.caption, previous.first_seen_at)?;
                }
                LiveOverlay {
                    caption: newest.clone(),
                    first_seen_at: seen_at,
                }
            }
        };
        self.overlay_live = Some(live);
        Ok(())
    }

    /// Logs the newest overlay line, which no newer line will finish.
    pub(super) fn flush_overlay(&mut self, log: &mut HuddleLog) -> std::io::Result<()> {
        match self.overlay_live.take() {
            Some(live) => self
                .log_overlay(log, live.caption, live.first_seen_at)
                .map(|_| ()),
            None => Ok(()),
        }
    }

    pub(super) fn trace_ended(&self) {
        tracing::info!(
            caption.rows = self.row_captions.len(),
            caption.row_edits = self.row_edits,
            caption.overlay_lines = self.overlay_lines,
            "Slack huddle ended"
        );
    }

    /// Returns whether the line was new. Final lines stay on screen for several reads.
    fn log_overlay(
        &mut self,
        log: &mut HuddleLog,
        caption: Caption,
        seen_at: Timestamp,
    ) -> std::io::Result<bool> {
        if self.overlay_logged.contains(&caption) {
            return Ok(false);
        }
        log.append(&LogLine::Overlay {
            seen_at,
            speaker: caption.speaker.clone(),
            text: caption.text.clone(),
        })?;
        self.overlay_lines += 1;
        self.overlay_logged.push_back(caption);
        if self.overlay_logged.len() > OVERLAY_MEMORY {
            self.overlay_logged.pop_front();
        }
        Ok(true)
    }
}
