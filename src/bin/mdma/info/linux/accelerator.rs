//! Coral Edge TPU detection from sysfs. Detection only: no device is opened.
use super::super::report::Accelerator;
use super::gpu::pci;
use super::read;
use std::{fs, io, path::Path};

const CORAL_PCI: (&str, &str) = ("0x1ac1", "0x089a");
/// The USB Accelerator enumerates as Global Unichip until its runtime
/// firmware is uploaded, then re-enumerates as Google.
const CORAL_USB_BOOTLOADER: (&str, &str) = ("1a6e", "089a");
const CORAL_USB_RUNTIME: (&str, &str) = ("18d1", "9302");

pub(super) fn collect(sys: &Path, warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let mut found = Vec::new();
    match entries(&sys.join("bus/pci/devices")) {
        Ok(devices) => found.extend(
            devices
                .iter()
                .filter_map(|device| pci_coral(sys, device, warnings)),
        ),
        // The GPU scan already reports an unreadable PCI tree.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!("Coral PCIe detection unavailable: {error}.")),
    }
    match entries(&sys.join("bus/usb/devices")) {
        Ok(devices) => found.extend(devices.iter().filter_map(|device| usb_coral(device))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!("Coral USB detection unavailable: {error}.")),
    }
    found
}

fn entries(dir: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut paths: Vec<_> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    Ok(paths)
}

fn pci_coral(sys: &Path, device: &Path, warnings: &mut Vec<String>) -> Option<Accelerator> {
    let ids = (read(device.join("vendor"))?, read(device.join("device"))?);
    if (ids.0.as_str(), ids.1.as_str()) != CORAL_PCI {
        return None;
    }
    let address = device.file_name()?.to_string_lossy().to_ascii_lowercase();
    let driver = link_name(&device.join("driver"));
    if driver.is_none() {
        warnings.push(format!("Coral PCIe device {address} has no driver bound; the apex/gasket kernel module provides /dev/apex_N."));
    }
    let node = apex_node(sys, device);
    let class = node.as_ref().map(|name| sys.join("class/apex").join(name));
    let attribute = |name: &str| class.as_ref().and_then(|dir| read(dir.join(name)));
    Some(Accelerator {
        kind: "coral_edge_tpu",
        bus: "pci",
        address: Some(address),
        vendor_id: ids.0,
        device_id: ids.1,
        driver,
        device_node: node.map(|name| format!("/dev/{name}")),
        status: attribute("status"),
        temperature_c: attribute("temp").and_then(|milli| millidegrees(&milli)),
        firmware_loaded: None,
        usb_speed_mbps: None,
        current_link: pci::link(device, "current"),
        max_link: pci::link(device, "max"),
    })
}

fn usb_coral(device: &Path) -> Option<Accelerator> {
    let vendor = read(device.join("idVendor"))?.to_ascii_lowercase();
    let product = read(device.join("idProduct"))?.to_ascii_lowercase();
    let firmware_loaded = match (vendor.as_str(), product.as_str()) {
        CORAL_USB_BOOTLOADER => false,
        CORAL_USB_RUNTIME => true,
        _ => return None,
    };
    Some(Accelerator {
        kind: "coral_edge_tpu",
        bus: "usb",
        address: device
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        vendor_id: format!("0x{vendor}"),
        device_id: format!("0x{product}"),
        driver: link_name(&device.join("driver")),
        firmware_loaded: Some(firmware_loaded),
        usb_speed_mbps: read(device.join("speed")).and_then(|speed| speed.parse().ok()),
        ..Accelerator::default()
    })
}

/// The `apex_N` class entry whose `device` link resolves to this PCI function.
fn apex_node(sys: &Path, device: &Path) -> Option<String> {
    let target = fs::canonicalize(device).ok()?;
    entries(&sys.join("class/apex"))
        .ok()?
        .into_iter()
        .find(|entry| {
            fs::canonicalize(entry.join("device")).is_ok_and(|resolved| resolved == target)
        })
        .and_then(|entry| {
            entry
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
}

fn link_name(path: &Path) -> Option<String> {
    fs::read_link(path)
        .ok()?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn millidegrees(value: &str) -> Option<f64> {
    // sysfs temperatures are integer millidegrees Celsius.
    value
        .parse::<i32>()
        .ok()
        .map(|milli| f64::from(milli) / 1000.0)
}

#[cfg(test)]
#[path = "accelerator/tests.rs"]
mod tests;
