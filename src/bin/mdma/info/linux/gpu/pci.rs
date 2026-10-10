use super::super::super::report::PcieLink;
use super::super::read;
use std::{collections::BTreeMap, fs, path::Path};

fn address(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7 | 10) || b.is_ascii_hexdigit())
}

pub(super) fn path(device: &Path) -> Vec<String> {
    device
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .filter(|s| address(s))
        .map(str::to_string)
        .collect()
}

pub(super) fn slot(
    sys: &Path,
    device: &Path,
    dmi_slots: &BTreeMap<String, String>,
) -> Option<String> {
    for ancestor in device.ancestors() {
        if let Some(name) = ancestor.file_name().and_then(|s| s.to_str())
            && let Some(designation) = dmi_slots.get(name)
        {
            return Some(designation.clone());
        }
    }
    for ancestor in device.ancestors() {
        if let Some(slot) = read(ancestor.join("physical_slot")) {
            return Some(slot);
        }
    }
    let ancestors = path(device);
    let entries = fs::read_dir(sys.join("bus/pci/slots")).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let Some(slot_address) = read(entry.path().join("address")) else {
            continue;
        };
        if ancestors.iter().any(|a| {
            a.rsplit_once('.')
                .is_some_and(|(prefix, _)| prefix == slot_address)
        }) {
            return Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

pub(in super::super) fn link(device: &Path, prefix: &str) -> PcieLink {
    let speed_gt_s = read(device.join(format!("{prefix}_link_speed"))).and_then(|s| {
        let mut words = s.split_whitespace();
        let value = words.next()?.parse::<f64>().ok()?;
        (words.next()? == "GT/s" && value.is_finite() && value > 0.0).then_some(value)
    });
    let width = read(device.join(format!("{prefix}_link_width")))
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| (1..=64).contains(v));
    PcieLink { speed_gt_s, width }
}
