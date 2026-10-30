//! Opens the two call stems: the microphone, and the remote audio the system plays. Each source
//! takes channel 0 in its real-time callback and pushes it, with the buffer's host time, into a
//! lock-free ring. The caller drains the ring through a `StemReader` on an ordinary thread.

use std::time::Duration;

mod call;
#[cfg(target_os = "macos")]
mod mac;
mod mic;
mod permission;
mod ring;

pub use call::{CallAudioSource, CallReaders, OpenedStem};
pub use mic::{
    InputDevices, MicPick, MicPickReason, MicRequest, MicRule, MicStream, MicTransport,
    default_devices, mic_pick,
};
pub use permission::permission_state;
pub use ring::StemReader;
pub use stem_timing::{
    BufferStamp, MicMoment, PermissionService, PermissionState, Stem, StemLosses, UtcAnchor,
};

#[cfg(target_os = "macos")]
pub use mac::clock::{host_now_ns, utc_anchor};
#[cfg(all(target_os = "macos", debug_assertions))]
pub use mac::hal::inject_hal_changes;
#[cfg(target_os = "macos")]
pub use mac::hal::{
    BluetoothRoute, HalChanges, HalDevice, bluetooth_route, hal_devices, take_hal_changes,
    watch_hal_changes,
};
#[cfg(target_os = "macos")]
pub use mac::power::IdleSleepBlock;
#[cfg(all(target_os = "macos", debug_assertions))]
pub use mac::sleep::inject_powered_on;
#[cfg(target_os = "macos")]
pub use mac::sleep::{PowerEvent, SleepAck, is_lid_closed, watch_system_power};
#[cfg(all(target_os = "macos", debug_assertions))]
pub use mac::tap::probe_device_list_listener;
#[cfg(target_os = "macos")]
pub use mac::tap::{RemoteSource, TapHealth};

/// About 1.4 s at 48 kHz: room for the writer to stall on a slow disk without dropping samples.
pub const RING_CAPACITY_SAMPLES: usize = 65_536;

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
    /// The mic rule allows no input: mid-call, every input left is Bluetooth or unreadable, or
    /// the one left is the skipped built-in mic.
    #[error("no microphone the rule allows at {moment}; the default input is {default_name:?}")]
    NoSafeMicrophone {
        default_name: String,
        moment: MicMoment,
    },
    /// The only input is a virtual device, which carries another app's audio, not a voice.
    #[error("the only microphone is the virtual device {default_name:?}")]
    OnlyVirtualMicrophone { default_name: String },
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
    #[error("IOKit refused the idle sleep assertion with code {code}")]
    PowerAssertion { code: i32 },
    #[error("could not listen for system sleep and wake: {reason}")]
    PowerWatch { reason: String },
}
