use super::super::report::{Cache, Cpu};
use super::{command, fields, read};
use std::{collections::BTreeMap, fs, path::Path};

pub(super) fn collect(warnings: &mut Vec<String>) -> Cpu {
    let proc = read("/proc/cpuinfo").unwrap_or_default();
    let mut cpu = fallback(&proc);
    match command("lscpu", &["--json"]) {
        Ok(text) => match parse(&text) {
            Ok(decoded) => {
                cpu.manufacturer = decoded.manufacturer.or(cpu.manufacturer);
                cpu.model = decoded.model.or(cpu.model);
                cpu.sockets = decoded.sockets.or(cpu.sockets);
                cpu.physical_cores = decoded.physical_cores.or(cpu.physical_cores);
                cpu.logical_threads = decoded.logical_threads.or(cpu.logical_threads);
                cpu.min_mhz = decoded.min_mhz;
                cpu.max_mhz = decoded.max_mhz;
                cpu.caches = decoded.caches;
            }
            Err(error) => warnings.push(format!(
                "lscpu output unavailable: {error}; using /proc/cpuinfo."
            )),
        },
        Err(error) => warnings.push(format!("{error}; using /proc/cpuinfo for CPU details.")),
    }
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        let mut cores = std::collections::BTreeSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name
                .to_string_lossy()
                .strip_prefix("cpu")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let path = entry.path().join("topology");
            if let Some(pair) = core_id(&path) {
                cores.insert(pair);
            }
        }
        if !cores.is_empty() {
            cpu.physical_cores = cores.len().try_into().ok();
            cpu.sockets = cores
                .iter()
                .map(|(socket, _)| socket)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                .try_into()
                .ok();
        }
    }
    if cpu.model.is_none() {
        warnings.push("CPU model unavailable.".into());
    }
    cpu
}

fn core_id(path: &Path) -> Option<(u32, u32)> {
    Some((
        read(path.join("physical_package_id"))?.parse().ok()?,
        read(path.join("core_id"))?.parse().ok()?,
    ))
}

fn manufacturer(vendor: &str) -> String {
    match vendor {
        "GenuineIntel" => "Intel".into(),
        "AuthenticAMD" => "AMD".into(),
        _ => vendor.into(),
    }
}

fn fallback(text: &str) -> Cpu {
    let records: Vec<_> = text
        .split("\n\n")
        .map(fields)
        .filter(|r| r.contains_key("processor"))
        .collect();
    let first = records.first();
    let model = first
        .and_then(|r| r.get("model name").or_else(|| r.get("Processor")))
        .map(ToString::to_string);
    let manufacturer = first
        .and_then(|r| r.get("vendor_id"))
        .map(|v| manufacturer(v));
    let frequencies: Vec<_> = records
        .iter()
        .filter_map(|r| r.get("cpu MHz")?.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    let current_mhz = (!frequencies.is_empty())
        .then(|| frequencies.iter().sum::<f64>() / frequencies.len() as f64);
    Cpu {
        model,
        manufacturer,
        current_mhz,
        logical_threads: (!records.is_empty())
            .then(|| records.len().try_into().ok())
            .flatten(),
        ..Cpu::default()
    }
}

fn parse(text: &str) -> Result<Cpu, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let values: BTreeMap<_, _> = value
        .get("lscpu")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("field")?.as_str()?.trim_end_matches(':'),
                entry.get("data")?.as_str()?,
            ))
        })
        .collect();
    let number = |key| {
        values
            .get(key)
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
    };
    let frequency = |key| {
        values
            .get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    let sockets = number("Socket(s)");
    let physical_cores = number("Core(s) per socket").and_then(|cores| cores.checked_mul(sockets?));
    let caches = ["L1d cache", "L1i cache", "L2 cache", "L3 cache"]
        .into_iter()
        .filter_map(|key| {
            Some(Cache {
                level: key.trim_end_matches(" cache").into(),
                size: values.get(key)?.to_string(),
            })
        })
        .collect();
    Ok(Cpu {
        manufacturer: values.get("Vendor ID").map(|v| manufacturer(v)),
        model: values.get("Model name").map(ToString::to_string),
        sockets,
        physical_cores,
        logical_threads: number("CPU(s)"),
        min_mhz: frequency("CPU min MHz"),
        max_mhz: frequency("CPU max MHz"),
        caches,
        ..Cpu::default()
    })
}

#[cfg(test)]
#[path = "cpu/tests.rs"]
mod tests;
