use super::*;
use serde_json::json;

#[test]
fn apple_silicon_has_unified_memory_not_fabricated_vram_or_pci_address() {
    let profile = json!({
        "SPHardwareDataType": [{"chip_type": "Apple M1", "serial_number": "never-report"}],
        "SPMemoryDataType": [{"SPMemoryDataType": "16 GB", "dimm_type": "LPDDR4X", "dimm_manufacturer": "Hynix"}],
        "SPDisplaysDataType": [{"_name": "kHW_AppleM1Item", "sppci_model": "Apple M1", "spdisplays_vendor": "sppci_vendor_Apple", "sppci_bus": "spdisplays_builtin",
            "spdisplays_ndrvs": [{"_name": "Color LCD", "spdisplays_connection_type": "spdisplays_internal", "spdisplays_online": "spdisplays_yes"}]}]
    });
    let values = [("hw.physicalcpu", "8"), ("hw.logicalcpu", "8")]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    let cpu = cpu(&values, &profile);
    assert_eq!(cpu.model.as_deref(), Some("Apple M1"));
    assert_eq!(cpu.manufacturer.as_deref(), Some("Apple"));
    assert_eq!(cpu.physical_cores, Some(8));
    assert!(cpu.max_mhz.is_none());
    let modules = profiler::memory(&profile);
    assert_eq!(modules[0].memory_type.as_deref(), Some("LPDDR4X"));
    assert_eq!(modules[0].size_bytes, Some(16 * 1024_u64.pow(3)));
    assert_eq!(modules[0].manufacturer.as_deref(), Some("Hynix"));
    let gpu = profiler::gpus(&profile).pop().unwrap();
    assert_eq!(gpu.model.as_deref(), Some("Apple M1"));
    assert_eq!(gpu.bus.as_deref(), Some("built_in"));
    assert_eq!(gpu.memory_kind.as_deref(), Some("unified"));
    assert!(gpu.vram_bytes.is_none());
    assert!(gpu.pci_address.is_none());
    assert_eq!(gpu.display_ports[0].status.as_deref(), Some("connected"));
    assert_eq!(gpu.display_ports[0].name, "Color LCD (internal)");
    assert!(
        !serde_json::to_string(&gpu)
            .unwrap()
            .contains("never-report")
    );
}

#[test]
fn intel_mac_dimms_and_discrete_gpu_are_not_treated_as_apple_unified_memory() {
    let profile = json!({
        "SPMemoryDataType": [{"_items": [
            {"_name": "BANK 0/DIMM0", "dimm_size": "8 GB", "dimm_type": "DDR4", "dimm_speed": "2400 MHz", "dimm_manufacturer": "Samsung"},
            {"_name": "BANK 1/DIMM0", "dimm_size": "Empty"}]}],
        "SPDisplaysDataType": [
            {"_name": "AMD Radeon", "spdisplays_vendor": "spdisplays_vendor_amd", "spdisplays_bus": "sppci_pcie", "sppci_slot": "Slot-1", "spdisplays_vram": "8 GB", "spdisplays_pcie_lane_width": "x16"},
            {"_name": "Intel UHD", "spdisplays_vendor": "Intel (0x8086)", "spdisplays_vram_shared": "1536 MB"}]
    });
    let modules = profiler::memory(&profile);
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].configured_speed.as_ref().unwrap().value, 2400);
    assert!(modules[0].rated_speed.is_none());
    let gpus = profiler::gpus(&profile);
    assert_eq!(gpus[0].manufacturer.as_deref(), Some("AMD"));
    assert_eq!(gpus[0].physical_slot.as_deref(), Some("Slot-1"));
    assert_eq!(gpus[0].memory_kind.as_deref(), Some("dedicated"));
    assert_eq!(gpus[0].vram_bytes, Some(8 * 1024_u64.pow(3)));
    assert_eq!(gpus[0].current_link.width, Some(16));
    assert_eq!(gpus[1].memory_kind.as_deref(), Some("shared"));
    assert!(gpus[1].vram_bytes.is_none());
}

#[test]
fn vm_stat_uses_its_reported_page_size_without_double_counting_purgeable_pages() {
    assert_eq!(
        available(
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages inactive: 20.\nPages purgeable: 15.\n"
        ),
        Some(30 * 16384)
    );
    assert!(available("").is_none());
}

#[test]
fn nominal_intel_frequency_is_not_an_observed_clock() {
    let values = [
        ("machdep.cpu.brand_string", "Intel Core i7"),
        ("hw.cpufrequency", "2300000000"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let cpu = cpu(&values, &json!({}));
    assert_eq!(cpu.nominal_mhz, Some(2300.0));
    assert!(cpu.current_mhz.is_none());
}
