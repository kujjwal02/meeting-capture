//! The health verdict table and the settle timer that keeps a short warning from being reported.

use std::time::{Duration, Instant};

use sources::slack_huddle::HuddleHealth;

use super::panel::Panel;
use super::{ListMatch, WindowSight};
use crate::profile::LayoutProfile;

// Slack takes a few seconds to show captions in a new huddle.
const CAPTIONS_GRACE: Duration = Duration::from_secs(30);
// A warning must hold this long before it is reported. At huddle start the overlay shows a few
// seconds before the panel.
const WARNING_SETTLE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HealthChange {
    pub(crate) state: HuddleHealth,
    pub(crate) reason: String,
}

impl HealthChange {
    fn new(state: HuddleHealth, reason: impl Into<String>) -> Self {
        Self {
            state,
            reason: reason.into(),
        }
    }
}

#[derive(Debug)]
pub(super) struct Judge {
    started: Instant,
    captions_seen: Option<Instant>,
    health: Option<HuddleHealth>,
    pending_warning: Option<(HuddleHealth, Instant)>,
}

impl Judge {
    pub(super) fn new(started: Instant) -> Self {
        Self {
            started,
            captions_seen: None,
            health: None,
            pending_warning: None,
        }
    }

    /// `None` means no verdict yet.
    pub(super) fn no_profile_verdict(
        &self,
        slack_version: &str,
        now: Instant,
    ) -> Option<HealthChange> {
        (now.duration_since(self.started) >= CAPTIONS_GRACE).then(|| {
            HealthChange::new(
                HuddleHealth::Broken,
                format!(
                    "Can't read Slack {slack_version} huddle captions: no layout profile matches the huddle window"
                ),
            )
        })
    }

    /// `None` means no verdict yet.
    pub(super) fn window_verdict(
        &mut self,
        sight: WindowSight,
        panel: &Panel,
        has_tiles: bool,
        profile: &LayoutProfile,
        slack_version: &str,
        now: Instant,
    ) -> Option<HealthChange> {
        if sight.list.is_some() || sight.has_overlay {
            self.captions_seen = Some(now);
        }
        let quiet_for = now.duration_since(self.captions_seen.unwrap_or(self.started));
        let is_hidden_briefly = panel.is_hidden_briefly(now);
        match (sight.list, sight.has_overlay) {
            (None, _) if is_hidden_briefly => None,
            (None, _) if panel.has_shown() => Some(HealthChange::new(
                HuddleHealth::Degraded,
                "The caption panel is hidden, so lines may be missed. Open the Captions tab in the huddle",
            )),
            (None, false) if quiet_for < CAPTIONS_GRACE => None,
            (None, false) => Some(HealthChange::new(
                HuddleHealth::CaptionsOff,
                "Turn on captions to get a note, or tick Slack's \"Automatically turn on captions\" preference",
            )),
            (None, true) => Some(HealthChange::new(
                HuddleHealth::Degraded,
                "Only the caption overlay is visible, so lines may be missed. Open the Captions tab in the huddle",
            )),
            (Some(ListMatch::Structure), _) => Some(HealthChange::new(
                HuddleHealth::Degraded,
                format!(
                    "Slack {slack_version} changed the caption panel; it was found by its shape only"
                ),
            )),
            (Some(ListMatch::Semantic | ListMatch::Class), _) if !has_tiles => {
                Some(HealthChange::new(
                    HuddleHealth::Degraded,
                    "Speaker list not found: the note will have no attendees",
                ))
            }
            (Some(ListMatch::Semantic | ListMatch::Class), _)
                if !profile.is_tested_on(slack_version) =>
            {
                Some(HealthChange::new(
                    HuddleHealth::Untested,
                    format!(
                        "Reading captions with layout {} on Slack {slack_version}, which it was not tested on",
                        profile.id
                    ),
                ))
            }
            (Some(ListMatch::Semantic | ListMatch::Class), _) => {
                Some(HealthChange::new(HuddleHealth::Healthy, "Reading captions"))
            }
        }
    }

    /// Returns the verdict when it changes the health. A warning counts only once it held for
    /// `WARNING_SETTLE`; `None` means no verdict yet.
    pub(super) fn settle(
        &mut self,
        verdict: Option<HealthChange>,
        now: Instant,
    ) -> Option<HealthChange> {
        let Some(change) = verdict else {
            self.pending_warning = None;
            return None;
        };
        if self.health == Some(change.state) {
            self.pending_warning = None;
            return None;
        }
        if change.state.is_warning() {
            match self.pending_warning {
                Some((pending, since)) if pending == change.state => {
                    if now.duration_since(since) < WARNING_SETTLE {
                        return None;
                    }
                }
                Some(_) | None => {
                    self.pending_warning = Some((change.state, now));
                    return None;
                }
            }
        }
        self.pending_warning = None;
        self.health = Some(change.state);
        Some(change)
    }
}

pub(super) fn closed_window_verdict(panel: &Panel, now: Instant) -> Option<HealthChange> {
    // Slack keeps filling the closed window's panel, so a short absence loses nothing.
    (!panel.is_hidden_briefly(now)).then(|| {
        HealthChange::new(
            HuddleHealth::Degraded,
            "The huddle window is closed, so captions are not read. Open it from Slack's sidebar",
        )
    })
}
