//! The types `capture` and `stem-writer` share: which stem, where each device buffer sits in host
//! time, the ring's losses and the host clock's reading against UTC. Portable: no audio API.

use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stem {
    Mic,
    Remote,
}

impl Stem {
    pub const ALL: [Stem; 2] = [Stem::Mic, Stem::Remote];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::Remote => "remote",
        }
    }
}

impl fmt::Display for Stem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// When one device buffer started, and where its samples sit in the reader's sample stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferStamp {
    /// Host clock time of the buffer's first sample, as the OS hands it to the callback, with no
    /// latency correction. Both stems use the same clock. On macOS it is mach host time in
    /// nanoseconds since boot, sleep excluded. On Windows it will be QPC time in nanoseconds.
    pub host_time_ns: u64,
    /// Samples the reader received before this buffer's first sample.
    pub sample_index: u64,
    /// Frames the device delivered in this buffer, dropped ones included.
    pub frame_count: u32,
    /// Frames of this buffer that reached the ring. A full ring drops the rest, the buffer's tail.
    pub accepted_count: u32,
    /// The rate the device delivered this buffer at.
    pub sample_rate_hz: u32,
}

/// Losses a stem ring counts while it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StemLosses {
    /// Samples the callback could not push because the sample ring was full.
    pub dropped_samples: u64,
    /// Stamps the callback could not push because the stamp ring was full.
    pub dropped_stamps: u64,
    /// Buffers that arrived without a valid host time, so they have no stamp.
    pub untimed_buffers: u64,
}

/// One reading of the host clock against UTC, to turn host times into wall-clock times.
#[derive(Debug, Clone, Copy)]
pub struct UtcAnchor {
    /// Midpoint of the two host clock reads around the UTC read.
    pub host_time_ns: u64,
    pub utc: SystemTime,
    /// Host time between the two reads: the anchor's error bound.
    pub read_gap_ns: u64,
}
