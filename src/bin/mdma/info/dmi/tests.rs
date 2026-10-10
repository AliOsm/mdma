use super::*;

const DMI: &str = r"# dmidecode 3.7
Handle 0x0001, DMI type 16, 23 bytes
Physical Memory Array
    Maximum Capacity: 128 GB

Handle 0x0002, DMI type 17, 92 bytes
Memory Device
    Size: 32 GB
    Form Factor: DIMM
    Locator: DIMM_A2
    Bank Locator: BANK 0
    Type: DDR5
    Speed: 6000 MT/s
    Manufacturer: Kingston
    Serial Number: deliberately-not-collected
    Part Number: KF560C36-32
    Configured Memory Speed: 5600 MT/s

Handle 0x0003, DMI type 17, 92 bytes
Memory Device
    Size: No Module Installed
    Locator: DIMM_A1
    Type: Unknown

Handle 0x0004, DMI type 17, 92 bytes
Memory Device
    Size: 32768 MB
    Locator: DIMM_B2
    Type: DDR5
    Manufacturer: Not Specified
    Speed: 6000 MT/s
    Configured Memory Speed: Unknown

Handle 0x0005, DMI type 9, 17 bytes
System Slot Information
    Designation: PCIEX16
    Bus Address: 0000:00:01.0

Handle 0x0006, DMI type 9, 17 bytes
System Slot Information
    Designation: Not Specified
    Bus Address: Unknown
";

#[test]
fn extracts_installed_dimms_and_preserves_rated_vs_configured_speed() {
    let modules = memory_modules(DMI);
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0].size_bytes, Some(32 * 1024_u64.pow(3)));
    assert_eq!(modules[0].slot.as_deref(), Some("DIMM_A2"));
    assert_eq!(modules[0].manufacturer.as_deref(), Some("Kingston"));
    assert_eq!(modules[0].memory_type.as_deref(), Some("DDR5"));
    assert_eq!(modules[0].configured_speed.as_ref().unwrap().value, 5600);
    assert_eq!(modules[0].rated_speed.as_ref().unwrap().value, 6000);
    assert_eq!(modules[1].size_bytes, modules[0].size_bytes);
    assert!(modules[1].manufacturer.is_none());
    assert!(modules[1].configured_speed.is_none());
    let json = serde_json::to_string(&modules).unwrap();
    assert!(!json.contains("deliberately-not-collected"));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn firmware_slot_designations_are_distinct_from_bus_addresses() {
    let slots = slots(DMI);
    assert_eq!(slots.len(), 1);
    assert_eq!(
        slots.get("0000:00:01.0").map(String::as_str),
        Some("PCIEX16")
    );
}

#[test]
fn legacy_units_unknown_capacities_and_missing_fields_are_preserved() {
    let modules = memory_modules(
        "Handle 1\nMemory Device\n Size: Unknown\n Speed: 2400 MHz\n Configured Clock Speed: 2133 MHz\nManufacturer: Unknown\n",
    );
    assert_eq!(modules.len(), 1);
    assert!(modules[0].size_bytes.is_none());
    assert!(modules[0].slot.is_none());
    let speed = modules[0].configured_speed.as_ref().unwrap();
    assert_eq!(speed.value, 2133);
    assert_eq!(speed.unit, "MHz");
    assert!(memory_modules("").is_empty());
    assert!(memory_modules("Handle 1\nMemory Device\nSize: 0 MB\n").is_empty());
    assert_eq!(capacity("18446744073709551615 TB"), None);
    assert_eq!(capacity("1 TB"), Some(1024_u64.pow(4)));
    assert!(memory_speed("Unknown").is_none());
    assert!(memory_speed("0 MT/s").is_none());
}
