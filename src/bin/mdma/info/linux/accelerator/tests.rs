use super::*;
use std::os::unix::fs::symlink;

fn write(path: impl AsRef<Path>, value: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

fn pci_device(root: &Path, address: &str, vendor: &str, device: &str) -> std::path::PathBuf {
    let path = root.join("devices/pci0000:00/0000:00:1c.0").join(address);
    write(path.join("vendor"), vendor);
    write(path.join("device"), device);
    fs::create_dir_all(root.join("bus/pci/devices")).unwrap();
    symlink(&path, root.join("bus/pci/devices").join(address)).unwrap();
    path
}

fn usb_device(root: &Path, port: &str, vendor: &str, product: &str, speed: &str) {
    let path = root.join("devices/pci0000:00/0000:00:14.0/usb2").join(port);
    write(path.join("idVendor"), vendor);
    write(path.join("idProduct"), product);
    write(path.join("speed"), speed);
    fs::create_dir_all(root.join("bus/usb/devices")).unwrap();
    symlink(&path, root.join("bus/usb/devices").join(port)).unwrap();
}

#[test]
fn pcie_coral_reports_driver_node_status_temperature_and_link() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let device = pci_device(root, "0000:01:00.0", "0x1ac1", "0x089a");
    write(device.join("current_link_speed"), "5.0 GT/s PCIe");
    write(device.join("current_link_width"), "1");
    write(device.join("max_link_speed"), "5.0 GT/s PCIe");
    write(device.join("max_link_width"), "1");
    fs::create_dir_all(root.join("bus/pci/drivers/apex")).unwrap();
    symlink(root.join("bus/pci/drivers/apex"), device.join("driver")).unwrap();
    fs::create_dir_all(root.join("class/apex/apex_0")).unwrap();
    symlink(&device, root.join("class/apex/apex_0/device")).unwrap();
    write(root.join("class/apex/apex_0/status"), "ALIVE");
    write(root.join("class/apex/apex_0/temp"), "64800");
    pci_device(root, "0000:02:00.0", "0x1002", "0x7551");

    let mut warnings = Vec::new();
    let found = collect(root, &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(found.len(), 1, "only the Coral is an accelerator");
    let coral = &found[0];
    assert_eq!((coral.kind, coral.bus), ("coral_edge_tpu", "pci"));
    assert_eq!(coral.address.as_deref(), Some("0000:01:00.0"));
    assert_eq!(
        (coral.vendor_id.as_str(), coral.device_id.as_str()),
        ("0x1ac1", "0x089a")
    );
    assert_eq!(coral.driver.as_deref(), Some("apex"));
    assert_eq!(coral.device_node.as_deref(), Some("/dev/apex_0"));
    assert_eq!(coral.status.as_deref(), Some("ALIVE"));
    assert_eq!(coral.temperature_c, Some(64.8));
    assert_eq!(
        (coral.current_link.speed_gt_s, coral.current_link.width),
        (Some(5.0), Some(1))
    );
    assert_eq!(coral.firmware_loaded, None);
}

#[test]
fn pcie_coral_without_driver_is_reported_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    pci_device(dir.path(), "0000:05:00.0", "0x1ac1", "0x089a");
    let mut warnings = Vec::new();
    let found = collect(dir.path(), &mut warnings);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (
            found[0].driver.as_deref(),
            found[0].device_node.as_deref(),
            found[0].status.as_deref()
        ),
        (None, None, None)
    );
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("0000:05:00.0") && warnings[0].contains("no driver"),
        "{warnings:?}"
    );
}

#[test]
fn usb_coral_distinguishes_bootloader_from_loaded_firmware() {
    let dir = tempfile::tempdir().unwrap();
    usb_device(dir.path(), "2-1", "1a6e", "089a", "5000");
    usb_device(dir.path(), "2-2", "18d1", "9302", "5000");
    usb_device(dir.path(), "1-3", "046d", "c52b", "12");
    let mut warnings = Vec::new();
    let found = collect(dir.path(), &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    let summary: Vec<_> = found
        .iter()
        .map(|a| {
            (
                a.bus,
                a.address.as_deref(),
                a.vendor_id.as_str(),
                a.firmware_loaded,
                a.usb_speed_mbps,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("usb", Some("2-1"), "0x1a6e", Some(false), Some(5000)),
            ("usb", Some("2-2"), "0x18d1", Some(true), Some(5000))
        ]
    );
}

#[test]
fn missing_sysfs_trees_yield_no_accelerators_and_no_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let mut warnings = Vec::new();
    assert!(collect(dir.path(), &mut warnings).is_empty());
    assert!(warnings.is_empty(), "{warnings:?}");
}
