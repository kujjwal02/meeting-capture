//! The remote stem: a Core Audio process tap on everything the system plays. The only module with
//! unsafe code: the IO proc reads Core Audio's buffers through raw pointers, and this module makes
//! the start, stop and destroy calls itself. cidre's wrappers for those assert success when they
//! drop, which panics in debug builds once the aggregate has died under us.
#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cidre::core_audio::aggregate_device_keys as agg_keys;
use cidre::{cat, cf, core_audio as ca, ns, os};

use crate::mac::hal::hal_devices;
use crate::mac::os_error;
use crate::ring::{MonoPusher, stem_ring};
use crate::{CaptureError, StemFormat, StemReader};

pub(crate) const AGGREGATE_DEVICE_NAME: &str = "Meeting Capture tap";

/// UIDs are global: the HAL refuses a second aggregate with a UID in use, even when the holder is
/// a private aggregate in another process that this one cannot list. A fixed UID lets stale
/// cleanup find a leftover we can see by its UID. The fallback UID covers one we cannot see.
const AGGREGATE_UIDS: [&str; 2] = [
    "com.ujjwalkumar.meetingcapture.tap",
    "com.ujjwalkumar.meetingcapture.tap.fallback",
];

/// Aggregates of ours this process holds. Stale cleanup waits until none are live, so it never
/// destroys a running source's device.
static LIVE_AGGREGATES: AtomicUsize = AtomicUsize::new(0);

/// Keeps the tap running. Teardown runs in a fixed order: stop the device, destroy the IO proc,
/// let the caller drain, remove the format listener, destroy the aggregate, then the tap.
/// `stop_callbacks` does the first two; dropping does the rest. A failed start drops a partly
/// built source, which runs the same order.
pub struct RemoteSource {
    /// Taken out of cidre's `TapGuard`, whose drop asserts that destruction succeeds.
    tap: Option<ca::Obj>,
    aggregate: Option<ca::Device>,
    aggregate_uid: &'static str,
    proc_id: Option<ca::DeviceIoProcId>,
    is_running: bool,
    /// From `Box::leak`. Freed only after the IO proc is destroyed; leaked if that fails.
    callback: Option<NonNull<TapCallback>>,
    /// The tap's current rate. The format listener writes it and the IO proc reads it.
    sample_rate_hz: Arc<AtomicU32>,
    is_listening: bool,
}

// SAFETY: the callback pointer is owned by this source alone. The IO thread uses it only between
// start and IO proc destruction, both of which this source performs, from whatever thread holds
// it.
unsafe impl Send for RemoteSource {}

struct TapCallback {
    pusher: MonoPusher,
    sample_rate_hz: Arc<AtomicU32>,
}

impl RemoteSource {
    /// Taps all system output, mixed to mono, in a private aggregate device whose only member is
    /// the tap. The output device stays out of the aggregate: adding it doubles the audio and
    /// blocks Zoom and Slack from joining calls.
    pub(crate) fn start() -> Result<(Self, StemReader), CaptureError> {
        destroy_stale_aggregates();

        // An empty exclude list: excluding this process broke Zoom on macOS 26.
        let tap_desc = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
        let tap = tap_desc
            .create_process_tap()
            .map_err(|error| os_error("AudioHardwareCreateProcessTap", error.0.get()))?;
        let tap_details = read_tap(&tap);
        let tap = into_tap_id(tap);
        let (asbd, tap_uid) = match tap_details {
            Ok(details) => details,
            Err(error) => {
                destroy_tap(tap);
                return Err(error);
            }
        };

        let format = StemFormat {
            sample_rate_hz: whole_hz(asbd.sample_rate),
            device_channels: u16::try_from(asbd.channels_per_frame).unwrap_or(1),
            device_name: AGGREGATE_DEVICE_NAME.to_owned(),
        };
        let sample_rate_hz = Arc::new(AtomicU32::new(format.sample_rate_hz));
        let (pusher, reader) = stem_ring(format);
        let mut callback = NonNull::from(Box::leak(Box::new(TapCallback {
            pusher,
            sample_rate_hz: Arc::clone(&sample_rate_hz),
        })));
        let mut source = Self {
            tap: Some(tap),
            aggregate: None,
            aggregate_uid: AGGREGATE_UIDS[0],
            proc_id: None,
            is_running: false,
            callback: Some(callback),
            sample_rate_hz,
            is_listening: false,
        };

        source.listen_for_format_changes(tap);
        let (aggregate, aggregate_uid) = create_aggregate(&tap_uid)?;
        LIVE_AGGREGATES.fetch_add(1, Ordering::AcqRel);
        source.aggregate = Some(aggregate);
        source.aggregate_uid = aggregate_uid;

        // SAFETY: the pointer came from a live Box, and no other reference to it exists until
        // the IO proc runs, which starts below.
        let callback = Some(unsafe { callback.as_mut() });
        let proc_id = aggregate
            .create_io_proc_id(io_proc, callback)
            .map_err(|error| os_error("AudioDeviceCreateIOProcID", error.0.get()))?;
        source.proc_id = Some(proc_id);
        // SAFETY: the proc belongs to this aggregate, and `source` keeps the callback it reads
        // alive until the proc is destroyed.
        let status = unsafe { AudioDeviceStart(aggregate, Some(proc_id)) };
        if !status.is_ok() {
            return Err(os_error("AudioDeviceStart", status.0));
        }
        source.is_running = true;
        Ok((source, reader))
    }

    /// The tap's current rate. It follows the output device, so it can change mid-call.
    #[must_use]
    pub fn current_sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz.load(Ordering::Relaxed)
    }

    /// The UID the aggregate device was created with: the fixed one, or its fallback.
    #[must_use]
    pub fn aggregate_uid(&self) -> &'static str {
        self.aggregate_uid
    }

    /// Stops the device and destroys the IO proc. No callback runs after this returns, so the
    /// reader then holds every sample the tap delivered. The aggregate and the tap stay until
    /// drop.
    pub fn stop_callbacks(&mut self) {
        match (self.aggregate, self.proc_id.take()) {
            (Some(aggregate), Some(proc_id)) => {
                if std::mem::take(&mut self.is_running) {
                    // SAFETY: the proc belongs to this aggregate. Called off the IO thread,
                    // AudioDeviceStop returns only after the running IO cycle ends.
                    let status = unsafe { AudioDeviceStop(aggregate, Some(proc_id)) };
                    if !status.is_ok() {
                        tracing::warn!(coreaudio.os_status = status.0, "tap.stop_failed");
                    }
                }
                // SAFETY: the proc belongs to this aggregate. Once this call succeeds Core Audio
                // never calls the proc again, so freeing the callback afterwards is safe.
                let status = unsafe { AudioDeviceDestroyIOProcID(aggregate, proc_id) };
                if status.is_ok() {
                    self.free_callback();
                } else {
                    // A small leak beats a callback that writes into freed memory.
                    tracing::warn!(coreaudio.os_status = status.0, "tap.destroy_proc_failed");
                    self.callback = None;
                }
            }
            // No proc was registered, so nothing else holds the callback.
            (Some(_) | None, None) => self.free_callback(),
            (None, Some(_)) => {
                tracing::warn!("tap.proc_without_aggregate");
                self.callback = None;
            }
        }
    }

    fn free_callback(&mut self) {
        if let Some(callback) = self.callback.take() {
            // SAFETY: the pointer came from `Box::leak` in `start`, and no IO proc can reach it.
            drop(unsafe { Box::from_raw(callback.as_ptr()) });
        }
    }

    fn listen_for_format_changes(&mut self, tap: ca::Obj) {
        // The listener holds its own reference and never gives it back: removing a listener
        // does not wait for a call already running, so the cell must outlive this source.
        let rate_cell = Arc::into_raw(Arc::clone(&self.sample_rate_hz)).cast_mut();
        match tap.add_prop_listener(&format_address(), tap_format_changed, rate_cell) {
            Ok(()) => self.is_listening = true,
            Err(error) => {
                tracing::warn!(
                    coreaudio.os_status = error.0.get(),
                    "tap.format_listener_failed"
                );
                // SAFETY: the pointer came from `Arc::into_raw` above, and the listener that
                // would have used it was never registered.
                drop(unsafe { Arc::from_raw(rate_cell) });
                return;
            }
        }
        // The format may have changed between the first read and the registration.
        match tap.prop::<cat::AudioStreamBasicDesc>(&format_address()) {
            Ok(asbd) => self
                .sample_rate_hz
                .store(whole_hz(asbd.sample_rate), Ordering::Relaxed),
            Err(error) => tracing::warn!(
                coreaudio.os_status = error.0.get(),
                "tap.format_read_failed"
            ),
        }
    }

    fn stop_listening(&mut self) {
        let Some(tap) = self.tap else {
            return;
        };
        if !std::mem::take(&mut self.is_listening) {
            return;
        }
        let rate_cell = Arc::as_ptr(&self.sample_rate_hz).cast_mut();
        if let Err(error) =
            tap.remove_prop_listener(&format_address(), tap_format_changed, rate_cell)
        {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                "tap.format_listener_remove_failed"
            );
        }
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.stop_callbacks();
        self.stop_listening();
        // The aggregate goes before the tap it contains.
        if let Some(aggregate) = self.aggregate.take() {
            // SAFETY: this source created the aggregate, and its proc is gone.
            let status = unsafe { AudioHardwareDestroyAggregateDevice(aggregate) };
            if !status.is_ok() {
                tracing::warn!(
                    coreaudio.os_status = status.0,
                    "tap.destroy_aggregate_failed"
                );
            }
            LIVE_AGGREGATES.fetch_sub(1, Ordering::AcqRel);
        }
        if let Some(tap) = self.tap.take() {
            destroy_tap(tap);
        }
    }
}

fn read_tap(
    tap: &ca::TapGuard,
) -> Result<(cat::AudioStreamBasicDesc, cidre::arc::R<cf::String>), CaptureError> {
    let asbd = tap
        .asbd()
        .map_err(|error| os_error("kAudioTapPropertyFormat", error.0.get()))?;
    check_format(&asbd)?;
    let tap_uid = tap
        .uid()
        .map_err(|error| os_error("kAudioTapPropertyUID", error.0.get()))?;
    Ok((asbd, tap_uid))
}

/// Takes the tap's ID out of cidre's guard, so a failed destroy is logged rather than asserted.
fn into_tap_id(tap: ca::TapGuard) -> ca::Obj {
    let tap_id: ca::Obj = **tap;
    // The guard holds only the ID, so forgetting it leaks nothing.
    std::mem::forget(tap);
    tap_id
}

fn destroy_tap(tap: ca::Obj) {
    // SAFETY: the call takes a plain object ID, and nothing of ours reads the tap after this.
    let status = unsafe { AudioHardwareDestroyProcessTap(tap) };
    if !status.is_ok() {
        tracing::warn!(coreaudio.os_status = status.0, "tap.destroy_tap_failed");
    }
}

fn format_address() -> ca::PropAddr {
    ca::PropSelector::TAP_FORMAT.global_addr()
}

extern "C-unwind" fn tap_format_changed(
    tap: ca::Obj,
    _address_count: u32,
    _addresses: *const ca::PropAddr,
    rate_cell: *mut AtomicU32,
) -> os::Status {
    // SAFETY: the listener owns a reference to the rate cell that is never released, so the cell
    // outlives every call.
    let Some(rate_hz) = (unsafe { rate_cell.as_ref() }) else {
        return os::Status::NO_ERR;
    };
    match tap.prop::<cat::AudioStreamBasicDesc>(&format_address()) {
        Ok(asbd) => {
            let sample_rate_hz = whole_hz(asbd.sample_rate);
            rate_hz.store(sample_rate_hz, Ordering::Relaxed);
            tracing::info!(audio.sample_rate_hz = sample_rate_hz, "tap.format_changed");
        }
        Err(error) => tracing::warn!(
            coreaudio.os_status = error.0.get(),
            "tap.format_read_failed"
        ),
    }
    os::Status::NO_ERR
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Core Audio rates are whole numbers well inside u32"
)]
fn whole_hz(sample_rate: f64) -> u32 {
    sample_rate as u32
}

/// Destroys every aggregate this process can see under our UIDs or our name, unless one of ours
/// is live. It finds public leftovers from any process and private ones from this process whose
/// destroy failed. The HAL frees a private aggregate when its process exits, kill -9 included.
fn destroy_stale_aggregates() {
    let live = LIVE_AGGREGATES.load(Ordering::Acquire);
    if live > 0 {
        tracing::info!(tap.live_aggregates = live, "tap.stale_cleanup_skipped");
        return;
    }
    let mut stale = Vec::new();
    for uid in AGGREGATE_UIDS {
        match ca::Device::with_uid(&cf::String::from_str(uid)) {
            Ok(device) if !device.is_unknown() => stale.push(device.0.0),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(coreaudio.os_status = error.0.get(), "tap.uid_lookup_failed");
            }
        }
    }
    match hal_devices() {
        Ok(devices) => stale.extend(
            devices
                .into_iter()
                .filter(|device| {
                    device
                        .uid
                        .as_deref()
                        .is_some_and(|uid| AGGREGATE_UIDS.contains(&uid))
                        || device.name.as_deref() == Some(AGGREGATE_DEVICE_NAME)
                })
                .map(|device| device.id),
        ),
        Err(error) => tracing::warn!(exception.message = %error, "tap.device_list_failed"),
    }
    stale.sort_unstable();
    stale.dedup();

    let mut destroyed = 0_usize;
    for &device_id in &stale {
        // SAFETY: the call takes a plain object ID and frees no memory of ours. No source of ours
        // is live, so the aggregate is not in use by this process.
        let status = unsafe { AudioHardwareDestroyAggregateDevice(ca::Device(ca::Obj(device_id))) };
        if status.is_ok() {
            destroyed += 1;
        } else {
            tracing::warn!(
                coreaudio.os_status = status.0,
                device.id = device_id,
                "tap.stale_aggregate_destroy_failed"
            );
        }
    }
    tracing::info!(
        tap.stale_found = stale.len(),
        tap.stale_destroyed = destroyed,
        "tap.stale_aggregates"
    );
}

/// Creates the aggregate under the fixed UID, or under the fallback UID if the first create
/// fails.
fn create_aggregate(tap_uid: &cf::String) -> Result<(ca::Device, &'static str), CaptureError> {
    let [fixed_uid, fallback_uid] = AGGREGATE_UIDS;
    match ca::AggregateDevice::with_desc(&aggregate_desc(tap_uid, fixed_uid)) {
        Ok(device) => Ok((into_device_id(device), fixed_uid)),
        Err(error) => {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                device.uid = fixed_uid,
                "tap.aggregate_create_failed"
            );
            let device = ca::AggregateDevice::with_desc(&aggregate_desc(tap_uid, fallback_uid))
                .map_err(|error| os_error("AudioHardwareCreateAggregateDevice", error.0.get()))?;
            Ok((into_device_id(device), fallback_uid))
        }
    }
}

/// Takes the aggregate's ID out of cidre's owner, so `RemoteSource` decides when to destroy it.
fn into_device_id(aggregate: ca::AggregateDevice) -> ca::Device {
    let device = *aggregate;
    // The owner holds only the ID, so forgetting it leaks nothing.
    std::mem::forget(aggregate);
    device
}

fn aggregate_desc(
    tap_uid: &cf::String,
    device_uid: &str,
) -> cidre::arc::R<cf::DictionaryOf<cf::String, cf::Type>> {
    let sub_tap =
        cf::DictionaryOf::with_keys_values(&[ca::sub_device_keys::uid()], &[tap_uid.as_type_ref()]);
    let device_uid = cf::String::from_str(device_uid);
    let device_name = cf::String::from_str(AGGREGATE_DEVICE_NAME);
    let tap_list = cf::ArrayOf::from_slice(&[sub_tap.as_ref()]);
    cf::DictionaryOf::with_keys_values(
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
            device_uid.as_ref(),
            &tap_list,
        ],
    )
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

extern "C" fn io_proc(
    _device: ca::Device,
    _now: &cat::AudioTimeStamp,
    input_data: &cat::AudioBufList<1>,
    input_time: &cat::AudioTimeStamp,
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
    let first_sample = buffer.data.cast::<f32>();
    if first_sample.is_null() || !first_sample.is_aligned() {
        return os::Status::NO_ERR;
    }
    let sample_count = buffer.data_bytes_size as usize / size_of::<f32>();
    // SAFETY: Core Audio owns the buffer for the length of this call. The pointer is non-null and
    // aligned, and the length comes from the buffer's own byte count. `check_format` confirmed
    // 32-bit float samples before the device started.
    let samples = unsafe { std::slice::from_raw_parts(first_sample, sample_count) };
    let has_host_time = input_time.flags.0 & cat::AudioTimeStampFlags::HOST_TIME_VALID.0 != 0;
    let host_time_ns = has_host_time.then(|| ca::host_time_to_nanos(input_time.host_time));
    let sample_rate_hz = callback.sample_rate_hz.load(Ordering::Relaxed);
    callback
        .pusher
        .push_buffer(samples, host_time_ns, sample_rate_hz);
    os::Status::NO_ERR
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C-unwind" {
    fn AudioDeviceStart(device: ca::Device, proc_id: Option<ca::DeviceIoProcId>) -> os::Status;
    fn AudioDeviceStop(device: ca::Device, proc_id: Option<ca::DeviceIoProcId>) -> os::Status;
    fn AudioDeviceDestroyIOProcID(device: ca::Device, proc_id: ca::DeviceIoProcId) -> os::Status;
    fn AudioHardwareDestroyAggregateDevice(device: ca::Device) -> os::Status;
    fn AudioHardwareDestroyProcessTap(tap: ca::Obj) -> os::Status;
}
