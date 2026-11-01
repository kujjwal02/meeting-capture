use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;

use crate::ring::stem_ring;
use crate::{CaptureError, MicMoment, MicPickReason, MicTransport, StemFormat, StemReader};

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

/// What the mic rule picks from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicRequest {
    pub rule: MicRule,
    pub moment: MicMoment,
    /// An input to leave out, such as the built-in mic while the lid is closed: macOS then
    /// mutes it to exact zeros.
    pub skipped_uid: Option<String>,
}

impl MicRequest {
    #[must_use]
    pub fn call_start(rule: MicRule) -> Self {
        Self {
            rule,
            moment: MicMoment::CallStart,
            skipped_uid: None,
        }
    }
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

/// Whether `AvoidBluetooth` may record from a device with this transport when the built-in mic
/// is missing.
fn is_wired_alternative(transport: MicTransport) -> bool {
    match transport {
        MicTransport::BuiltIn | MicTransport::Other => true,
        MicTransport::Bluetooth
        | MicTransport::Aggregate
        | MicTransport::Virtual
        | MicTransport::Unknown => false,
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
display_as_str!(MicRule);

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

/// Virtual inputs that report a hardware-like transport, by name. Zoom's device is the one
/// several recorders had to block by name.
const VIRTUAL_INPUT_NAMES: [&str; 2] = ["ZoomAudioDevice", "Microsoft Teams Audio"];

/// One open mic stream. Dropping it stops the stream.
pub struct MicStream {
    stream: Option<cpal::Stream>,
    pick: MicPick,
    /// Set by cpal when the device went away or changed its rate under the stream. Either way
    /// the stream delivers nothing more that the stamps describe correctly.
    is_lost: Arc<AtomicBool>,
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

impl MicStream {
    /// Opens the mic as mono f32 at the device's default rate.
    pub(crate) fn start(request: &MicRequest) -> Result<(Self, StemReader), CaptureError> {
        let (device, pick) = pick_device(request)?;
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
        let is_lost = Arc::new(AtomicBool::new(false));
        let lost_flag = Arc::clone(&is_lost);
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
                move |error| note_stream_error(&lost_flag, &error),
                None,
            )
            .map_err(|e| microphone_error("build_input_stream", &e))?;
        stream.play().map_err(|e| microphone_error("play", &e))?;
        Ok((
            Self {
                stream: Some(stream),
                pick,
                is_lost,
            },
            reader,
        ))
    }

    /// What the mic rule chose when this stream opened.
    #[must_use]
    pub fn pick(&self) -> &MicPick {
        &self.pick
    }

    /// Whether the device went away or changed its rate under the stream.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.is_lost.load(Ordering::Relaxed)
    }

    /// Marks the stream lost, as a vanished device would. The record loop also calls it when a
    /// closed lid mutes the built-in mic to exact zeros.
    pub fn mark_lost(&self) {
        self.is_lost.store(true, Ordering::Relaxed);
    }

    /// Stops the callbacks. The reader still holds what they pushed.
    pub(crate) fn stop(&mut self) {
        drop(self.stream.take());
    }
}

fn note_stream_error(is_lost: &AtomicBool, error: &cpal::Error) {
    #[allow(
        clippy::match_same_arms,
        reason = "cpal's error kind is non-exhaustive, so the wildcard needs its own arm"
    )]
    let is_fatal = match error.kind() {
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => true,
        cpal::ErrorKind::Xrun => false,
        _ => false,
    };
    if is_fatal {
        is_lost.store(true, Ordering::Relaxed);
    }
    tracing::warn!(
        exception.message = %error,
        mic.is_lost = is_fatal,
        "mic.stream_error"
    );
}

/// Decides which input device the mic rule picks, without opening it.
///
/// # Errors
///
/// Fails when no input device exists, the device list cannot be read, or the rule allows no
/// input: see `CaptureError::NoSafeMicrophone` and `CaptureError::OnlyVirtualMicrophone`.
pub fn mic_pick(request: &MicRequest) -> Result<MicPick, CaptureError> {
    pick_device(request).map(|(_, pick)| pick)
}

fn pick_device(request: &MicRequest) -> Result<(Device, MicPick), CaptureError> {
    let rule = request.rule;
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .ok_or(CaptureError::NoMicrophone)?;
    let default_transport = transport(&default);
    let is_default_skipped =
        request.skipped_uid.is_some() && device_uid(&default) == request.skipped_uid;
    let (device, reason) = match (rule, default_transport) {
        _ if is_default_skipped => {
            pick_alternative(&host, &default, request, AlternativeCause::Skipped)?
        }
        (MicRule::FollowDefault, _) => (default.clone(), MicPickReason::RuleFollowsDefault),
        (
            MicRule::AvoidBluetooth,
            MicTransport::BuiltIn | MicTransport::Aggregate | MicTransport::Other,
        ) => (default.clone(), MicPickReason::DefaultIsSafe),
        (MicRule::AvoidBluetooth, MicTransport::Bluetooth) => {
            pick_alternative(&host, &default, request, AlternativeCause::Bluetooth)?
        }
        (MicRule::AvoidBluetooth, MicTransport::Virtual) => {
            pick_alternative(&host, &default, request, AlternativeCause::Virtual)?
        }
        (MicRule::AvoidBluetooth, MicTransport::Unknown) => {
            pick_alternative(&host, &default, request, AlternativeCause::UnknownTransport)?
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

/// Why the default input does not record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlternativeCause {
    Bluetooth,
    Virtual,
    UnknownTransport,
    Skipped,
}

impl AlternativeCause {
    /// The reason a pick names when the built-in mic records instead.
    fn reason(self) -> MicPickReason {
        match self {
            Self::Bluetooth => MicPickReason::DefaultIsBluetooth,
            Self::Virtual => MicPickReason::DefaultIsVirtual,
            Self::UnknownTransport => MicPickReason::DefaultTransportUnknown,
            Self::Skipped => MicPickReason::DefaultIsSkipped,
        }
    }
}

/// The built-in mic, else the first wired input. With neither, only a call start falls back
/// to a Bluetooth or unreadable default; a virtual or skipped default never records.
fn pick_alternative(
    host: &cpal::Host,
    default: &Device,
    request: &MicRequest,
    cause: AlternativeCause,
) -> Result<(Device, MicPickReason), CaptureError> {
    let inputs: Vec<(Device, MicTransport)> = host
        .input_devices()
        .map_err(|e| microphone_error("input_devices", &e))?
        .filter(|device| request.skipped_uid.is_none() || device_uid(device) != request.skipped_uid)
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
        .find(|(_, transport)| is_wired_alternative(*transport));
    match (built_in, wired) {
        (Some((built_in, _)), _) => return Ok((built_in.clone(), cause.reason())),
        (None, Some((wired, _))) => return Ok((wired.clone(), MicPickReason::NoBuiltInMic)),
        (None, None) => {}
    }
    let default_name = device_name(default).unwrap_or_default();
    match (cause, request.moment) {
        (AlternativeCause::Virtual, MicMoment::CallStart | MicMoment::MidCall) => {
            Err(CaptureError::OnlyVirtualMicrophone { default_name })
        }
        (
            AlternativeCause::Bluetooth | AlternativeCause::UnknownTransport,
            MicMoment::CallStart,
        ) => {
            tracing::warn!(
                device.name = default_name,
                mic.reason = %cause.reason(),
                "mic.unsafe_fallback"
            );
            Ok((default.clone(), MicPickReason::NoAlternativeInput))
        }
        (AlternativeCause::Bluetooth | AlternativeCause::UnknownTransport, MicMoment::MidCall)
        | (AlternativeCause::Skipped, MicMoment::CallStart | MicMoment::MidCall) => {
            Err(CaptureError::NoSafeMicrophone {
                default_name,
                moment: request.moment,
            })
        }
    }
}

fn device_uid(device: &Device) -> Option<String> {
    device.id().ok().map(|id| id.id().to_owned())
}

/// On macOS cpal reports no Bluetooth interface type, so the transport comes from the HAL. cpal
/// 0.18.2's device ID there is the Core Audio device UID, which the HAL resolves to the same
/// device.
fn transport(device: &Device) -> MicTransport {
    let is_virtual_by_name = device_name(device)
        .is_some_and(|name| VIRTUAL_INPUT_NAMES.iter().any(|known| name.contains(known)));
    if is_virtual_by_name {
        return MicTransport::Virtual;
    }
    platform_transport(device)
}

#[cfg(target_os = "macos")]
fn platform_transport(device: &Device) -> MicTransport {
    use cidre::core_audio::DeviceTransportType;

    let Some(uid) = device_uid(device) else {
        return MicTransport::Unknown;
    };
    match crate::mac::hal::transport_type(&uid) {
        Some(transport) if crate::mac::hal::is_bluetooth_transport(transport) => {
            MicTransport::Bluetooth
        }
        Some(DeviceTransportType::BUILT_IN) => MicTransport::BuiltIn,
        // An aggregate that holds a Bluetooth mic opens it like the headset itself would. One
        // whose members cannot be read counts as unknown.
        Some(DeviceTransportType::AGGREGATE) => {
            match crate::mac::hal::aggregate_has_bluetooth(&uid) {
                Some(false) => MicTransport::Aggregate,
                Some(true) => MicTransport::Bluetooth,
                None => MicTransport::Unknown,
            }
        }
        Some(DeviceTransportType::VIRTUAL) => MicTransport::Virtual,
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
fn platform_transport(device: &Device) -> MicTransport {
    use cpal::InterfaceType;

    match device.description().ok().map(|d| d.interface_type()) {
        Some(InterfaceType::BuiltIn) => MicTransport::BuiltIn,
        Some(InterfaceType::Bluetooth) => MicTransport::Bluetooth,
        Some(InterfaceType::Aggregate) => MicTransport::Aggregate,
        Some(InterfaceType::Virtual) => MicTransport::Virtual,
        Some(
            InterfaceType::Usb
            | InterfaceType::Pci
            | InterfaceType::FireWire
            | InterfaceType::Thunderbolt
            | InterfaceType::Hdmi
            | InterfaceType::Line
            | InterfaceType::Spdif
            | InterfaceType::Network
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
