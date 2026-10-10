use super::{
    common::{capacity, installed, known, memory_speed},
    report::{Memory, MemoryModule},
};
use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use {
    super::common::command,
    std::{io, path::Path},
};

fn records<'a>(text: &'a str, title: &str) -> Vec<BTreeMap<&'a str, &'a str>> {
    let mut output = Vec::new();
    let mut current = None;
    for line in text.lines() {
        if line.starts_with("Handle ") {
            if let Some(record) = current.take() {
                output.push(record);
            }
        } else if line.trim() == title {
            current = Some(BTreeMap::new());
        } else if let Some(record) = current.as_mut()
            && let Some((key, value)) = line.trim().split_once(':')
        {
            record.insert(key.trim(), value.trim());
        }
    }
    if let Some(record) = current {
        output.push(record);
    }
    output
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(super) fn load(file: Option<&Path>, warnings: &mut Vec<String>) -> io::Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(path);
    }
    Ok(match command("dmidecode", &["--type", "17,9"]) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!("RAM module/slot metadata unavailable: {error}. SMBIOS access often requires root; use --dmi-file with saved dmidecode --type 17,9 output."));
            String::new()
        }
    })
}

pub(super) fn apply(text: &str, memory: &mut Memory, warnings: &mut Vec<String>) {
    memory.modules = memory_modules(text);
    if !memory.modules.is_empty() {
        memory.installed_bytes = installed(memory);
    } else if !text.is_empty() {
        warnings.push(
            "No populated RAM modules found in the SMBIOS report; firmware may not expose them."
                .into(),
        );
    }
}

pub(super) fn memory_modules(text: &str) -> Vec<MemoryModule> {
    records(text, "Memory Device")
        .into_iter()
        .filter_map(|record| {
            let raw_size = record.get("Size").copied().unwrap_or("Unknown");
            if matches!(raw_size, "No Module Installed" | "Not Installed")
                || capacity(raw_size) == Some(0)
            {
                return None;
            }
            let value = |key| record.get(key).and_then(|s| known(s));
            Some(MemoryModule {
                slot: value("Locator"),
                bank: value("Bank Locator"),
                manufacturer: value("Manufacturer"),
                part_number: value("Part Number"),
                size_bytes: capacity(raw_size),
                memory_type: value("Type"),
                form_factor: value("Form Factor"),
                rated_speed: record.get("Speed").and_then(|s| memory_speed(s)),
                configured_speed: record
                    .get("Configured Memory Speed")
                    .or_else(|| record.get("Configured Clock Speed"))
                    .and_then(|s| memory_speed(s)),
            })
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(super) fn slots(text: &str) -> BTreeMap<String, String> {
    records(text, "System Slot Information")
        .into_iter()
        .filter_map(|record| {
            let address = known(record.get("Bus Address")?)?;
            let designation = known(record.get("Designation")?)?;
            Some((address.to_ascii_lowercase(), designation))
        })
        .collect()
}

#[cfg(test)]
#[path = "dmi/tests.rs"]
mod tests;
