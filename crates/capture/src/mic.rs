use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;

use crate::ring::stem_ring;
use crate::{CaptureError, StemFormat, StemReader};

/// Which input device records the mic stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicRule {
    /// The system default input, whatever it is.
    FollowDefault,
    /// The system default input, unless it is Bluetooth: then the built-in mic. Opening a
    /// Bluetooth headset's mic switches it to the hands-free profile, which drops its playback to
    /// narrowband audio for everyone on the call.
    AvoidBluetooth,
}

/// How an input device connects, as far as the mic rule cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicTransport {
    BuiltIn,
    /// Classic Bluetooth or Bluetooth LE.
    Bluetooth,
    /// An aggregate device, such as our own private tap aggregate.
    Aggregate,
    Other,
    /// The transport could not be read. `AvoidBluetooth` treats it as Bluetooth.
    Unknown,
}

/// Why the mic rule chose the device it chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicPickReason {
    /// `FollowDefault` takes the default input as it is.
    RuleFollowsDefault,
    /// The default input is not Bluetooth.
    DefaultIsSafe,
    /// The default input is Bluetooth, so the built-in mic records instead.
    DefaultIsBluetooth,
    /// The default input's transport is unreadable, so the built-in mic records instead.
    DefaultTransportUnknown,
    /// The default input is Bluetooth or unreadable and no built-in mic exists, so the first
    /// wired input records instead.
    NoBuiltInMic,
    /// The default input is Bluetooth or unreadable and no other input qualifies, as on a
    /// closed-lid Mac mini with only a headset.
    NoAlternativeInput,
}

impl MicRule {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FollowDefault => "follow_default",
            Self::AvoidBluetooth => "avoid_bluetooth",
        }
    }
}

impl MicTransport {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::Bluetooth => "bluetooth",
            Self::Aggregate => "aggregate",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }

    /// Whether `AvoidBluetooth` may record from a device with this transport when the built-in
    /// mic is missing.
    fn is_wired_alternative(self) -> bool {
        match self {
            Self::BuiltIn | Self::Other => true,
            Self::Bluetooth | Self::Aggregate | Self::Unknown => false,
        }
    }
}

impl MicPickReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuleFollowsDefault => "rule_follows_default",
            Self::DefaultIsSafe => "default_is_safe",
            Self::DefaultIsBluetooth => "default_is_bluetooth",
            Self::DefaultTransportUnknown => "default_transport_unknown",
            Self::NoBuiltInMic => "no_built_in_mic",
            Self::NoAlternativeInput => "no_alternative_input",
        }
    }
}

macro_rules! display_as_str {
    ($($kind:ty),*) => {$(
        impl std::fmt::Display for $kind {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    )*};
}
display_as_str!(MicRule, MicTransport, MicPickReason);

/// The mic rule's decision, made without opening any device.
#[derive(Debug, Clone, Serialize)]
pub struct MicPick {
    pub rule: MicRule,
    pub reason: MicPickReason,
    pub device_name: Option<String>,
    pub device_uid: Option<String>,
    pub transport: MicTransport,
    pub default_input_name: Option<String>,
    pub default_input_transport: MicTransport,
}

/// Keeps the input stream open. Dropping it stops the stream.
pub(crate) struct MicSource {
    stream: Option<cpal::Stream>,
}

#[derive(Debug, Clone, Default)]
pub struct InputDevices {
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[must_use]
pub fn default_devices() -> InputDevices {
    let host = cpal::default_host();
    InputDevices {
        default_input: host.default_input_device().and_then(|d| device_name(&d)),
        default_output: host.default_output_device().and_then(|d| device_name(&d)),
    }
}

impl MicSource {
    /// Opens the mic as mono f32 at the device's default rate.
    pub(crate) fn start(rule: MicRule) -> Result<(Self, StemReader), CaptureError> {
        let (device, pick) = pick_device(rule)?;
        tracing::info!(
            device.name = pick.device_name.as_deref(),
            mic.transport = %pick.transport,
            mic.rule = %pick.rule,
            mic.reason = %pick.reason,
            "mic.chosen"
        );
        let config = device
            .default_input_config()
            .map_err(|e| microphone_error("default_input_config", &e))?;
        let stream_config = cpal::StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let format = StemFormat {
            sample_rate_hz: config.sample_rate(),
            device_channels: config.channels(),
            device_name: device_name(&device).unwrap_or_default(),
        };
        let sample_rate_hz = config.sample_rate();
        let (mut pusher, reader) = stem_ring(format);
        let stream = device
            .build_input_stream::<f32, _, _>(
                stream_config,
                move |samples, callback_info: &cpal::InputCallbackInfo| {
                    let host_time_ns =
                        u64::try_from(first_sample_instant(callback_info).as_nanos())
                            .ok()
                            .filter(|&nanos| nanos != 0);
                    pusher.push_buffer(samples, host_time_ns, sample_rate_hz);
                },
                |error| tracing::warn!(exception.message = %error, "mic.stream_error"),
                None,
            )
            .map_err(|e| microphone_error("build_input_stream", &e))?;
        stream.play().map_err(|e| microphone_error("play", &e))?;
        Ok((
            Self {
                stream: Some(stream),
            },
            reader,
        ))
    }

    /// Stops the callbacks. The reader still holds what they pushed.
    pub(crate) fn stop(&mut self) {
        drop(self.stream.take());
    }
}

/// Decides which input device the mic rule picks, without opening it.
///
/// # Errors
///
/// Fails when no input device exists or the device list cannot be read.
pub fn mic_pick(rule: MicRule) -> Result<MicPick, CaptureError> {
    pick_device(rule).map(|(_, pick)| pick)
}

fn pick_device(rule: MicRule) -> Result<(Device, MicPick), CaptureError> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .ok_or(CaptureError::NoMicrophone)?;
    let default_transport = transport(&default);
    let (device, reason) = match (rule, default_transport) {
        (MicRule::FollowDefault, _) => (default.clone(), MicPickReason::RuleFollowsDefault),
        (
            MicRule::AvoidBluetooth,
            MicTransport::BuiltIn | MicTransport::Aggregate | MicTransport::Other,
        ) => (default.clone(), MicPickReason::DefaultIsSafe),
        (MicRule::AvoidBluetooth, MicTransport::Bluetooth) => {
            pick_alternative(&host, &default, MicPickReason::DefaultIsBluetooth)?
        }
        (MicRule::AvoidBluetooth, MicTransport::Unknown) => {
            pick_alternative(&host, &default, MicPickReason::DefaultTransportUnknown)?
        }
    };
    let pick = MicPick {
        rule,
        reason,
        device_name: device_name(&device),
        device_uid: device_uid(&device),
        transport: transport(&device),
        default_input_name: device_name(&default),
        default_input_transport: default_transport,
    };
    Ok((device, pick))
}

/// The built-in mic, else the first wired input, else the default when nothing else qualifies.
fn pick_alternative(
    host: &cpal::Host,
    default: &Device,
    built_in_reason: MicPickReason,
) -> Result<(Device, MicPickReason), CaptureError> {
    let inputs: Vec<(Device, MicTransport)> = host
        .input_devices()
        .map_err(|e| microphone_error("input_devices", &e))?
        .map(|device| {
            let transport = transport(&device);
            (device, transport)
        })
        .collect();
    let built_in = inputs
        .iter()
        .find(|(_, transport)| *transport == MicTransport::BuiltIn);
    let wired = inputs
        .iter()
        .find(|(_, transport)| transport.is_wired_alternative());
    Ok(match (built_in, wired) {
        (Some((built_in, _)), _) => (built_in.clone(), built_in_reason),
        (None, Some((wired, _))) => (wired.clone(), MicPickReason::NoBuiltInMic),
        (None, None) => (default.clone(), MicPickReason::NoAlternativeInput),
    })
}

fn device_uid(device: &Device) -> Option<String> {
    device.id().ok().map(|id| id.id().to_owned())
}

/// On macOS cpal reports no Bluetooth interface type, so the transport comes from the HAL. cpal
/// 0.18.2's device ID there is the Core Audio device UID, which the HAL resolves to the same
/// device.
#[cfg(target_os = "macos")]
fn transport(device: &Device) -> MicTransport {
    use cidre::core_audio::DeviceTransportType;

    let Some(uid) = device_uid(device) else {
        return MicTransport::Unknown;
    };
    match crate::mac::hal::transport_type(&uid) {
        Some(DeviceTransportType::BUILT_IN) => MicTransport::BuiltIn,
        Some(DeviceTransportType::BLUETOOTH | DeviceTransportType::BLUETOOTH_LE) => {
            MicTransport::Bluetooth
        }
        Some(DeviceTransportType::AGGREGATE) => MicTransport::Aggregate,
        Some(DeviceTransportType::UNKNOWN) | None => MicTransport::Unknown,
        // Core Audio's transport type is an open set of four-char codes.
        Some(_) => MicTransport::Other,
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(
    clippy::match_same_arms,
    reason = "Unknown is named on its own so a reader sees it; the wildcard covers variants cpal adds later"
)]
fn transport(device: &Device) -> MicTransport {
    use cpal::InterfaceType;

    match device.description().ok().map(|d| d.interface_type()) {
        Some(InterfaceType::BuiltIn) => MicTransport::BuiltIn,
        Some(InterfaceType::Bluetooth) => MicTransport::Bluetooth,
        Some(InterfaceType::Aggregate) => MicTransport::Aggregate,
        Some(
            InterfaceType::Usb
            | InterfaceType::Pci
            | InterfaceType::FireWire
            | InterfaceType::Thunderbolt
            | InterfaceType::Hdmi
            | InterfaceType::Line
            | InterfaceType::Spdif
            | InterfaceType::Network
            | InterfaceType::Virtual
            | InterfaceType::DisplayPort,
        ) => MicTransport::Other,
        Some(InterfaceType::Unknown) | None => MicTransport::Unknown,
        // `InterfaceType` is non-exhaustive, so a variant added upstream needs this arm.
        Some(_) => MicTransport::Unknown,
    }
}

/// The host time of the buffer's first sample.
#[cfg(target_os = "macos")]
fn first_sample_instant(callback_info: &cpal::InputCallbackInfo) -> cpal::StreamInstant {
    // In cpal 0.18.2's Core Audio backend, `callback` is the input buffer's mHostTime converted
    // with mach_timebase_info: the tap's clock and meaning. `capture` also subtracts one buffer
    // plus the device latency, which the tap stamp does not.
    callback_info.timestamp().callback
}

/// The host time of the buffer's first sample.
#[cfg(not(target_os = "macos"))]
fn first_sample_instant(callback_info: &cpal::InputCallbackInfo) -> cpal::StreamInstant {
    // In cpal 0.18.2's WASAPI backend, `capture` is the QPC position GetBuffer reports for the
    // buffer's first sample. `callback` is the clock position when the callback ran.
    callback_info.timestamp().capture
}

fn microphone_error(step: &'static str, error: &impl std::fmt::Display) -> CaptureError {
    CaptureError::Microphone {
        step,
        reason: error.to_string(),
    }
}

fn device_name(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}
