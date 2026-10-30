use cidre::{cf, core_audio as ca};

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

/// The transport of the device with this UID, or `None` when the HAL cannot say.
pub(crate) fn transport_type(uid: &str) -> Option<ca::DeviceTransportType> {
    let device = match ca::Device::with_uid(&cf::String::from_str(uid)) {
        Ok(device) if !device.is_unknown() => device,
        Ok(_) => {
            tracing::warn!(device.uid = uid, "hal.uid_unknown");
            return None;
        }
        Err(error) => {
            tracing::warn!(
                coreaudio.os_status = error.0.get(),
                device.uid = uid,
                "hal.uid_lookup_failed"
            );
            return None;
        }
    };
    device
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
