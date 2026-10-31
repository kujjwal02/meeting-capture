//! When a failed capture process restarts, and when "Recording failed" shows. Std only, and
//! the caller passes the time, so the policy runs the same on any clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The wait before each restart while the process is not failing.
const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// This many failures within `FAILURE_WINDOW` make the process failing.
pub(super) const FAILURES_SHOWN: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// The restart delay once the process is failing.
const FAILING_RESTART_DELAY: Duration = Duration::from_secs(30);
/// The restart delay once the process is failing and a device would not open. A device that
/// is gone stays gone for a while, and each try costs up to 10 s of opens.
const DEVICE_FAILING_RESTART_DELAY: Duration = Duration::from_mins(5);

/// The failures of one process of one call.
#[derive(Debug, Default)]
pub(super) struct RestartBackoff {
    failures: VecDeque<Instant>,
    /// Set at `FAILURES_SHOWN` failures within `FAILURE_WINDOW`. Only a run that records clears
    /// it: failures 30 s or 5 minutes apart never fill the window again, and must not count as
    /// a recovery.
    is_failing: bool,
}

/// What to do after one failure.
#[derive(Debug, Clone, Copy)]
pub(super) struct Restart {
    pub(super) delay: Duration,
    pub(super) is_failing: bool,
    /// This failure made the process failing: show "Recording failed", once.
    pub(super) is_newly_failing: bool,
    pub(super) failures_in_window: usize,
}

impl RestartBackoff {
    pub(super) fn note_failure(&mut self, now: Instant, is_device_open_failed: bool) -> Restart {
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|failed_at| now.duration_since(*failed_at) > FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        let is_newly_failing = !self.is_failing && self.failures.len() >= FAILURES_SHOWN;
        self.is_failing |= is_newly_failing;
        let delay = match (self.is_failing, is_device_open_failed) {
            (true, true) => DEVICE_FAILING_RESTART_DELAY,
            (true, false) => FAILING_RESTART_DELAY,
            (false, _) => RESTART_DELAYS[self.failures.len().clamp(1, RESTART_DELAYS.len()) - 1],
        };
        Restart {
            delay,
            is_failing: self.is_failing,
            is_newly_failing,
            failures_in_window: self.failures.len(),
        }
    }

    /// A run delivered audio: the process works again.
    pub(super) fn note_recording(&mut self) {
        self.failures.clear();
        self.is_failing = false;
    }
}
