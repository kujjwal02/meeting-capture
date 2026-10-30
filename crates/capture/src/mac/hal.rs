use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use cidre::{cf, core_audio as ca, os};

use crate::CaptureError;
use crate::mac::os_error;

/// One Core Audio device as the HAL lists it to this process.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HalDevice {
    pub id: u32,
    pub uid: Option<String>,
    pub name: Option<String>,
}

/// Every device the HAL lists to this process. Private aggregates of other processes are not in
/// the list.
///
/// # Errors
///
/// Fails when the HAL refuses the device list.
pub fn hal_devices() -> Result<Vec<HalDevice>, CaptureError> {
    let devices = ca::System::devices()
        .map_err(|error| os_error("kAudioHardwarePropertyDevices", error.0.get()))?;
    Ok(devices
        .into_iter()
        .map(|device| HalDevice {
            id: device.0.0,
            uid: device
                .uid()
                .inspect_err(|error| {
                    tracing::warn!(
                        coreaudio.os_status = error.0.get(),
                        device.id = device.0.0,
                        "hal.uid_read_failed"
                    );
                })
                .ok()
                .map(|uid| uid.to_string()),
            name: device
                .name()
                .inspect_err(|error| {
                    tracing::warn!(
                        coreaudio.os_status = error.0.get(),
                        device.id = device.0.0,
                        "hal.name_read_failed"
                    );
                })
                .ok()
                .map(|name| name.to_string()),
        })
        .collect())
}

/// Whether a transport type is classic Bluetooth or Bluetooth LE.
pub(crate) fn is_bluetooth_transport(transport: ca::DeviceTransportType) -> bool {
    transport == ca::DeviceTransportType::BLUETOOTH
        || transport == ca::DeviceTransportType::BLUETOOTH_LE
}

fn is_bluetooth(device: ca::Device) -> bool {
    device.transport_type().is_ok_and(is_bluetooth_transport)
}

/// Whether the aggregate with this UID holds a Bluetooth device among its active sub-devices.
/// `None` when the HAL cannot say.
pub(crate) fn aggregate_has_bluetooth(uid: &str) -> Option<bool> {
    let device = device_with_uid(uid)?;
    let sub_devices: Vec<ca::Device> = device
        .prop_vec(&ca::PropSelector::AGGREGATE_DEVICE_ACTIVE_SUB_DEVICE_LIST.global_addr())
        .inspect_err(|error| {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                device.uid = uid,
                "hal.sub_devices_read_failed"
            );
        })
        .ok()?;
    Some(sub_devices.into_iter().any(is_bluetooth))
}

fn device_with_uid(uid: &str) -> Option<ca::Device> {
    match ca::Device::with_uid(&cf::String::from_str(uid)) {
        Ok(device) if !device.is_unknown() => Some(device),
        Ok(_) => {
            tracing::warn!(device.uid = uid, "hal.uid_unknown");
            None
        }
        Err(error) => {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                device.uid = uid,
                "hal.uid_lookup_failed"
            );
            None
        }
    }
}

/// The transport of the device with this UID, or `None` when the HAL cannot say.
pub(crate) fn transport_type(uid: &str) -> Option<ca::DeviceTransportType> {
    device_with_uid(uid)?
        .transport_type()
        .inspect_err(|error| {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                device.uid = uid,
                "hal.transport_read_failed"
            );
        })
        .ok()
}

const DEFAULT_INPUT_CHANGED: u32 = 1;
const DEFAULT_OUTPUT_CHANGED: u32 = 1 << 1;
const DEVICE_LIST_CHANGED: u32 = 1 << 2;

/// Changes the system listeners saw since the last `take_hal_changes`. The listeners only set
/// bits, so the HAL's notification thread never waits on this process.
static HAL_CHANGES: AtomicU32 = AtomicU32::new(0);
static IS_WATCHING: Mutex<bool> = Mutex::new(false);

/// System-wide device changes since the last look.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HalChanges {
    pub default_input: bool,
    pub default_output: bool,
    /// A device appeared or went away, Bluetooth headsets and aggregates included.
    pub device_list: bool,
}

impl HalChanges {
    #[must_use]
    pub fn any(self) -> bool {
        self.default_input || self.default_output || self.device_list
    }

    #[cfg(debug_assertions)]
    fn bits(self) -> u32 {
        let mut bits = 0;
        if self.default_input {
            bits |= DEFAULT_INPUT_CHANGED;
        }
        if self.default_output {
            bits |= DEFAULT_OUTPUT_CHANGED;
        }
        if self.device_list {
            bits |= DEVICE_LIST_CHANGED;
        }
        bits
    }
}

/// Registers the system listeners for the default input, the default output and the device
/// list, once per process. They stay for the process's life, so no removal can race a running
/// call.
///
/// # Errors
///
/// Fails when the HAL refuses a listener. The ones registered before it stay.
pub fn watch_hal_changes() -> Result<(), CaptureError> {
    let mut is_watching = IS_WATCHING.lock().unwrap_or_else(PoisonError::into_inner);
    if *is_watching {
        return Ok(());
    }
    let system = ca::System::OBJ;
    let listeners: [(ca::PropSelector, ca::PropListenerFn, &'static str); 3] = [
        (
            ca::PropSelector::HW_DEFAULT_INPUT_DEVICE,
            default_input_changed,
            "kAudioHardwarePropertyDefaultInputDevice",
        ),
        (
            ca::PropSelector::HW_DEFAULT_OUTPUT_DEVICE,
            default_output_changed,
            "kAudioHardwarePropertyDefaultOutputDevice",
        ),
        (
            ca::PropSelector::HW_DEVICES,
            device_list_changed,
            "kAudioHardwarePropertyDevices",
        ),
    ];
    for (selector, listener, name) in listeners {
        system
            .add_prop_listener(&selector.global_addr(), listener, std::ptr::null_mut())
            .map_err(|error| os_error(name, error.0.get()))?;
    }
    *is_watching = true;
    tracing::info!("hal.watching");
    Ok(())
}

/// Takes the changes seen since the last call. Reads only this process's memory, never the HAL.
#[must_use]
pub fn take_hal_changes() -> HalChanges {
    let bits = HAL_CHANGES.swap(0, Ordering::AcqRel);
    HalChanges {
        default_input: bits & DEFAULT_INPUT_CHANGED != 0,
        default_output: bits & DEFAULT_OUTPUT_CHANGED != 0,
        device_list: bits & DEVICE_LIST_CHANGED != 0,
    }
}

/// Adds changes as if the listeners had seen them. For debug builds' injected events.
#[cfg(debug_assertions)]
pub fn inject_hal_changes(changes: HalChanges) {
    HAL_CHANGES.fetch_or(changes.bits(), Ordering::AcqRel);
}

fn note_change(bit: u32, name: &'static str) -> os::Status {
    HAL_CHANGES.fetch_or(bit, Ordering::AcqRel);
    tracing::info!(hal.property = name, "hal.changed");
    os::Status::NO_ERR
}

extern "C-unwind" fn default_input_changed(
    _system: ca::Obj,
    _address_count: u32,
    _addresses: *const ca::PropAddr,
    _client_data: *mut c_void,
) -> os::Status {
    note_change(DEFAULT_INPUT_CHANGED, "default_input")
}

extern "C-unwind" fn default_output_changed(
    _system: ca::Obj,
    _address_count: u32,
    _addresses: *const ca::PropAddr,
    _client_data: *mut c_void,
) -> os::Status {
    note_change(DEFAULT_OUTPUT_CHANGED, "default_output")
}

extern "C-unwind" fn device_list_changed(
    _system: ca::Obj,
    _address_count: u32,
    _addresses: *const ca::PropAddr,
    _client_data: *mut c_void,
) -> os::Status {
    note_change(DEVICE_LIST_CHANGED, "device_list")
}

/// Where Bluetooth sits in the audio route. Two readings that differ mean a Bluetooth route
/// change, after which Core Audio needs 1.5 s to settle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BluetoothRoute {
    pub default_input_is_bluetooth: bool,
    pub default_output_is_bluetooth: bool,
    pub bluetooth_devices: usize,
}

/// Reads the route. Call it on a change, never on a timer.
#[must_use]
pub fn bluetooth_route() -> BluetoothRoute {
    BluetoothRoute {
        default_input_is_bluetooth: ca::System::default_input_device().is_ok_and(is_bluetooth),
        default_output_is_bluetooth: ca::System::default_output_device().is_ok_and(is_bluetooth),
        bluetooth_devices: ca::System::devices().map_or(0, |devices| {
            devices
                .iter()
                .filter(|device| is_bluetooth(**device))
                .count()
        }),
    }
}
