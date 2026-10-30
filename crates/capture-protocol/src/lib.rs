//! The JSON lines the app and the `mc-capture audio` process exchange: commands on the helper's
//! stdin, reports on its stdout. Each line is one object with a `seq` and a `type`, keys sorted.
//! Every kind is a closed enum, so a line is validated once, when it is read; an unknown `type`
//! or a missing field fails there. Both sides build from this crate, so they cannot drift.

use std::fmt;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
pub use sources::Platform;
pub use stem_timing::{MicMoment, PermissionService, PermissionState, Stem};
pub use stem_writer::GapCause;

/// One line on the wire: a message and its sequence number. Each side numbers its own lines
/// from 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line<M> {
    pub seq: u64,
    #[serde(flatten)]
    pub message: M,
}

/// App to helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioCommand {
    /// Record segment `segment` of the call into `folder`. The app picks the number: one above
    /// every segment in the folder.
    Start {
        call_id: String,
        folder: PathBuf,
        segment: u32,
        platform: Platform,
        /// `call_start` for the call's first segment only. Later segments pick the mic as
        /// mid-call, so they never open a Bluetooth mic.
        mic_moment: MicMoment,
    },
    /// Close the segment and wait for the next command. Closing stdin also stops, and then the
    /// helper exits. Closing stdin stops within 1 s once its devices are open.
    Stop,
}

/// Helper to app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioReport {
    /// A stream delivered its first buffer into the segment.
    Recording {
        segment: u32,
        stream: Stem,
        device: String,
        rate_hz: u32,
        first_host_time_ns: u64,
    },
    /// The writer filled a span with silence.
    Gap {
        segment: u32,
        stream: Stem,
        start_ms: f64,
        start_host_time_ns: u64,
        length_ms: f64,
        cause: GapCause,
    },
    /// Every 5 s while a segment is open. The app restarts a helper whose counters stall while
    /// its stem files stop growing.
    Heartbeat {
        segment: u32,
        mic: Option<StreamCounters>,
        remote: Option<StreamCounters>,
    },
    /// A stem reached the 24-hour limit, so both stems moved to the next segment.
    Rotated { from_segment: u32, to_segment: u32 },
    /// A stream moved to another device mid-segment. The time between the two devices is a
    /// `device_switch` gap. `old_device` is `None` when the segment had no mic before.
    DeviceChanged {
        segment: u32,
        stream: Stem,
        old_device: Option<String>,
        new_device: String,
        rate_hz: u32,
        cause: DeviceChangeCause,
    },
    /// The mic went away, or a segment started, with no safe input left, so the mic stem
    /// records a gap. The helper tries again every 15 s and at each device change; a later
    /// `device_changed` on the mic ends the gap. `old_device` and `cause` are `None` when the
    /// segment started with no mic.
    MicUnavailable {
        segment: u32,
        old_device: Option<String>,
        cause: Option<DeviceChangeCause>,
        message: String,
    },
    /// A permission's state, read at each segment start and after a failure that a missing
    /// permission explains.
    Permission {
        service: PermissionService,
        state: PermissionState,
    },
    /// The Mac is about to sleep. `segment` is the one this closed for it, already reported
    /// `stopped`; `None` when none was open. The helper then waits for commands.
    SystemSleep { segment: Option<u32> },
    /// The Mac woke. The app answers with `start` for the call's next segment.
    SystemWake,
    /// The segment is closed and `audio.json` marks it so.
    Stopped { segment: u32 },
    Error {
        kind: AudioErrorKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCounters {
    /// Device samples taken from the ring, at the device's rate.
    pub samples_in: u64,
    /// 16 kHz samples in the stem file, inserted silence included.
    pub samples_written: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioErrorKind {
    /// A stdin line that is not a valid command, or a command that does not fit the state.
    BadCommand,
    /// The segment could not start: its number is taken, or the folder is unwritable.
    SegmentStartFailed,
    /// A device did not open within its 5 s limit, or failed to open. The helper exits.
    DeviceOpenFailed,
    /// A write to the segment failed. The helper closes the segment and exits.
    WriteFailed,
    /// One stem stopped; the other keeps recording.
    StemStopped,
    /// A sync or a store of `audio.json` failed. The next checkpoint retries it.
    CheckpointFailed,
    /// The segment could not be marked closed. Launch repair finds it open.
    SegmentCloseFailed,
    /// Idle sleep could not be held off. Recording goes on.
    PowerAssertionFailed,
    /// The device listeners or the sleep watcher could not register. Recording goes on, blind
    /// to device changes or sleep.
    WatchFailed,
    /// The tap needed a fifth rebuild within one episode. The helper closes the segment and
    /// exits, so the app restarts it.
    TapRebuildLimit,
}

/// Why a stream moved to another device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceChangeCause {
    /// The mic rule picks another input after a device change.
    MicPickChanged,
    /// The mic in use went away or changed its rate under the stream.
    MicLost,
    /// The built-in mic delivered only exact zeros for 10 s while the lid was closed: macOS
    /// mutes it then.
    MicLidClosed,
    /// The tap's aggregate device died.
    TapAggregateDied,
    /// The tap's format changed its rate.
    TapFormatChanged,
    /// The tap delivered buffers at another rate than its format said.
    TapRateMismatch,
    /// The tap delivered nothing after an output route change.
    TapStaleAfterRouteChange,
    /// The tap delivered nothing for 5 s while open.
    TapStalled,
}

impl fmt::Display for AudioErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BadCommand => "bad_command",
            Self::SegmentStartFailed => "segment_start_failed",
            Self::DeviceOpenFailed => "device_open_failed",
            Self::WriteFailed => "write_failed",
            Self::StemStopped => "stem_stopped",
            Self::CheckpointFailed => "checkpoint_failed",
            Self::SegmentCloseFailed => "segment_close_failed",
            Self::PowerAssertionFailed => "power_assertion_failed",
            Self::WatchFailed => "watch_failed",
            Self::TapRebuildLimit => "tap_rebuild_limit",
        })
    }
}

impl fmt::Display for DeviceChangeCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MicPickChanged => "mic_pick_changed",
            Self::MicLost => "mic_lost",
            Self::MicLidClosed => "mic_lid_closed",
            Self::TapAggregateDied => "tap_aggregate_died",
            Self::TapFormatChanged => "tap_format_changed",
            Self::TapRateMismatch => "tap_rate_mismatch",
            Self::TapStaleAfterRouteChange => "tap_stale_after_route_change",
            Self::TapStalled => "tap_stalled",
        })
    }
}

/// One line of JSON with sorted keys and no newline.
///
/// # Errors
///
/// Fails only when the message holds a float that is not finite.
pub fn encode<M: Serialize>(seq: u64, message: &M) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(Line { seq, message })?;
    // Some builds enable serde_json's `preserve_order`, which keeps declaration order instead.
    value.sort_all_objects();
    serde_json::to_string(&value)
}

/// Reads one line. Anything outside the closed kinds fails here.
///
/// # Errors
///
/// Fails when the line is not JSON, has no `seq`, or holds an unknown `type` or field value.
pub fn decode<M: DeserializeOwned>(line: &str) -> Result<Line<M>, serde_json::Error> {
    serde_json::from_str(line)
}
