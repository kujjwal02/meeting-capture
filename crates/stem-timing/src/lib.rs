//! The types `capture`, `stem-writer` and `capture-protocol` share: which stem, where each
//! device buffer sits in host time, the ring's losses, the host clock's reading against UTC, and
//! the privacy permissions recording needs, and when in a call the mic is picked. Portable: no
//! audio API.

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

/// A macOS privacy permission recording needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionService {
    Microphone,
    /// "System Audio Recording Only", which the tap needs.
    AudioCapture,
    /// "Screen & System Audio Recording", which window video needs. Read with
    /// `CGPreflightScreenCaptureAccess`, which never prompts.
    ScreenRecording,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    /// The user has not answered yet. The first open asks.
    NotDetermined,
    /// A profile or parental control blocks it; the user cannot grant it.
    Restricted,
    /// The state cannot be read, as when the private TCC call is missing or answers neither yes
    /// nor no.
    Unknown,
}

impl PermissionService {
    /// The services the audio process needs.
    pub const AUDIO: [PermissionService; 2] = [
        PermissionService::Microphone,
        PermissionService::AudioCapture,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::AudioCapture => "audio_capture",
            Self::ScreenRecording => "screen_recording",
        }
    }
}

impl PermissionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NotDetermined => "not_determined",
            Self::Restricted => "restricted",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for PermissionService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for PermissionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// When the mic rule picks. The call's first segment is its start; every later segment, after
/// a wake, a rotation or a helper restart, is mid-call. Mid-call, the avoid-Bluetooth rule never
/// falls back to a Bluetooth, virtual or unreadable input: the stem records no mic rather than
/// move a headset to its hands-free profile in the middle of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicMoment {
    CallStart,
    MidCall,
}

impl MicMoment {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallStart => "call_start",
            Self::MidCall => "mid_call",
        }
    }
}

impl fmt::Display for MicMoment {
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

impl std::ops::Add for StemLosses {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            dropped_samples: self.dropped_samples + other.dropped_samples,
            dropped_stamps: self.dropped_stamps + other.dropped_stamps,
            untimed_buffers: self.untimed_buffers + other.untimed_buffers,
        }
    }
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
