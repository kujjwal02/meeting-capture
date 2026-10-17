//! Whether the caption panel shows, and when to press the Captions tab to show it.

use std::time::{Duration, Instant};

use super::ListFind;

const FIRST_RETRY_AFTER: Duration = Duration::from_secs(10);
// Once the panel was shown, a hidden panel means the user picked another tab, such as Thread.
// Slack keeps filling the hidden panel, so nothing is lost meanwhile.
const HIDDEN_PANEL_PATIENCE: Duration = Duration::from_mins(2);

#[derive(Debug, Default)]
pub(super) struct Panel {
    has_shown: bool,
    hidden_since: Option<Instant>,
    presses: u32,
    pressed_at: Option<Instant>,
}

impl Panel {
    pub(super) fn found(&mut self) -> ListFind {
        let find = if self.has_shown {
            ListFind::Again
        } else {
            ListFind::First
        };
        self.has_shown = true;
        self.hidden_since = None;
        find
    }

    pub(super) fn lost(&mut self, now: Instant) {
        self.hidden_since = Some(now);
    }

    pub(super) fn has_shown(&self) -> bool {
        self.has_shown
    }

    pub(super) fn is_hidden_briefly(&self, now: Instant) -> bool {
        self.hidden_since
            .is_some_and(|since| now.duration_since(since) < HIDDEN_PANEL_PATIENCE)
    }

    // Until the panel first shows: one press, one retry after 10 s, then one every 2 minutes.
    // After that, a press waits until the panel has been hidden for 2 minutes, so a user who
    // picked the Thread tab keeps it for a while.
    pub(super) fn claim_press_turn(&mut self, now: Instant) -> bool {
        if self.has_shown {
            if self
                .hidden_since
                .is_none_or(|since| now.duration_since(since) < HIDDEN_PANEL_PATIENCE)
            {
                return false;
            }
            // Restarts the wait, so the next press is 2 minutes after this one.
            self.hidden_since = Some(now);
            return true;
        }
        let wait = if self.presses < 2 {
            FIRST_RETRY_AFTER
        } else {
            HIDDEN_PANEL_PATIENCE
        };
        self.pressed_at
            .is_none_or(|at| now.duration_since(at) >= wait)
    }

    pub(super) fn count_press(&mut self, now: Instant) {
        self.presses += 1;
        self.pressed_at = Some(now);
    }
}
