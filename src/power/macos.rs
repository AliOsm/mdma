use super::{PowerReading, PowerSource};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFString, CFType};
use objc2_io_kit::{
    IOPSCopyPowerSourcesInfo, IOPSCopyPowerSourcesList, IOPSGetPowerSourceDescription,
};
use std::io;

/// macOS internal battery state through IOKit.
pub struct IokitPower;

impl PowerSource for IokitPower {
    fn read(&mut self) -> io::Result<Option<PowerReading>> {
        let info = IOPSCopyPowerSourcesInfo()
            .ok_or_else(|| io::Error::other("IOKit power-source snapshot unavailable"))?;
        // SAFETY: info is a live snapshot returned by IOPSCopyPowerSourcesInfo.
        let sources = unsafe { IOPSCopyPowerSourcesList(Some(&info)) }
            .ok_or_else(|| io::Error::other("IOKit power-source list unavailable"))?;
        // SAFETY: IOPSCopyPowerSourcesList returns an array of CFType handles.
        let sources = unsafe { sources.cast_unchecked::<CFType>() };
        for source in sources {
            // SAFETY: source belongs to this snapshot, and both remain retained.
            let description = unsafe { IOPSGetPowerSourceDescription(Some(&info), Some(&source)) }
                .ok_or_else(|| io::Error::other("IOKit power-source description unavailable"))?;
            // SAFETY: IOKit descriptions have CFString keys and CFType values.
            let description = unsafe { description.cast_unchecked::<CFString, CFType>() };
            if let Some(reading) = battery_reading(description) {
                return Ok(Some(reading));
            }
        }
        Ok(None)
    }
}

fn battery_reading(description: &CFDictionary<CFString, CFType>) -> Option<PowerReading> {
    let string = |key: &str| {
        let value = description.get(&CFString::from_str(key))?;
        Some(value.downcast_ref::<CFString>()?.to_string())
    };
    let number = |key: &str| {
        let value = description.get(&CFString::from_str(key))?;
        value.downcast_ref::<CFNumber>()?.as_i32()
    };
    if string("Type").as_deref() != Some("InternalBattery") {
        return None;
    }
    let present = description.get(&CFString::from_static_str("Is Present"))?;
    if !present.downcast_ref::<CFBoolean>()?.as_bool() {
        return None;
    }
    let on_battery = match string("Power Source State").as_deref() {
        Some("Battery Power") => true,
        Some("AC Power") => false,
        _ => return None,
    };
    let battery_percent = match (number("Current Capacity"), number("Max Capacity")) {
        (Some(current), Some(max)) if current >= 0 && max > 0 => {
            Some(((u64::from(current.cast_unsigned()) * 100 / u64::from(max.cast_unsigned())).min(100)) as u8)
        }
        _ => None,
    };
    Some(PowerReading {
        on_battery,
        battery_percent,
        gpu: None,
    })
}

#[cfg(test)]
mod tests;
