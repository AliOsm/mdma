use super::super::report::{Clock, DisplayPort, Gpu};
use super::{command, fields, read};
use std::{collections::BTreeMap, fs, path::Path};

#[path = "gpu/pci.rs"]
pub(super) mod pci;

type Names = BTreeMap<String, BTreeMap<String, String>>;

pub(super) fn collect(
    sys: &Path,
    slots: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Vec<Gpu> {
    let names = match command("lspci", &["-D", "-vmm"]) {
        Ok(text) => pci_names(&text),
        Err(error) => {
            warnings.push(format!("{error}; GPU names may only show PCI IDs."));
            Names::new()
        }
    };
    let mut gpus = match scan(sys, &names, slots) {
        Ok(gpus) => gpus,
        Err(error) => {
            warnings.push(format!("GPU PCI inventory unavailable: {error}."));
            Vec::new()
        }
    };
    for gpu in &mut gpus {
        if gpu.vendor_id.as_deref() == Some("0x10de") {
            let Some(address) = gpu.pci_address.as_deref() else {
                continue;
            };
            let id = format!("--id={address}");
            match command(
                "nvidia-smi",
                &[
                    &id,
                    "--query-gpu=memory.total,clocks.current.graphics,clocks.max.graphics,clocks.current.memory,clocks.max.memory",
                    "--format=csv,noheader,nounits",
                ],
            ) {
                Ok(text) => nvidia_details(gpu, &text),
                Err(error) => {
                    warnings.push(format!("{error}; NVIDIA VRAM/clocks may be unavailable."));
                }
            }
        }
    }
    gpus
}

fn pci_names(text: &str) -> Names {
    text.split("\n\n")
        .filter_map(|record| {
            let values = fields(record);
            let address = values.get("Slot")?.to_ascii_lowercase();
            Some((
                address,
                values
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ))
        })
        .collect()
}

fn scan(sys: &Path, names: &Names, slots: &BTreeMap<String, String>) -> std::io::Result<Vec<Gpu>> {
    let mut devices: Vec<_> = fs::read_dir(sys.join("bus/pci/devices"))?
        .filter_map(Result::ok)
        .collect();
    devices.sort_by_key(std::fs::DirEntry::file_name);
    let mut output = Vec::new();
    for entry in devices {
        let device = entry.path();
        let class = read(device.join("class"))
            .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        if class.is_none_or(|c| c >> 16 != 0x03) {
            continue;
        }
        let address = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let metadata = names.get(&address);
        let name = |key| metadata.and_then(|m| m.get(key)).cloned();
        let vendor_id = read(device.join("vendor"));
        let device_id = read(device.join("device"));
        let path = fs::canonicalize(&device).unwrap_or_else(|_| device.clone());
        let pcie_path = pci::path(&path);
        let card = drm_card(sys, &address);
        let mut core_clock = dpm_clock(&read(device.join("pp_dpm_sclk")).unwrap_or_default());
        if let Some(card) = &card {
            core_clock.current_mhz = core_clock
                .current_mhz
                .or_else(|| decimal(card.join("gt_cur_freq_mhz")));
            core_clock.max_reported_mhz = core_clock
                .max_reported_mhz
                .or_else(|| decimal(card.join("gt_max_freq_mhz")).filter(|v| *v > 0.0));
        }
        output.push(Gpu {
            manufacturer: name("Vendor")
                .or_else(|| vendor_id.as_deref().and_then(vendor).map(str::to_string)),
            model: name("Device"),
            board_manufacturer: name("SVendor"),
            driver: fs::read_link(device.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned())),
            vram_bytes: read(device.join("mem_info_vram_total"))
                .and_then(|s| s.parse().ok())
                .filter(|v| *v > 0),
            vram_manufacturer: read(device.join("mem_info_vram_vendor")),
            memory_clock: dpm_clock(&read(device.join("pp_dpm_mclk")).unwrap_or_default()),
            physical_slot: pci::slot(sys, &path, slots),
            current_link: pci::link(&device, "current"),
            max_link: pci::link(&device, "max"),
            display_ports: card
                .as_ref()
                .map(|card| display_ports(sys, card))
                .unwrap_or_default(),
            pci_address: Some(address),
            bus: Some("pci".into()),
            vendor_id,
            device_id,
            core_clock,
            pcie_path,
            ..Gpu::default()
        });
    }
    Ok(output)
}

fn decimal(path: impl AsRef<Path>) -> Option<f64> {
    read(path)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn vendor(id: &str) -> Option<&'static str> {
    match id {
        "0x1002" => Some("AMD"),
        "0x10de" => Some("NVIDIA"),
        "0x8086" => Some("Intel"),
        _ => None,
    }
}

fn drm_card(sys: &Path, address: &str) -> Option<std::path::PathBuf> {
    fs::read_dir(sys.join("class/drm"))
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let number = name.strip_prefix("card")?;
            if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let path = fs::canonicalize(entry.path().join("device")).ok()?;
            (path.file_name()?.to_str()? == address).then(|| entry.path())
        })
}

fn display_ports(sys: &Path, card: &Path) -> Vec<DisplayPort> {
    let prefix = format!(
        "{}-",
        card.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut output: Vec<_> = fs::read_dir(sys.join("class/drm"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?.strip_prefix(&prefix)?.to_string();
            if name.starts_with("Writeback-") {
                return None;
            }
            Some(DisplayPort {
                name,
                status: read(entry.path().join("status")),
            })
        })
        .collect();
    output.sort_by(|a, b| a.name.cmp(&b.name));
    output
}

fn dpm_clock(text: &str) -> Clock {
    let mut clock = Clock::default();
    for line in text.lines() {
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(word) = rest.split_whitespace().next() else {
            continue;
        };
        let Some(value) = word
            .to_ascii_lowercase()
            .strip_suffix("mhz")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue;
        };
        if value > 0.0 {
            clock.max_reported_mhz = Some(clock.max_reported_mhz.unwrap_or(0.0).max(value));
        }
        if rest.contains('*') {
            clock.current_mhz = Some(value);
        }
    }
    clock
}

fn nvidia_details(gpu: &mut Gpu, text: &str) {
    let values: Vec<_> = text.trim().split(',').map(str::trim).collect();
    let number = |index: usize| {
        values
            .get(index)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    gpu.vram_bytes = values
        .first()
        .and_then(|s| s.parse::<u64>().ok())
        .and_then(|v| v.checked_mul(1024 * 1024))
        .or(gpu.vram_bytes);
    gpu.core_clock = Clock {
        current_mhz: number(1),
        max_reported_mhz: number(2).filter(|v| *v > 0.0),
    };
    gpu.memory_clock = Clock {
        current_mhz: number(3),
        max_reported_mhz: number(4).filter(|v| *v > 0.0),
    };
}

#[cfg(test)]
#[path = "gpu/tests.rs"]
mod tests;
