//! One huddle's window elements, read into observations for its tracker.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use jiff::Timestamp;

use super::ax::{Element, SlackProcess};
use super::tree::{self, Node};
use crate::WatchSettings;
use crate::profile::LayoutProfile;
use crate::tracker::{
    HealthChange, HuddleTracker, ListMatch, Moment, Tile, WindowSight, WindowTitle,
};

pub(super) struct Huddle {
    pub(super) channel: String,
    /// `None` while the user has closed the huddle window and the huddle goes on.
    pub(super) window: Option<Element>,
    /// The current window's whole title, for the app's window hint.
    pub(super) window_title: String,
    /// The app has the current window's id. It gets the id only once the huddle content class
    /// shows in the window: a conversation popped out of the main window is a titled child
    /// window too, and video must never record it.
    pub(super) is_window_hinted: bool,
    pub(super) tracker: HuddleTracker,
    pub(super) missed_discoveries: u32,
    pub(super) last_seen_at: Timestamp,
    /// `HuddleTracker::caption_changes` when the app last heard of captions.
    pub(super) reported_caption_changes: u64,
    list: Option<(Element, ListMatch)>,
    overlay: Option<Element>,
}

impl Huddle {
    pub(super) fn start(
        log_dir: &Path,
        slack: &SlackProcess,
        window: &Element,
        window_title: WindowTitle,
        profile: Option<Rc<LayoutProfile>>,
    ) -> std::io::Result<Self> {
        let moment = Moment::now();
        let channel = window_title.channel.clone();
        let title = window_title.title.clone();
        let is_window_hinted = profile.is_some();
        let tracker = HuddleTracker::start(
            log_dir,
            moment,
            slack.version.clone(),
            profile,
            window_title,
        )?;
        Ok(Self {
            channel,
            window: Some(window.clone()),
            window_title: title,
            is_window_hinted,
            tracker,
            missed_discoveries: 0,
            last_seen_at: moment.seen_at,
            reported_caption_changes: 0,
            list: None,
            overlay: None,
        })
    }

    /// Forgets the closed window's elements. The rows read so far stay, so the reopened window's
    /// panel adds only new rows.
    pub(super) fn detach(&mut self, now: Instant) {
        self.list = None;
        self.overlay = None;
        self.tracker.list_lost(now);
    }

    /// Reads tiles, finds the caption list and overlay, presses the Captions tab when allowed,
    /// and works out the health.
    pub(super) fn check_window(
        &mut self,
        nodes: &[Node],
        settings: &WatchSettings,
    ) -> std::io::Result<Option<HealthChange>> {
        let moment = Moment::now();
        let Some(profile) = self.tracker.profile() else {
            return self.tracker.saw_unreadable_window(moment);
        };

        self.tracker
            .saw_tiles(read_tiles(&profile, nodes), moment.seen_at)?;
        if self.list.is_none() {
            self.list = tree::find_list(&profile, nodes, self.tracker.tile_names());
            if self.list.is_some() {
                self.tracker.list_found();
            }
        }
        self.overlay = nodes
            .iter()
            .find(|node| node.has_class(&profile.overlay.class))
            .map(|node| node.element.clone());
        if self.list.is_none() && settings.auto_start_captions {
            self.press_captions_tab(&profile, nodes, moment)?;
        }
        let sight = WindowSight {
            list: self.list.as_ref().map(|(_, level)| *level),
            has_overlay: self.overlay.is_some(),
        };
        self.tracker.saw_window(sight, moment)
    }

    fn press_captions_tab(
        &mut self,
        profile: &LayoutProfile,
        nodes: &[Node],
        moment: Moment,
    ) -> std::io::Result<()> {
        if !self.tracker.claim_press_turn(moment.instant) {
            return Ok(());
        }
        let tab = nodes.iter().find(|node| {
            node.dom_id.as_deref() == Some(profile.captions_tab.id.as_str())
                && node.element.title().as_deref() == Some(profile.captions_tab.title.as_str())
        });
        let Some(tab) = tab else {
            return Ok(());
        };
        if tab.has_class(&profile.captions_tab.active_class) {
            return Ok(());
        }
        self.tracker.count_press(moment.instant);
        match tab.element.press() {
            Ok(()) => {
                tracing::info!("pressed the Captions tab");
                self.tracker.captions_tab_pressed(moment.seen_at)
            }
            Err(failure) => {
                tracing::warn!(ax.error = failure.0, "could not press the Captions tab");
                Ok(())
            }
        }
    }

    /// Reads the cached caption list, or the overlay while the list is missing. While the user
    /// has recording stopped, the rows are read so they count as seen, but not logged.
    pub(super) fn read_captions(&mut self, is_recording_stopped: bool) -> std::io::Result<()> {
        self.tracker
            .set_recording_stopped(is_recording_stopped, Timestamp::now())?;
        let Some(profile) = self.tracker.profile() else {
            return Ok(());
        };
        let moment = Moment::now();
        if let Some((list, level)) = self.list.clone() {
            match list.children() {
                Ok(rows) => {
                    self.tracker.flush_overlay()?;
                    let row_ids: Vec<Option<String>> = rows.iter().map(Element::dom_id).collect();
                    return self.tracker.saw_rows(
                        &row_ids,
                        |index| tree::row_sight(&profile, &rows[index], level),
                        moment.seen_at,
                    );
                }
                Err(failure) if failure.is_busy() => return Ok(()),
                Err(failure) => {
                    if !failure.is_gone() {
                        tracing::debug!(ax.error = failure.0, "caption list read failed");
                    }
                    self.list = None;
                    self.tracker.list_lost(moment.instant);
                }
            }
        }
        let lines = if let Some(Ok(elements)) = self.overlay.as_ref().map(Element::children) {
            elements
                .iter()
                .filter(|line| line.has_class(&profile.overlay.line_class))
                .filter_map(tree::overlay_line)
                .collect()
        } else {
            self.overlay = None;
            Vec::new()
        };
        self.tracker.saw_overlay(&lines, moment.seen_at)
    }
}

/// The participant tiles by Slack user id.
fn read_tiles(profile: &LayoutProfile, nodes: &[Node]) -> BTreeMap<String, Tile> {
    let mut seen = BTreeMap::new();
    for node in nodes {
        if node.role.as_deref() != Some(profile.tiles.role.as_str()) {
            continue;
        }
        let Some(rest) = node
            .dom_id
            .as_deref()
            .and_then(|id| id.strip_prefix(&profile.tiles.id_prefix))
        else {
            continue;
        };
        // A screen share tile says "View <name>'s screen" and is not a person.
        let Some(name) = node
            .description
            .as_deref()
            .and_then(|description| profile.tile_name(description))
        else {
            continue;
        };
        let (user_id, is_self) = match rest.strip_prefix(&profile.tiles.self_marker) {
            Some(user_id) => (user_id, true),
            None => (rest, false),
        };
        seen.insert(
            user_id.to_owned(),
            Tile {
                name: name.to_owned(),
                is_self,
            },
        );
    }
    seen
}
