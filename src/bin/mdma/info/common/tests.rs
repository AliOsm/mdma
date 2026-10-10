use super::*;

#[test]
fn capacity_handles_native_units_and_fractional_vram() {
    assert_eq!(capacity("16 GB"), Some(16 * 1024_u64.pow(3)));
    assert_eq!(capacity("1.5 GB"), Some(1536 * 1024_u64.pow(2)));
    assert_eq!(capacity("4096 MiB"), Some(4 * 1024_u64.pow(3)));
    assert_eq!(capacity("18446744073709551615 TB"), None);
    assert_eq!(capacity("Empty"), None);
}

#[test]
fn missing_labels_and_speed_units_are_preserved() {
    assert!(known("Unknown").is_none());
    assert!(known("Empty").is_none());
    assert_eq!(known(" Hynix ").as_deref(), Some("Hynix"));
    assert_eq!(memory_speed("5600 MT/s").unwrap().unit, "MT/s");
    assert_eq!(memory_speed("2133 MHz").unwrap().unit, "MHz");
}
