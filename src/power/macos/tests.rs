use super::*;
use crate::power::{ServerState, WatchPolicy, classify};
use objc2_core_foundation::{CFMutableDictionary, CFRetained};

fn battery(
    state: &str,
    current: i32,
    max: i32,
) -> CFRetained<CFMutableDictionary<CFString, CFType>> {
    let description = CFMutableDictionary::<CFString, CFType>::empty();
    description.set(
        &CFString::from_static_str("Type"),
        &CFString::from_static_str("InternalBattery"),
    );
    description.set(
        &CFString::from_static_str("Is Present"),
        CFBoolean::new(true),
    );
    description.set(
        &CFString::from_static_str("Power Source State"),
        &CFString::from_str(state),
    );
    description.set(
        &CFString::from_static_str("Current Capacity"),
        &CFNumber::new_i32(current),
    );
    description.set(
        &CFString::from_static_str("Max Capacity"),
        &CFNumber::new_i32(max),
    );
    description
}

#[test]
fn battery_and_ac_readings_drive_existing_policy() {
    let policy = WatchPolicy::default();
    for (state, capacity, expected) in [
        ("Battery Power", 63, ServerState::Degraded),
        ("Battery Power", 10, ServerState::Critical),
        ("AC Power", 5, ServerState::Serving),
        ("AC Power", 100, ServerState::Serving),
    ] {
        let reading = battery_reading(&battery(state, capacity, 100)).unwrap();
        assert_eq!(reading.on_battery, state == "Battery Power");
        assert_eq!(
            reading.battery_percent,
            Some(u8::try_from(capacity).unwrap())
        );
        assert_eq!(reading.gpu, None);
        assert_eq!(classify(Some(reading), &policy), expected);
    }
}

#[test]
fn capacity_is_scaled_and_clamped() {
    for (current, max, expected) in [
        (2400, 4800, Some(50)),
        (1, 3, Some(33)),
        (0, 100, Some(0)),
        (120, 100, Some(100)),
        (i32::MAX, i32::MAX, Some(100)),
        (-1, 100, None),
        (50, 0, None),
        (50, -1, None),
    ] {
        let reading = battery_reading(&battery("Battery Power", current, max)).unwrap();
        assert!(reading.on_battery);
        assert_eq!(reading.battery_percent, expected);
    }
}

#[test]
fn missing_or_malformed_capacity_preserves_battery_state() {
    for key in ["Current Capacity", "Max Capacity"] {
        let description = battery("Battery Power", 50, 100);
        description.remove(&CFString::from_str(key));
        let reading = battery_reading(&description).unwrap();
        assert!(reading.on_battery);
        assert_eq!(reading.battery_percent, None);

        description.set(
            &CFString::from_str(key),
            &CFString::from_static_str("unknown"),
        );
        assert_eq!(battery_reading(&description), Some(reading));
    }
}

#[test]
fn absent_and_non_internal_batteries_are_ignored() {
    let description = battery("Battery Power", 5, 100);
    description.set(
        &CFString::from_static_str("Is Present"),
        CFBoolean::new(false),
    );
    assert_eq!(battery_reading(&description), None);
    for kind in ["UPS", "Accessory", ""] {
        let description = battery("Battery Power", 5, 100);
        description.set(
            &CFString::from_static_str("Type"),
            &CFString::from_str(kind),
        );
        assert_eq!(battery_reading(&description), None);
    }
}

#[test]
fn missing_or_unknown_supply_state_is_not_reported() {
    assert_eq!(battery_reading(&CFDictionary::empty()), None);
    for state in ["Off Line", "unknown"] {
        assert_eq!(battery_reading(&battery(state, 50, 100)), None);
    }
    let description = battery("Battery Power", 50, 100);
    description.remove(&CFString::from_static_str("Power Source State"));
    assert_eq!(battery_reading(&description), None);
}

#[test]
#[ignore = "reads host IOKit power sources"]
fn native_power_source_read() {
    let reading = crate::power::default_source()
        .read()
        .expect("read IOKit power sources");
    if let Some(reading) = reading {
        assert!(reading.battery_percent.is_none_or(|percent| percent <= 100));
        assert_eq!(reading.gpu, None);
    }
    eprintln!("{reading:?}");
}
