//! Power gauges for the host sample: the power source, the battery's charge, Low Power Mode and
//! the thermal state. A value the OS does not report leaves its gauge out of the sample.

use crate::host_metrics::HostSample;

/// What powers the machine now.
#[derive(Clone, Copy)]
enum PowerSource {
    Ac,
    Battery,
}

impl PowerSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ac => "ac",
            Self::Battery => "battery",
        }
    }
}

/// OpenTelemetry's `hw.battery.state` values.
#[derive(Clone, Copy)]
enum BatteryState {
    Charging,
    Discharging,
}

impl BatteryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
        }
    }
}

/// One read of the machine's power.
#[derive(Default)]
struct PowerReading {
    source: Option<PowerSource>,
    /// Share of a full charge, 0 to 1.
    battery_charge: Option<f64>,
    /// Absent on AC once the battery is full and holds its charge.
    battery_state: Option<BatteryState>,
    is_low_power_mode: Option<bool>,
    /// The OS's thermal pressure level: 0 nominal, 1 fair, 2 serious, 3 critical.
    thermal_level: Option<u8>,
}

/// The power gauges for one host sample.
pub(crate) fn power_samples() -> Vec<HostSample> {
    let reading = read_power();
    let mut samples = Vec::new();
    if let Some(source) = reading.source {
        samples.push(HostSample {
            name: "mc.host.power.source",
            unit: "1",
            attributes: vec![("mc.host.power.source", source.as_str())],
            value: 1.0,
        });
    }
    if let Some(charge) = reading.battery_charge {
        let attributes = reading
            .battery_state
            .map(|state| vec![("hw.battery.state", state.as_str())])
            .unwrap_or_default();
        samples.push(HostSample {
            name: "hw.battery.charge",
            unit: "1",
            attributes,
            value: charge,
        });
    }
    if let Some(is_low_power_mode) = reading.is_low_power_mode {
        samples.push(HostSample {
            name: "mc.host.power.low_power_mode",
            unit: "1",
            attributes: Vec::new(),
            value: if is_low_power_mode { 1.0 } else { 0.0 },
        });
    }
    if let Some(level) = reading.thermal_level {
        samples.push(HostSample {
            name: "mc.host.thermal.state",
            unit: "1",
            attributes: Vec::new(),
            value: f64::from(level),
        });
    }
    samples
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn read_power() -> PowerReading {
    use objc2_core_foundation::{
        CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
    };
    use objc2_foundation::{NSProcessInfo, NSProcessInfoThermalState};
    use objc2_io_kit::{
        IOPSCopyPowerSourcesInfo, IOPSCopyPowerSourcesList, IOPSGetPowerSourceDescription,
        IOPSGetProvidingPowerSourceType,
    };

    let mut reading = PowerReading::default();
    let process_info = NSProcessInfo::processInfo();
    reading.is_low_power_mode = Some(process_info.isLowPowerModeEnabled());
    reading.thermal_level = match process_info.thermalState() {
        NSProcessInfoThermalState::Nominal => Some(0),
        NSProcessInfoThermalState::Fair => Some(1),
        NSProcessInfoThermalState::Serious => Some(2),
        NSProcessInfoThermalState::Critical => Some(3),
        // A level a later macOS adds has no meaning here yet.
        _ => None,
    };

    let Some(snapshot) = IOPSCopyPowerSourcesInfo() else {
        return reading;
    };
    // SAFETY: the snapshot comes from IOPSCopyPowerSourcesInfo, as the call requires.
    let providing = unsafe { IOPSGetProvidingPowerSourceType(Some(&snapshot)) };
    reading.source = providing.and_then(|source| match source.to_string().as_str() {
        "AC Power" => Some(PowerSource::Ac),
        "Battery Power" => Some(PowerSource::Battery),
        // Only a Mac reports "UPS Power", so the source values both OSes share leave it out.
        _ => None,
    });

    // SAFETY: the snapshot comes from IOPSCopyPowerSourcesInfo, as the call requires.
    let Some(sources) = (unsafe { IOPSCopyPowerSourcesList(Some(&snapshot)) }) else {
        return reading;
    };
    // SAFETY: IOPSCopyPowerSourcesList returns an array of CFTypeRef power source handles.
    let sources = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(sources) };
    let type_key = CFString::from_static_str("Type");
    let current_key = CFString::from_static_str("Current Capacity");
    let max_key = CFString::from_static_str("Max Capacity");
    let charging_key = CFString::from_static_str("Is Charging");
    for source in sources.iter() {
        // SAFETY: the source handle comes from the same snapshot's list.
        let Some(description) =
            (unsafe { IOPSGetPowerSourceDescription(Some(&snapshot), Some(&source)) })
        else {
            continue;
        };
        // SAFETY: a power source description is a dictionary keyed by CFString.
        let description =
            unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(description) };
        let is_internal_battery = description
            .get(&type_key)
            .and_then(|kind| kind.downcast::<CFString>().ok())
            .is_some_and(|kind| kind.to_string() == "InternalBattery");
        if !is_internal_battery {
            continue;
        }
        let capacity = |key: &CFString| {
            description
                .get(key)
                .and_then(|number| number.downcast::<CFNumber>().ok())
                .and_then(|number| number.as_f64())
        };
        if let (Some(current), Some(max)) = (capacity(&current_key), capacity(&max_key))
            && max > 0.0
        {
            reading.battery_charge = Some((current / max).clamp(0.0, 1.0));
        }
        let is_charging = description
            .get(&charging_key)
            .and_then(|flag| flag.downcast::<CFBoolean>().ok())
            .is_some_and(|flag| flag.as_bool());
        reading.battery_state = if is_charging {
            Some(BatteryState::Charging)
        } else if matches!(reading.source, Some(PowerSource::Battery)) {
            Some(BatteryState::Discharging)
        } else {
            None
        };
        break;
    }
    reading
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn read_power() -> PowerReading {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    const AC_OFFLINE: u8 = 0;
    const AC_ONLINE: u8 = 1;
    const BATTERY_CHARGING: u8 = 8;
    const NO_SYSTEM_BATTERY: u8 = 128;
    const UNKNOWN: u8 = 255;
    const BATTERY_SAVER_ON: u8 = 1;

    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: the pointer is to a live SYSTEM_POWER_STATUS the call fills.
    if unsafe { GetSystemPowerStatus(&raw mut status) }.is_err() {
        return PowerReading::default();
    }
    let source = match status.ACLineStatus {
        AC_OFFLINE => Some(PowerSource::Battery),
        AC_ONLINE => Some(PowerSource::Ac),
        _ => None,
    };
    let has_battery = status.BatteryFlag != NO_SYSTEM_BATTERY && status.BatteryFlag != UNKNOWN;
    let battery_charge = (has_battery && status.BatteryLifePercent <= 100)
        .then(|| f64::from(status.BatteryLifePercent) / 100.0);
    let battery_state = if !has_battery {
        None
    } else if status.BatteryFlag & BATTERY_CHARGING != 0 {
        Some(BatteryState::Charging)
    } else if status.ACLineStatus == AC_OFFLINE {
        Some(BatteryState::Discharging)
    } else {
        None
    };
    PowerReading {
        source,
        battery_charge,
        battery_state,
        // Battery saver is Windows' Low Power Mode.
        is_low_power_mode: Some(status.SystemStatusFlag == BATTERY_SAVER_ON),
        thermal_level: None,
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_power() -> PowerReading {
    PowerReading::default()
}
