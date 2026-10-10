use super::{
    common::{command, cpu_vendor, sysctls},
    dmi,
    report::{Cache, Cpu, Memory, Report},
};
use std::{collections::BTreeMap, io, path::Path};

#[path = "macos/profiler.rs"]
mod profiler;

pub(super) fn collect(dmi_file: Option<&Path>) -> io::Result<Report> {
    let mut warnings = Vec::new();
    let profile: serde_json::Value = match command(
        "system_profiler",
        &[
            "-json",
            "-detailLevel",
            "basic",
            "SPHardwareDataType",
            "SPMemoryDataType",
            "SPDisplaysDataType",
        ],
    ) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!("system_profiler JSON unavailable: {error}."));
                serde_json::Value::Null
            }
        },
        Err(error) => {
            warnings.push(error);
            serde_json::Value::Null
        }
    };
    let values = sysctls(&[
        "machdep.cpu.brand_string",
        "machdep.cpu.vendor",
        "hw.physicalcpu",
        "hw.logicalcpu",
        "hw.packages",
        "hw.memsize",
        "hw.cpufrequency",
        "hw.cpufrequency_min",
        "hw.cpufrequency_max",
        "hw.l1dcachesize",
        "hw.l1icachesize",
        "hw.l2cachesize",
        "hw.l3cachesize",
    ]);
    let cpu = cpu(&values, &profile);
    let mut memory = Memory {
        total_bytes: number(&values, "hw.memsize").filter(|v| *v > 0),
        installed_bytes: number(&values, "hw.memsize").filter(|v| *v > 0),
        available_is_estimate: true,
        modules: profiler::memory(&profile),
        ..Memory::default()
    };
    match command("vm_stat", &[]) {
        Ok(text) => {
            memory.available_bytes =
                available(&text).filter(|v| memory.total_bytes.is_none_or(|total| *v <= total));
        }
        Err(error) => warnings.push(format!("{error}; available RAM estimate unavailable.")),
    }
    if let Some(path) = dmi_file {
        let text = std::fs::read_to_string(path)?;
        dmi::apply(&text, &mut memory, &mut warnings);
    }
    let gpus = profiler::gpus(&profile);
    if cpu.model.is_none() {
        warnings.push("CPU model unavailable from sysctl/system_profiler.".into());
    }
    if memory.total_bytes.is_none() {
        warnings.push("RAM total unavailable from hw.memsize.".into());
    }
    if memory.modules.is_empty() {
        warnings
            .push("RAM type/manufacturer/speed not exposed by system_profiler on this Mac.".into());
    }
    if gpus.is_empty() {
        warnings.push("No GPU entries returned by system_profiler.".into());
    }
    warnings.push("macOS inventory does not expose current CPU/GPU clocks or a complete PCIe path; unavailable fields are not guessed.".into());
    Ok(Report {
        schema_version: 1,
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        cpu,
        memory,
        gpus,
        accelerators: Vec::new(),
        warnings,
    })
}

fn number(values: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    values.get(key)?.parse().ok()
}

fn cpu(values: &BTreeMap<String, String>, profile: &serde_json::Value) -> Cpu {
    let hardware = profile
        .get("SPHardwareDataType")
        .and_then(|v| v.as_array())
        .and_then(|v| v.first());
    let model = values.get("machdep.cpu.brand_string").cloned().or_else(|| {
        let hardware = hardware?;
        hardware
            .get("chip_type")
            .or_else(|| hardware.get("cpu_type"))?
            .as_str()
            .map(str::to_string)
    });
    let manufacturer = model
        .as_deref()
        .and_then(cpu_vendor)
        .or_else(|| values.get("machdep.cpu.vendor").cloned());
    let count = |key| {
        number(values, key)
            .and_then(|v| v.try_into().ok())
            .filter(|v| *v > 0)
    };
    let frequency = |key| {
        number(values, key)
            .filter(|v| *v > 0)
            .map(|v| v as f64 / 1_000_000.0)
    };
    let caches = [
        ("L1d", "hw.l1dcachesize"),
        ("L1i", "hw.l1icachesize"),
        ("L2", "hw.l2cachesize"),
        ("L3", "hw.l3cachesize"),
    ]
    .into_iter()
    .filter_map(|(level, key)| {
        let size = number(values, key).filter(|v| *v > 0)?;
        Some(Cache {
            level: level.into(),
            size: format!("{:.0} KiB (sysctl-reported)", size as f64 / 1024.0),
        })
    })
    .collect();
    Cpu {
        model,
        manufacturer,
        sockets: count("hw.packages"),
        physical_cores: count("hw.physicalcpu"),
        logical_threads: count("hw.logicalcpu"),
        nominal_mhz: frequency("hw.cpufrequency"),
        min_mhz: frequency("hw.cpufrequency_min"),
        max_mhz: frequency("hw.cpufrequency_max"),
        caches,
        ..Cpu::default()
    }
}

fn available(text: &str) -> Option<u64> {
    let header = text.lines().next()?;
    let page_size = header
        .split_once("page size of ")?
        .1
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0)?;
    let values = super::common::fields(text);
    let pages = |key| values.get(key)?.trim_end_matches('.').parse::<u64>().ok();
    pages("Pages free")?
        .checked_add(pages("Pages inactive")?)?
        .checked_mul(page_size)
}

#[cfg(test)]
#[path = "macos/tests.rs"]
mod tests;
