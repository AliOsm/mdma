use super::*;

fn values() -> BTreeMap<String, String> {
    [
        ("hw.model", "AMD Ryzen 9 5950X 16-Core Processor"),
        ("hw.ncpu", "32"),
        ("kern.smp.cores", "16"),
        ("hw.clockrate", "3400"),
        ("dev.cpu.0.freq", "2800"),
        ("dev.cpu.0.freq_levels", "3400/3740 2800/2800 2200/1980"),
        ("hw.physmem", "68588720128"),
        ("hw.realmem", "68719476736"),
        ("vm.stats.vm.v_page_size", "4096"),
        ("vm.stats.vm.v_free_count", "100"),
        ("vm.stats.vm.v_inactive_count", "20"),
        ("vm.stats.vm.v_cache_count", "5"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}

#[test]
fn native_cpu_and_ram_counts_do_not_assume_socket_topology() {
    let cpu = cpu(&values());
    assert_eq!(cpu.manufacturer.as_deref(), Some("AMD"));
    assert_eq!(cpu.physical_cores, Some(16));
    assert_eq!(cpu.logical_threads, Some(32));
    assert!(cpu.sockets.is_none());
    assert_eq!(cpu.nominal_mhz, Some(3400.0));
    assert_eq!(cpu.current_mhz, Some(2800.0));
    assert_eq!(cpu.min_mhz, Some(2200.0));
    let memory = memory(&values());
    assert_eq!(memory.installed_bytes, Some(64 * 1024_u64.pow(3)));
    assert_eq!(memory.available_bytes, Some(125 * 4096));
    assert!(memory.available_is_estimate);
}

#[test]
fn pciconf_addresses_are_decimal_selectors_not_hex_bus_numbers() {
    assert_eq!(
        pci::address("pci0:40:0:0:").as_deref(),
        Some("0000:28:00.0")
    );
    assert_eq!(
        pci::address("pci1:255:31:7").as_deref(),
        Some("0001:ff:1f.7")
    );
    assert!(pci::address("pci0:256:0:0").is_none());
}

#[test]
fn gpu_identity_and_capability_links_are_distinct_from_bar_sizes() {
    let gpus = pci::parse(
        "vgapci0@pci0:40:0:0:\tclass=0x030000 vendor=0x1002 device=0x73bf\n    vendor = 'AMD'\n    device = 'Navi 21'\n    cap 10[80] = PCI-Express 2 endpoint\n                 link x8(x16) speed 8.0(16.0)\n    bar [10] = type Prefetchable Memory, range 64, size 17179869184\nhdac0@pci0:40:0:1:\tclass=0x040300 vendor=0x1002 device=0xab28\n",
    );
    assert_eq!(gpus.len(), 1);
    let gpu = &gpus[0];
    assert_eq!(gpu.pci_address.as_deref(), Some("0000:28:00.0"));
    assert_eq!(gpu.vendor_id.as_deref(), Some("0x1002"));
    assert_eq!(gpu.current_link.width, Some(8));
    assert_eq!(gpu.current_link.speed_gt_s, Some(8.0));
    assert_eq!(gpu.max_link.width, Some(16));
    assert_eq!(gpu.max_link.speed_gt_s, Some(16.0));
    assert!(gpu.vram_bytes.is_none());
    assert!(gpu.driver.is_none());
}

#[test]
fn unprivileged_inventory_keeps_unknown_links_null() {
    let gpu = pci::parse("vgapci0@pci0:40:0:0:\tclass=0x030000 vendor=0x1002 device=0x73bf\n    device = 'Navi 21'\n").pop().unwrap();
    assert!(gpu.current_link.width.is_none());
    assert!(gpu.max_link.speed_gt_s.is_none());
    assert_eq!(gpu.model.as_deref(), Some("Navi 21"));
}

#[test]
fn coral_records_report_driver_node_and_link_whatever_their_class() {
    let text = "apex0@pci0:2:0:0:\tclass=0x0000ff rev=0x00 hdr=0x00 vendor=0x1ac1 device=0x089a subvendor=0x1ac1 subdevice=0x089a\n    vendor     = 'Global Unichip Corp.'\n    device     = 'Coral Edge TPU'\n    cap 10[80] = PCI-Express 2 endpoint max data 256(256) RO NS\n                 link x1(x1) speed 5.0(5.0) ASPM disabled(L0s/L1) ClockPM enabled\nnone3@pci0:5:0:0:\tclass=0x088000 vendor=0x1ac1 device=0x089a\nvgapci0@pci0:40:0:0:\tclass=0x030000 vendor=0x1002 device=0x73bf\n";
    let found = pci::coral(text);
    assert_eq!(found.len(), 2, "only Coral records");
    let bound = &found[0];
    assert_eq!(bound.address.as_deref(), Some("0000:02:00.0"));
    assert_eq!(bound.driver.as_deref(), Some("apex"));
    assert_eq!(bound.device_node.as_deref(), Some("/dev/apex0"));
    assert_eq!((bound.current_link.width, bound.current_link.speed_gt_s), (Some(1), Some(5.0)));
    let unbound = &found[1];
    assert_eq!((unbound.address.as_deref(), unbound.driver.as_deref(), unbound.device_node.as_deref()), (Some("0000:05:00.0"), None, None));
}

#[test]
fn apex_temperature_sysctl_is_parsed_in_celsius() {
    assert_eq!(celsius("69.5C"), Some(69.5));
    assert_eq!(celsius(" 70C\n"), Some(70.0));
    assert_eq!(celsius("69.5"), None);
    assert_eq!(celsius("hot"), None);
}
