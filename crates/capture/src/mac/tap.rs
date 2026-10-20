//! The remote stem: a Core Audio process tap on everything the system plays. The only module with
//! unsafe code: the IO proc reads Core Audio's buffers through raw pointers.
#![allow(unsafe_code)]

use cidre::core_audio::aggregate_device_keys as agg_keys;
use cidre::core_audio::hardware::StartedDevice;
use cidre::{cat, cf, core_audio as ca, ns, os};

use crate::ring::{MonoPusher, stem_ring};
use crate::{CaptureError, Stem, StemFormat, StemReader};

const AGGREGATE_DEVICE_NAME: &str = "Meeting Capture tap";

/// Keeps the tap running. Dropping it stops the device and destroys the IO proc before it frees
/// the callback's state, so no callback can run against freed memory.
pub struct RemoteSource {
    started: Option<StartedDevice<ca::AggregateDevice>>,
    proc_id: ca::DeviceIoProcId,
    callback: Box<TapCallback>,
    tap: Option<ca::TapGuard>,
}

struct TapCallback {
    pusher: MonoPusher,
}

impl RemoteSource {
    /// Taps all system output, mixed to mono, in a private aggregate device whose only member is
    /// the tap. The output device stays out of the aggregate: adding it doubles the audio and
    /// blocks Zoom and Slack from joining calls.
    ///
    /// # Errors
    ///
    /// Fails when Core Audio refuses the tap or the aggregate device, or the tap is not 32-bit
    /// float.
    pub fn start() -> Result<(Self, StemReader), CaptureError> {
        // An empty exclude list: excluding this process broke Zoom on macOS 26.
        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        let tap = tap_desc.create_process_tap().map_err(tap_error)?;
        let asbd = tap.asbd().map_err(tap_error)?;
        check_format(&asbd)?;
        let tap_uid = tap.uid().map_err(tap_error)?;

        let sub_tap = cf::DictionaryOf::with_keys_values(
            &[ca::sub_device_keys::uid()],
            &[tap_uid.as_type_ref()],
        );
        let device_uid = cf::Uuid::new().to_cf_string();
        let device_name = cf::String::from_str(AGGREGATE_DEVICE_NAME);
        let tap_list = cf::ArrayOf::from_slice(&[sub_tap.as_ref()]);
        let agg_desc = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::uid(),
                agg_keys::tap_list(),
            ],
            &[
                cf::Boolean::value_true().as_type_ref(),
                cf::Boolean::value_false(),
                device_name.as_ref(),
                &device_uid,
                &tap_list,
            ],
        );

        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Core Audio rates are whole numbers well inside u32"
        )]
        let format = StemFormat {
            sample_rate_hz: asbd.sample_rate as u32,
            device_channels: u16::try_from(asbd.channels_per_frame).unwrap_or(1),
            device_name: AGGREGATE_DEVICE_NAME.to_owned(),
        };
        let (pusher, reader) = stem_ring(Stem::Remote, format);
        let mut callback = Box::new(TapCallback { pusher });

        let device = ca::AggregateDevice::with_desc(&agg_desc).map_err(tap_error)?;
        let proc_id = device
            .create_io_proc_id(io_proc, Some(&mut *callback))
            .map_err(tap_error)?;
        let started = match ca::device_start(device, Some(proc_id)) {
            Ok(started) => started,
            Err(error) => {
                // `device_start` consumed the device, which destroyed the aggregate and its proc.
                return Err(tap_error(error));
            }
        };

        let source = Self {
            started: Some(started),
            proc_id,
            callback,
            tap: Some(tap),
        };
        Ok((source, reader))
    }

    /// The tap's current rate. It follows the output device, so it can change mid-call.
    #[must_use]
    pub fn current_sample_rate_hz(&self) -> Option<f64> {
        self.tap.as_ref()?.asbd().ok().map(|asbd| asbd.sample_rate)
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        if let Some(started) = self.started.take() {
            match started.stop() {
                Ok(device) => {
                    // SAFETY: the device is stopped and the proc belongs to it. After this call
                    // Core Audio never calls the proc again, so freeing `callback` below is safe.
                    let status =
                        unsafe { AudioDeviceDestroyIOProcID(ca::Device(device.0), self.proc_id) };
                    if !status.is_ok() {
                        tracing::warn!(os.status = status.0, "tap.destroy_proc_failed");
                    }
                    drop(device);
                }
                Err(error) => {
                    tracing::warn!(os.status = error.0.get(), "tap.stop_failed");
                }
            }
        }
        drop(self.tap.take());
        let _ = &self.callback;
    }
}

fn check_format(asbd: &cat::AudioStreamBasicDesc) -> Result<(), CaptureError> {
    let is_float = asbd.format_flags.0 & cat::AudioFormatFlags::IS_FLOAT.0 != 0;
    if asbd.format == cat::AudioFormat::LINEAR_PCM && is_float && asbd.bits_per_channel == 32 {
        return Ok(());
    }
    Err(CaptureError::UnsupportedTapFormat {
        bits_per_channel: asbd.bits_per_channel,
        sample_kind: if is_float { "float" } else { "integer" },
    })
}

fn tap_error(error: os::Error) -> CaptureError {
    CaptureError::Tap(format!("OSStatus {}", error.0))
}

extern "C" fn io_proc(
    _device: ca::Device,
    _now: &cat::AudioTimeStamp,
    input_data: &cat::AudioBufList<1>,
    _input_time: &cat::AudioTimeStamp,
    _output_data: &mut cat::AudioBufList<1>,
    _output_time: &cat::AudioTimeStamp,
    callback: Option<&mut TapCallback>,
) -> os::Status {
    let Some(callback) = callback else {
        return os::Status::NO_ERR;
    };
    if input_data.number_buffers == 0 {
        return os::Status::NO_ERR;
    }
    let buffer = &input_data.buffers[0];
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "the alignment is checked on the next line"
    )]
    let data = buffer.data.cast::<f32>();
    if data.is_null() || !data.is_aligned() {
        return os::Status::NO_ERR;
    }
    let sample_count = buffer.data_bytes_size as usize / size_of::<f32>();
    // SAFETY: Core Audio owns the buffer for the length of this call. The pointer is non-null and
    // aligned, and the length comes from the buffer's own byte count. `check_format` confirmed
    // 32-bit float samples before the device started.
    let samples = unsafe { std::slice::from_raw_parts(data, sample_count) };
    callback.pusher.push_interleaved(samples);
    os::Status::NO_ERR
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C-unwind" {
    // cidre binds the create call but not this one.
    fn AudioDeviceDestroyIOProcID(device: ca::Device, proc_id: ca::DeviceIoProcId) -> os::Status;
}
