use super::report::{Clock, MemorySpeed, PcieLink, Report};
use std::fmt::Write;

fn known(value: Option<&str>) -> &str {
    value.unwrap_or("unavailable")
}

fn bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".into(), |n| format!("{:.2} GiB", n as f64 / 1024_f64.powi(3)))
}

fn count(value: Option<u32>) -> String {
    value.map_or_else(|| "unavailable".into(), |n| n.to_string())
}

fn mhz(value: Option<f64>) -> String {
    value.map_or_else(|| "unavailable".into(), |n| format!("{n:.0} MHz"))
}

fn speed(value: Option<&MemorySpeed>) -> String {
    value.map_or_else(|| "unavailable".into(), |s| format!("{} {}", s.value, s.unit))
}

fn clock(value: &Clock) -> String {
    format!(
        "{} current / {} max reported",
        mhz(value.current_mhz),
        mhz(value.max_reported_mhz)
    )
}

fn link(value: &PcieLink) -> String {
    let generation = value.speed_gt_s.and_then(|speed| {
        [2.5, 5.0, 8.0, 16.0, 32.0, 64.0, 128.0]
            .iter()
            .position(|v| (speed - v).abs() < 0.01)
    });
    let speed = value
        .speed_gt_s.map_or_else(|| "unavailable".into(), |n| format!("{n:.1} GT/s"));
    let width = value
        .width.map_or_else(|| "width unavailable".into(), |n| format!("x{n}"));
    match generation {
        Some(index) => format!("PCIe {}.0 {width} ({speed})", index + 1),
        None => format!("{speed}, {width}"),
    }
}

pub(super) fn text(report: &Report) -> String {
    let mut out = String::new();
    write_text(&mut out, report).expect("formatting into a String cannot fail");
    out
}

fn write_text(out: &mut String, report: &Report) -> std::fmt::Result {
    writeln!(
        out,
        "Hardware inventory — {} / {}\n",
        report.os, report.architecture
    )?;
    let cpu = &report.cpu;
    writeln!(
        out,
        "CPU\n  Model: {}\n  Manufacturer: {}",
        known(cpu.model.as_deref()),
        known(cpu.manufacturer.as_deref())
    )?;
    writeln!(
        out,
        "  Topology: {} sockets / {} physical cores / {} logical threads",
        count(cpu.sockets),
        count(cpu.physical_cores),
        count(cpu.logical_threads)
    )?;
    writeln!(
        out,
        "  Frequency: {} observed / {} minimum reported / {} maximum reported",
        mhz(cpu.current_mhz),
        mhz(cpu.min_mhz),
        mhz(cpu.max_mhz)
    )?;
    if cpu.nominal_mhz.is_some() {
        writeln!(out, "  Nominal frequency: {}", mhz(cpu.nominal_mhz))?;
    }
    for cache in &cpu.caches {
        writeln!(out, "  {} cache: {}", cache.level, cache.size)?;
    }
    writeln!(
        out,
        "\nRAM\n  Physical capacity: {}\n  OS reported: {}\n  OS available now: {}{}",
        bytes(report.memory.installed_bytes),
        bytes(report.memory.total_bytes),
        bytes(report.memory.available_bytes),
        if report.memory.available_is_estimate {
            " (estimate)"
        } else {
            ""
        }
    )?;
    if report.memory.modules.is_empty() {
        writeln!(out, "  Module type / manufacturer / speed: unavailable")?;
    }
    for module in &report.memory.modules {
        writeln!(
            out,
            "  {} [{}]: {} {} / {}",
            known(module.slot.as_deref()),
            known(module.bank.as_deref()),
            bytes(module.size_bytes),
            known(module.memory_type.as_deref()),
            known(module.form_factor.as_deref())
        )?;
        writeln!(
            out,
            "    Manufacturer: {} / part: {}",
            known(module.manufacturer.as_deref()),
            known(module.part_number.as_deref())
        )?;
        writeln!(
            out,
            "    Speed: {} configured / {} rated",
            speed(module.configured_speed.as_ref()),
            speed(module.rated_speed.as_ref())
        )?;
    }
    if report.gpus.is_empty() {
        writeln!(out, "\nGPU\n  No GPU devices reported.")?;
    }
    for (index, gpu) in report.gpus.iter().enumerate() {
        writeln!(
            out,
            "\nGPU {}\n  Model: {}\n  Manufacturer: {} / board: {}",
            index + 1,
            known(gpu.model.as_deref()),
            known(gpu.manufacturer.as_deref()),
            known(gpu.board_manufacturer.as_deref())
        )?;
        writeln!(out, "  Bus: {}", known(gpu.bus.as_deref()))?;
        writeln!(
            out,
            "  PCI address: {} ({}:{}) / driver: {}",
            known(gpu.pci_address.as_deref()),
            known(gpu.vendor_id.as_deref()),
            known(gpu.device_id.as_deref()),
            known(gpu.driver.as_deref())
        )?;
        if matches!(gpu.memory_kind.as_deref(), Some("unified" | "shared")) {
            writeln!(
                out,
                "  GPU memory: {} system RAM (no dedicated VRAM)",
                known(gpu.memory_kind.as_deref())
            )?;
        } else {
            writeln!(
                out,
                "  VRAM: {} / memory manufacturer: {}",
                bytes(gpu.vram_bytes),
                known(gpu.vram_manufacturer.as_deref())
            )?;
        }
        if gpu.vram_bytes.is_none() && gpu.memory_kind.is_none() {
            writeln!(
                out,
                "    Dedicated VRAM not exposed; integrated GPUs may use shared system RAM."
            )?;
        }
        writeln!(
            out,
            "  Core clock: {}\n  Memory clock: {}",
            clock(&gpu.core_clock),
            clock(&gpu.memory_clock)
        )?;
        writeln!(
            out,
            "  Physical slot: {}\n  PCIe link now: {}\n  PCIe device maximum: {}",
            known(gpu.physical_slot.as_deref()),
            link(&gpu.current_link),
            link(&gpu.max_link)
        )?;
        let path = if gpu.pcie_path.is_empty() {
            "unavailable".into()
        } else {
            gpu.pcie_path.join(" → ")
        };
        writeln!(out, "  PCIe path: {path}")?;
        if gpu.display_ports.is_empty() {
            writeln!(out, "  Display ports: unavailable")?;
        }
        for port in &gpu.display_ports {
            writeln!(out, "  Display port {}: {}", port.name, known(port.status.as_deref()))?;
        }
    }
    for (index, accelerator) in report.accelerators.iter().enumerate() {
        let kind = if accelerator.kind == "coral_edge_tpu" { "Coral Edge TPU" } else { accelerator.kind };
        writeln!(
            out,
            "\nAccelerator {}\n  Kind: {kind} ({}) — detected, no mdma backend yet",
            index + 1,
            accelerator.bus
        )?;
        writeln!(
            out,
            "  Address: {} ({}:{}) / driver: {}",
            known(accelerator.address.as_deref()),
            accelerator.vendor_id,
            accelerator.device_id,
            known(accelerator.driver.as_deref())
        )?;
        if accelerator.bus == "pci" {
            writeln!(
                out,
                "  Device node: {} / status: {} / temperature: {}",
                known(accelerator.device_node.as_deref()),
                known(accelerator.status.as_deref()),
                accelerator.temperature_c.map_or_else(|| "unavailable".into(), |c| format!("{c:.1} °C"))
            )?;
            writeln!(out, "  PCIe link now: {} / device maximum: {}", link(&accelerator.current_link), link(&accelerator.max_link))?;
        } else {
            let firmware = match accelerator.firmware_loaded {
                Some(true) => "loaded",
                Some(false) => "not loaded (bootloader)",
                None => "unavailable",
            };
            let usb = accelerator.usb_speed_mbps.map_or_else(|| "unavailable".into(), |mbps| format!("{mbps} Mb/s"));
            writeln!(out, "  Firmware: {firmware} / USB link: {usb}")?;
        }
    }
    if !report.warnings.is_empty() {
        writeln!(out, "\nNotes")?;
        for warning in &report.warnings {
            writeln!(out, "  - {warning}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "render/tests.rs"]
mod tests;
