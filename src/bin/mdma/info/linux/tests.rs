use super::*;

#[test]
fn os_totals_are_kib_not_kilobytes_and_missing_is_not_zero() {
    let memory = memory_totals("MemTotal: 131072 kB\nMemAvailable: 32768 kB\nMemFree: 123 kB");
    assert_eq!(memory.total_bytes, Some(128 * 1024 * 1024));
    assert_eq!(memory.available_bytes, Some(32 * 1024 * 1024));
    assert!(memory.installed_bytes.is_none());
    let memory = memory_totals("MemTotal: 123 MB\nMemAvailable: 18446744073709551615 kB");
    assert!(memory.total_bytes.is_none());
    assert!(memory.available_bytes.is_none());
    assert!(memory_totals("").total_bytes.is_none());
}

#[test]
fn explicit_dmi_file_errors_are_not_silently_ignored() {
    let dir = tempfile::tempdir().unwrap();
    assert!(collect(Some(&dir.path().join("missing.txt"))).is_err());
}
