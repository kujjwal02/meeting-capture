//! What every detector of a call reads besides its own lines: the call's facts, the global
//! facts, and the clock arithmetic that skips the time the host slept.

use jiff::{SignedDuration, Timestamp};

/// A call's platform, from `call.platform` or the first platform-only line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Meet,
    SlackHuddle,
}

impl Platform {
    #[must_use]
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "meet" => Some(Self::Meet),
            "slack_huddle" => Some(Self::SlackHuddle),
            _unknown => None,
        }
    }

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Meet => "meet",
            Self::SlackHuddle => "slack_huddle",
        }
    }
}

/// What every detector of one call reads.
#[derive(Debug, Clone, Default)]
pub struct CallFacts {
    /// "Stop recording this call" was chosen and "Resume recording" was not.
    pub is_stopped_by_user: bool,
    /// The Mac went to sleep and no audio segment opened since, nor did it wake.
    pub is_asleep: bool,
    pub platform: Option<Platform>,
}

/// A span the host slept through, from a `host.suspended` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuspendedSpan {
    pub start: Timestamp,
    pub end: Timestamp,
}

/// Facts that belong to no call.
#[derive(Debug, Clone)]
pub struct GlobalContext {
    /// Whether any mc-meet port is connected. Until a line says so, assume one is, so a
    /// disconnect in a bundle without `extension.connection` lines still notifies.
    pub is_extension_connected: bool,
    /// Oldest first.
    pub suspended: Vec<SuspendedSpan>,
}

impl Default for GlobalContext {
    fn default() -> Self {
        Self {
            is_extension_connected: true,
            suspended: Vec::new(),
        }
    }
}

/// The context a detector gets with each line and each clock step.
#[derive(Debug, Clone, Copy)]
pub struct CallContext<'a> {
    pub facts: &'a CallFacts,
    pub global: &'a GlobalContext,
}

impl CallContext<'_> {
    /// The time from `since` to `now` the host was awake. A sleep must not count as a stalled
    /// call: macOS stops nothing else the user sees while the lid is shut.
    #[must_use]
    pub fn elapsed(&self, since: Timestamp, now: Timestamp) -> SignedDuration {
        let mut elapsed = now.duration_since(since);
        for span in &self.global.suspended {
            let start = span.start.max(since);
            let end = span.end.min(now);
            if end > start {
                elapsed -= end.duration_since(start);
            }
        }
        elapsed.max(SignedDuration::ZERO)
    }

    /// The first time at which `elapsed(since, time)` reaches `limit`.
    #[must_use]
    pub fn deadline(&self, since: Timestamp, limit: SignedDuration) -> Timestamp {
        let mut cursor = since;
        let mut remaining = limit;
        for span in &self.global.suspended {
            if span.end <= cursor {
                continue;
            }
            let awake_until_span = span.start.duration_since(cursor).max(SignedDuration::ZERO);
            if awake_until_span >= remaining {
                break;
            }
            remaining -= awake_until_span;
            cursor = span.end;
        }
        later(cursor, remaining)
    }
}

/// `at` moved on by `duration`, held at the end of time.
#[must_use]
pub fn later(at: Timestamp, duration: SignedDuration) -> Timestamp {
    at.saturating_add(duration).unwrap_or(Timestamp::MAX)
}
