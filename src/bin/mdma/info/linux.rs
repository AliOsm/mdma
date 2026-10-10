use super::report::{Memory, Report};
use super::{
    common::{command, fields, read},
    dmi,
};
use std::{io, path::Path};

#[path = "linux/accelerator.rs"]
mod accelerator;
#[path = "linux/cpu.rs"]
mod cpu;
#[path = "linux/gpu.rs"]
mod gpu;

pub(super) fn collect(dmi_file: Option<&Path>) -> io::Result<Report> {
    let mut warnings = Vec::new();
    let dmi_text = dmi::load(dmi_file, &mut warnings)?;
    let mut memory = memory_totals(&read("/proc/meminfo").unwrap_or_default());
    dmi::apply(&dmi_text, &mut memory, &mut warnings);
    if memory.total_bytes.is_none() {
        warnings.push("OS RAM totals unavailable from /proc/meminfo.".into());
    }
    let cpu = cpu::collect(&mut warnings);
    let gpus = gpu::collect(Path::new("/sys"), &dmi::slots(&dmi_text), &mut warnings);
    let accelerators = accelerator::collect(Path::new("/sys"), &mut warnings);
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

fn memory_totals(text: &str) -> Memory {
    let values = fields(text);
    let bytes = |key| {
        let mut words = values.get(key)?.split_whitespace();
        let value = words.next()?.parse::<u64>().ok()?;
        (words.next()? == "kB")
            .then(|| value.checked_mul(1024))
            .flatten()
    };
    Memory {
        total_bytes: bytes("MemTotal"),
        available_bytes: bytes("MemAvailable"),
        ..Memory::default()
    }
}

#[cfg(test)]
#[path = "linux/tests.rs"]
mod tests;
