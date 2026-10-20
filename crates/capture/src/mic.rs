use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, InterfaceType};

use crate::ring::stem_ring;
use crate::{CaptureError, Stem, StemFormat, StemReader};

/// Which input device records the mic stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicRule {
    /// The system default input, whatever it is.
    FollowDefault,
    /// The system default input, unless it is Bluetooth: then the built-in mic. Opening a
    /// Bluetooth headset's mic switches it to the hands-free profile, which drops its playback to
    /// narrowband audio for everyone on the call.
    AvoidBluetooth,
}

/// Keeps the input stream open. Dropping it stops the stream.
pub struct MicSource {
    _stream: cpal::Stream,
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
    ///
    /// # Errors
    ///
    /// Fails when no input device exists or the stream does not open.
    pub fn start(rule: MicRule) -> Result<(Self, StemReader), CaptureError> {
        let device = pick_device(rule)?;
        let config = device
            .default_input_config()
            .map_err(|e| CaptureError::Microphone(e.to_string()))?;
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
        let (mut pusher, reader) = stem_ring(Stem::Mic, format);
        let stream = device
            .build_input_stream::<f32, _, _>(
                stream_config,
                move |samples, _| pusher.push_interleaved(samples),
                |error| tracing::warn!(exception.message = %error, "mic.stream_error"),
                None,
            )
            .map_err(|e| CaptureError::Microphone(e.to_string()))?;
        stream
            .play()
            .map_err(|e| CaptureError::Microphone(e.to_string()))?;
        Ok((Self { _stream: stream }, reader))
    }
}

fn pick_device(rule: MicRule) -> Result<Device, CaptureError> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .ok_or(CaptureError::NoMicrophone)?;
    match rule {
        MicRule::FollowDefault => Ok(default),
        MicRule::AvoidBluetooth => {
            if interface_type(&default) != Some(InterfaceType::Bluetooth) {
                return Ok(default);
            }
            let built_in = host
                .input_devices()
                .map_err(|e| CaptureError::Microphone(e.to_string()))?
                .find(|d| interface_type(d) == Some(InterfaceType::BuiltIn));
            // Without a built-in mic (a closed-lid Mac mini), the Bluetooth mic is the only one.
            Ok(built_in.unwrap_or(default))
        }
    }
}

fn interface_type(device: &Device) -> Option<InterfaceType> {
    device.description().ok().map(|d| d.interface_type())
}

fn device_name(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}
