//! Opens the two call stems: the microphone, and the remote audio the system plays. Each source
//! takes channel 0 in its real-time callback and pushes it, with the buffer's host time, into a
//! lock-free ring. The caller drains the ring through a `StemReader` on an ordinary thread.

use std::fmt;
use std::time::Duration;

mod call;
#[cfg(target_os = "macos")]
mod mac;
mod mic;
mod ring;

pub use call::{CallAudioSource, CallReaders, OpenedStem};
pub use mic::{
    InputDevices, MicPick, MicPickReason, MicRule, MicTransport, default_devices, mic_pick,
};
pub use ring::{BufferStamp, StemLosses, StemReader};

#[cfg(target_os = "macos")]
pub use mac::clock::{UtcAnchor, host_now_ns, utc_anchor};
#[cfg(target_os = "macos")]
pub use mac::hal::{HalDevice, hal_devices};
#[cfg(target_os = "macos")]
pub use mac::tap::RemoteSource;

/// About 1.4 s at 48 kHz: room for the writer to stall on a slow disk without dropping samples.
pub const RING_CAPACITY_SAMPLES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stem {
    Mic,
    Remote,
}

impl Stem {
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

/// The format a source delivers into its ring: always mono f32, at the device's own rate.
#[derive(Debug, Clone)]
pub struct StemFormat {
    pub sample_rate_hz: u32,
    /// Channels the device delivers. The ring keeps channel 0.
    pub device_channels: u16,
    pub device_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no microphone is available")]
    NoMicrophone,
    #[error("could not open the microphone at {step}: {reason}")]
    Microphone { step: &'static str, reason: String },
    #[error("Core Audio call {call} failed with OSStatus {os_status}")]
    CoreAudio { call: &'static str, os_status: i32 },
    #[error(
        "the system audio tap delivers {bits_per_channel}-bit {sample_kind} audio; only 32-bit float is read"
    )]
    UnsupportedTapFormat {
        bits_per_channel: u32,
        sample_kind: &'static str,
    },
    #[error("the {stem} stem is already open")]
    AlreadyOpen { stem: Stem },
    #[error("an earlier open of the {stem} stem has not returned yet")]
    OpenPending { stem: Stem },
    #[error("the {stem} stem did not open within {} s", open_limit.as_secs())]
    OpenTimedOut { stem: Stem, open_limit: Duration },
    #[error("the thread opening the {stem} stem stopped without an answer")]
    OpenThreadDied { stem: Stem },
    #[error("could not start a thread to open the {stem} stem: {reason}")]
    OpenThreadSpawn { stem: Stem, reason: String },
}
