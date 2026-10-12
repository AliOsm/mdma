use super::{
    common::{command, cpu_vendor, sysctls},
    dmi,
    report::{Cpu, Memory, Report},
};
use std::{collections::BTreeMap, io, path::Path};

#[path = "freebsd/pci.rs"]
mod pci;

pub(super) fn collect(dmi_file: Option<&Path>) -> io::Result<Report> {
    let mut warnings = Vec::new();
    let values = sysctls(&[
        "hw.model",
        "hw.ncpu",
        "hw.physmem",
        "hw.realmem",
        "hw.clockrate",
        "kern.smp.cores",
        "kern.smp.packages",
        "dev.cpu.0.freq",
        "dev.cpu.0.freq_levels",
        "vm.stats.vm.v_page_size",
        "vm.stats.vm.v_free_count",
        "vm.stats.vm.v_inactive_count",
        "vm.stats.vm.v_cache_count",
    ]);
    let cpu = cpu(&values);
    let mut memory = memory(&values);
    let dmi_text = dmi::load(dmi_file, &mut warnings)?;
    dmi::apply(&dmi_text, &mut memory, &mut warnings);
    let text = match command("pciconf", &["-lvc"]) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!(
                "PCIe link metadata unavailable: {error}; trying unprivileged PCI inventory."
            ));
            match command("pciconf", &["-lv"]) {
                Ok(text) => text,
                Err(error) => {
                    warnings.push(error);
                    String::new()
                }
            }
        }
    };
    let mut gpus = pci::parse(&text);
    let slots = dmi::slots(&dmi_text);
    for gpu in &mut gpus {
        gpu.physical_slot = gpu
            .pci_address
            .as_ref()
            .and_then(|address| slots.get(address))
            .cloned();
    }
    let mut accelerators = pci::coral(&text);
    for accelerator in &mut accelerators {
        let unit = accelerator
            .device_node
            .as_deref()
            .and_then(|node| node.strip_prefix("/dev/apex"));
        match unit {
            Some(unit) => {
                let name = format!("dev.apex.{unit}.temperature");
                accelerator.temperature_c = sysctls(&[&name])
                    .get(&name)
                    .and_then(|value| celsius(value));
            }
            None => warnings.push(format!(
                "Coral PCIe device {} has no apex driver attached; load the apex kernel module.",
                accelerator.address.as_deref().unwrap_or("unknown")
            )),
        }
    }
    if cpu.model.is_none() {
        warnings.push("CPU model unavailable from sysctl.".into());
    }
    if memory.total_bytes.is_none() {
        warnings.push("RAM total unavailable from hw.physmem.".into());
    }
    if !gpus.is_empty() {
        warnings.push("FreeBSD pciconf does not expose VRAM capacity, GPU clocks or display connectors; those fields remain unavailable.".into());
    }
    Ok(Report {
        schema_version: 1,
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        cpu,
        memory,
        gpus,
        accelerators,
        warnings,
    })
}

/// The apex driver reports temperature as a sysctl string such as `69.5C`.
fn celsius(value: &str) -> Option<f64> {
    value
        .trim()
        .strip_suffix('C')?
        .parse::<f64>()
        .ok()
        .filter(|c| c.is_finite())
}

fn number(values: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    values.get(key)?.parse().ok()
}

fn cpu(values: &BTreeMap<String, String>) -> Cpu {
    let model = values.get("hw.model").map(|v| v.trim().to_string());
    let frequency = |key| number(values, key).filter(|v| *v > 0).map(|v| v as f64);
    let levels: Vec<f64> = values
        .get("dev.cpu.0.freq_levels")
        .into_iter()
        .flat_map(|v| v.split_whitespace())
        .filter_map(|v| v.split_once('/')?.0.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    Cpu {
        manufacturer: model.as_deref().and_then(cpu_vendor),
        model,
        sockets: number(values, "kern.smp.packages")
            .and_then(|v| v.try_into().ok())
            .filter(|v| *v > 0),
        physical_cores: number(values, "kern.smp.cores")
            .and_then(|v| v.try_into().ok())
            .filter(|v| *v > 0),
        logical_threads: number(values, "hw.ncpu")
            .and_then(|v| v.try_into().ok())
            .filter(|v| *v > 0),
        current_mhz: frequency("dev.cpu.0.freq"),
        nominal_mhz: frequency("hw.clockrate"),
        min_mhz: levels.iter().copied().reduce(f64::min),
        max_mhz: levels.iter().copied().reduce(f64::max),
        ..Cpu::default()
    }
}

fn memory(values: &BTreeMap<String, String>) -> Memory {
    let total_bytes = number(values, "hw.physmem").filter(|v| *v > 0);
    let available_bytes = number(values, "vm.stats.vm.v_free_count")
        .and_then(|free| free.checked_add(number(values, "vm.stats.vm.v_inactive_count")?))
        .and_then(|pages| {
            pages.checked_add(number(values, "vm.stats.vm.v_cache_count").unwrap_or(0))
        })
        .and_then(|pages| {
            pages.checked_mul(number(values, "vm.stats.vm.v_page_size").filter(|v| *v > 0)?)
        })
        .filter(|bytes| total_bytes.is_none_or(|total| *bytes <= total));
    Memory {
        total_bytes,
        available_bytes,
        available_is_estimate: true,
        installed_bytes: number(values, "hw.realmem").filter(|v| *v > 0),
        ..Memory::default()
    }
}

#[cfg(test)]
#[path = "freebsd/tests.rs"]
mod tests;
