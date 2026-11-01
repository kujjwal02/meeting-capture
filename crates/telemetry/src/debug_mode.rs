//! Debug mode: every process's log file at DEBUG until a set time. The settings file holds the
//! time as `[debug] enabled_until`; the app passes it on to `mc-capture` and mc-meet. Each
//! process turns its own file level with a [`DebugModeSwitch`].

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp, Unit};

use crate::init::{FileLevel, FileLevelHandle};

/// How long debug mode stays on after the tray turns it on.
pub const DEBUG_MODE_LENGTH: SignedDuration = SignedDuration::from_hours(24);

/// How often a process that cannot hear a change rereads the setting. Expiry is checked as
/// often, so debug mode ends at most this late.
pub const DEBUG_MODE_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugMode {
    Off,
    /// The log file keeps DEBUG lines until `until`, UTC.
    On {
        until: Timestamp,
    },
}

/// A stored `enabled_until` that is not a time.
#[derive(Debug, thiserror::Error)]
#[error("{value:?} is not an RFC 3339 time such as \"2026-11-01T04:10:00Z\"")]
pub struct DebugModeError {
    pub value: String,
}

impl DebugMode {
    /// On for [`DEBUG_MODE_LENGTH`] from `now`, to the second.
    #[must_use]
    pub fn turned_on_at(now: Timestamp) -> Self {
        let until = now
            .round(Unit::Second)
            .unwrap_or(now)
            .checked_add(DEBUG_MODE_LENGTH)
            .unwrap_or(Timestamp::MAX);
        Self::On { until }
    }

    /// Reads the stored `enabled_until`, RFC 3339. Absent is off. A past time is `On` still;
    /// [`DebugMode::at`] turns it off. A time more than [`DEBUG_MODE_LENGTH`] after `now` reads
    /// as `now` plus that length: a hand edit must not keep DEBUG files growing for weeks.
    ///
    /// # Errors
    ///
    /// Fails when the value is not an RFC 3339 time.
    pub fn from_enabled_until(
        enabled_until: Option<&str>,
        now: Timestamp,
    ) -> Result<Self, DebugModeError> {
        let Some(value) = enabled_until else {
            return Ok(Self::Off);
        };
        let until = value.parse::<Timestamp>().map_err(|_| DebugModeError {
            value: value.to_owned(),
        })?;
        let latest = now
            .round(Unit::Second)
            .unwrap_or(now)
            .checked_add(DEBUG_MODE_LENGTH)
            .unwrap_or(Timestamp::MAX);
        Ok(Self::On {
            until: until.min(latest),
        })
    }

    /// The value to store or send: RFC 3339 UTC, or `None` when off.
    #[must_use]
    pub fn enabled_until(self) -> Option<String> {
        match self {
            Self::Off => None,
            Self::On { until } => Some(until.to_string()),
        }
    }

    #[must_use]
    pub fn is_on(self, now: Timestamp) -> bool {
        match self {
            Self::Off => false,
            Self::On { until } => now < until,
        }
    }

    /// The mode in effect at `now`: off once `until` has passed.
    #[must_use]
    pub fn at(self, now: Timestamp) -> Self {
        if self.is_on(now) { self } else { Self::Off }
    }

    #[must_use]
    pub fn file_level(self, now: Timestamp) -> FileLevel {
        if self.is_on(now) {
            FileLevel::Debug
        } else {
            FileLevel::Info
        }
    }
}

/// Sets this process's log file level from the debug mode, and logs each change.
pub struct DebugModeSwitch {
    file_level: FileLevelHandle,
    applied: Mutex<FileLevel>,
}

impl DebugModeSwitch {
    /// `init` starts the file at INFO.
    #[must_use]
    pub fn new(file_level: FileLevelHandle) -> Self {
        Self {
            file_level,
            applied: Mutex::new(FileLevel::Info),
        }
    }

    /// Sets the file level for `mode` at `now`. Returns the mode in effect.
    pub fn apply(&self, mode: DebugMode, now: Timestamp) -> DebugMode {
        let in_effect = mode.at(now);
        let level = in_effect.file_level(now);
        let mut applied = self.applied.lock().unwrap_or_else(PoisonError::into_inner);
        if *applied == level {
            return in_effect;
        }
        if let Err(error) = self.file_level.set(level) {
            tracing::warn!(exception.message = %error, "could not set the log file level");
            return in_effect;
        }
        *applied = level;
        let enabled_until = in_effect.enabled_until();
        match level {
            FileLevel::Debug => tracing::info!(
                mc.debug_mode.until = enabled_until.as_deref(),
                "debug mode on: the log file keeps DEBUG lines"
            ),
            FileLevel::Info => tracing::info!("debug mode off: the log file keeps INFO lines"),
        }
        in_effect
    }

    /// [`DebugModeSwitch::apply`] at the current time.
    pub fn apply_now(&self, mode: DebugMode) -> DebugMode {
        self.apply(mode, Timestamp::now())
    }
}

/// Follows a debug mode that only a reread can show, as the settings file is for the native
/// host. Rereads at most every [`DEBUG_MODE_RECHECK_INTERVAL`].
pub struct DebugModeFollower<R: FnMut() -> DebugMode> {
    switch: DebugModeSwitch,
    read: R,
    read_at: Option<Instant>,
}

impl<R: FnMut() -> DebugMode> DebugModeFollower<R> {
    #[must_use]
    pub fn new(file_level: FileLevelHandle, read: R) -> Self {
        Self {
            switch: DebugModeSwitch::new(file_level),
            read,
            read_at: None,
        }
    }

    /// Rereads the mode when the last read is older than the interval, then applies it. Cheap
    /// between rereads: one clock read.
    pub fn follow(&mut self) {
        let is_due = self
            .read_at
            .is_none_or(|read_at| read_at.elapsed() >= DEBUG_MODE_RECHECK_INTERVAL);
        if !is_due {
            return;
        }
        self.read_at = Some(Instant::now());
        let mode = (self.read)();
        self.switch.apply_now(mode);
    }
}
