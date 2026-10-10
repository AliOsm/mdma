use super::*;
use std::os::unix::fs::symlink;

fn write(path: impl AsRef<Path>, value: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

fn fixture(root: &Path) -> std::path::PathBuf {
    let device =
        root.join("devices/pci0000:00/0000:00:01.0/0000:01:00.0/0000:02:00.0/0000:03:00.0");
    fs::create_dir_all(&device).unwrap();
    fs::create_dir_all(root.join("bus/pci/devices")).unwrap();
    symlink(&device, root.join("bus/pci/devices/0000:03:00.0")).unwrap();
    write(device.join("class"), "0x030000");
    write(device.join("vendor"), "0x1002");
    write(device.join("device"), "0x7551");
    write(device.join("mem_info_vram_total"), "34359738368");
    write(device.join("mem_info_vram_vendor"), "samsung");
    write(device.join("current_link_speed"), "32.0 GT/s PCIe");
    write(device.join("current_link_width"), "16");
    write(device.join("max_link_speed"), "32.0 GT/s PCIe");
    write(device.join("max_link_width"), "16");
    write(
        device.join("pp_dpm_sclk"),
        "S: 44Mhz *\n1: 500Mhz\n2: 2350Mhz",
    );
    fs::create_dir_all(root.join("class/drm/card2")).unwrap();
    symlink(&device, root.join("class/drm/card2/device")).unwrap();
    write(root.join("class/drm/card2-DP-2/status"), "connected");
    write(root.join("class/drm/card2-HDMI-A-1/status"), "disconnected");
    write(root.join("class/drm/card2-Writeback-1/status"), "unknown");
    device
}

#[test]
fn discrete_gpu_reports_slot_path_link_vram_and_display_connectors() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let names = pci_names(
        "Slot:\t0000:03:00.0\nVendor:\tAMD\nDevice:\tRadeon AI PRO R9700\nSVendor:\tSapphire\n\n",
    );
    let slots = BTreeMap::from([("0000:00:01.0".into(), "PCIEX16".into())]);
    let gpus = scan(dir.path(), &names, &slots).unwrap();
    assert_eq!(gpus.len(), 1);
    let gpu = &gpus[0];
    assert_eq!(gpu.model.as_deref(), Some("Radeon AI PRO R9700"));
    assert_eq!(gpu.board_manufacturer.as_deref(), Some("Sapphire"));
    assert_eq!(gpu.vram_bytes, Some(32 * 1024_u64.pow(3)));
    assert_eq!(gpu.vram_manufacturer.as_deref(), Some("samsung"));
    assert_eq!(gpu.current_link.speed_gt_s, Some(32.0));
    assert_eq!(gpu.current_link.width, Some(16));
    assert_eq!(gpu.physical_slot.as_deref(), Some("PCIEX16"));
    assert_eq!(
        gpu.pcie_path,
        [
            "0000:00:01.0",
            "0000:01:00.0",
            "0000:02:00.0",
            "0000:03:00.0"
        ]
    );
    assert_eq!(gpu.display_ports.len(), 2);
    assert_eq!(gpu.display_ports[0].name, "DP-2");
    assert_eq!(gpu.display_ports[0].status.as_deref(), Some("connected"));
    assert_eq!(gpu.core_clock.current_mhz, Some(44.0));
}

#[test]
fn upstream_acpi_slot_is_found_without_smbios() {
    let dir = tempfile::tempdir().unwrap();
    let device = fixture(dir.path());
    write(device.parent().unwrap().join("physical_slot"), "4");
    assert_eq!(
        pci::slot(dir.path(), &device, &BTreeMap::new()).as_deref(),
        Some("4")
    );
    fs::remove_file(device.parent().unwrap().join("physical_slot")).unwrap();
    write(dir.path().join("bus/pci/slots/6/address"), "0000:00:01");
    assert_eq!(
        pci::slot(dir.path(), &device, &BTreeMap::new()).as_deref(),
        Some("6")
    );
}

#[test]
fn shared_memory_and_unknown_links_are_not_invented() {
    let dir = tempfile::tempdir().unwrap();
    let device = fixture(dir.path());
    fs::remove_file(device.join("mem_info_vram_total")).unwrap();
    write(device.join("current_link_speed"), "Unknown");
    write(device.join("current_link_width"), "0");
    write(device.join("max_link_width"), "255");
    let gpu = scan(dir.path(), &Names::new(), &BTreeMap::new())
        .unwrap()
        .pop()
        .unwrap();
    assert!(gpu.vram_bytes.is_none());
    assert!(gpu.current_link.speed_gt_s.is_none());
    assert!(gpu.current_link.width.is_none());
    assert!(gpu.max_link.width.is_none());
    assert!(gpu.physical_slot.is_none());
    assert_eq!(gpu.manufacturer.as_deref(), Some("AMD"));
    assert!(gpu.model.is_none());
}

#[test]
fn missing_identifiers_and_connector_status_are_json_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let device = fixture(dir.path());
    fs::remove_file(device.join("vendor")).unwrap();
    fs::remove_file(device.join("device")).unwrap();
    fs::remove_file(dir.path().join("class/drm/card2-DP-2/status")).unwrap();
    let gpu = scan(dir.path(), &Names::new(), &BTreeMap::new())
        .unwrap()
        .pop()
        .unwrap();
    let json = serde_json::to_value(&gpu).unwrap();
    assert!(json["vendor_id"].is_null());
    assert!(json["device_id"].is_null());
    assert!(json["display_ports"][0]["status"].is_null());
}

#[test]
fn headless_pci_devices_are_found_but_audio_functions_are_not_gpus() {
    let dir = tempfile::tempdir().unwrap();
    let device = fixture(dir.path());
    fs::remove_dir_all(dir.path().join("class")).unwrap();
    let audio = dir.path().join("bus/pci/devices/0000:03:00.1");
    write(audio.join("class"), "0x040300");
    write(device.join("class"), "0x030200");
    let gpus = scan(dir.path(), &Names::new(), &BTreeMap::new()).unwrap();
    assert_eq!(gpus.len(), 1);
    assert!(gpus[0].display_ports.is_empty());
}

#[test]
fn nvidia_units_and_clock_profile_selection_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let mut gpu = scan(dir.path(), &Names::new(), &BTreeMap::new())
        .unwrap()
        .pop()
        .unwrap();
    nvidia_details(&mut gpu, "24576, 300, 2520, 405, 10501");
    assert_eq!(gpu.vram_bytes, Some(24 * 1024_u64.pow(3)));
    assert_eq!(gpu.core_clock.current_mhz, Some(300.0));
    assert_eq!(gpu.memory_clock.max_reported_mhz, Some(10501.0));
    nvidia_details(&mut gpu, "[N/A], [N/A], NaN, [N/A], inf");
    assert!(gpu.core_clock.current_mhz.is_none());
    assert!(gpu.memory_clock.max_reported_mhz.is_none());
    let clock = dpm_clock("0: 96Mhz\n1: 1258Mhz *\n2: NaNMhz");
    assert_eq!(clock.current_mhz, Some(1258.0));
    assert_eq!(clock.max_reported_mhz, Some(1258.0));
    assert!(dpm_clock("").current_mhz.is_none());
    let idle = dpm_clock("0: 0Mhz *\n1: 500Mhz");
    assert_eq!(idle.current_mhz, Some(0.0));
    assert_eq!(idle.max_reported_mhz, Some(500.0));
}
