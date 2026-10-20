//! Opens the two call stems: the microphone, and the remote audio the system plays. Each source
//! downmixes to mono in its real-time callback and pushes into a lock-free ring. The caller drains
//! the ring through a `StemReader` on an ordinary thread.

#[cfg(target_os = "macos")]
mod mac;
mod mic;
mod ring;

pub use mic::{InputDevices, MicRule, MicSource, default_devices};
pub use ring::StemReader;

#[cfg(target_os = "macos")]
pub use mac::tap::RemoteSource;

/// About 1.4 s at 48 kHz: room for the writer to stall on a slow disk without dropping samples.
pub const RING_CAPACITY_SAMPLES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stem {
    Mic,
    Remote,
}

/// The format a source delivers into its ring: always mono f32, at the device's own rate.
#[derive(Debug, Clone)]
pub struct StemFormat {
    pub sample_rate_hz: u32,
    /// Channels the device delivers before the downmix.
    pub device_channels: u16,
    pub device_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no microphone is available")]
    NoMicrophone,
    #[error("could not open the microphone: {0}")]
    Microphone(String),
    #[error("could not open the system audio tap: {0}")]
    Tap(String),
    #[error(
        "the system audio tap delivers {bits_per_channel}-bit {sample_kind} audio; only 32-bit float is read"
    )]
    UnsupportedTapFormat {
        bits_per_channel: u32,
        sample_kind: &'static str,
    },
}
