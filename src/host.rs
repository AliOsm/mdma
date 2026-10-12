//! The machine a server runs on, as clients and peers see it.
use crate::grpc::Host;
use std::path::Path;
use sysinfo::{Disks, System};

/// Longest host field, in bytes.
pub const MAX_FIELD_BYTES: usize = 128;

/// `name` is the server's node name, `device` its accelerator or backend.
pub fn detect(name: &str, device: &str) -> Host {
    let mut system = System::new();
    system.refresh_memory();
    // A full refresh: FreeBSD fills in the CPU brand only then.
    system.refresh_cpu_all();
    Host {
        name: field(name),
        os: std::env::consts::OS.to_owned(),
        os_name: field(&os_name()),
        arch: field(&System::cpu_arch()),
        cpu: field(system.cpus().first().map_or("", sysinfo::Cpu::brand)),
        memory_bytes: system.total_memory(),
        device: field(device),
        ..Host::default()
    }
}

/// `host` with its figures now: free RAM, the device's memory (free, total)
/// and the disk under `models`.
pub fn live(host: &Host, models: Option<&Path>, device: (Option<usize>, Option<usize>)) -> Host {
    let mut system = System::new();
    system.refresh_memory();
    let (disk_bytes, disk_available_bytes) = models.and_then(disk_of).unwrap_or_default();
    Host {
        memory_available_bytes: system.available_memory(),
        device_available_bytes: device.0.unwrap_or(0) as u64,
        device_memory_bytes: device.1.unwrap_or(0) as u64,
        disk_bytes,
        disk_available_bytes,
        ..host.clone()
    }
}

/// Total and free bytes of the filesystem holding `path`, following symlinks,
/// so a folder linked to another disk reports that disk.
pub fn disk_of(path: &Path) -> Option<(u64, u64)> {
    let path = path.canonicalize().ok()?;
    let disks = Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| (disk.total_space(), disk.available_space()))
}

/// "FreeBSD 16.0-CURRENT", "macOS 15.6", "Ubuntu 24.04".
pub fn os_name() -> String {
    cfg_select! {
        target_os = "freebsd" => { System::long_os_version().map(|banner| freebsd_release(&banner)).unwrap_or_default() }
        _ => {
            // sysinfo names macOS by its kernel.
            let name = if cfg!(target_os = "macos") { Some("macOS".to_owned()) } else { System::name() };
            [name, System::os_version()].into_iter().flatten().collect::<Vec<_>>().join(" ")
        }
    }
}

/// The release with its branch, from the kern.version banner: "FreeBSD 16.0-CURRENT #1 ...".
#[cfg(any(target_os = "freebsd", test))]
fn freebsd_release(banner: &str) -> String {
    banner
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Printable ASCII within `MAX_FIELD_BYTES`.
pub fn is_valid(host: &Host) -> bool {
    [
        &host.name,
        &host.os,
        &host.os_name,
        &host.arch,
        &host.cpu,
        &host.device,
    ]
    .into_iter()
    .all(|f| f.len() <= MAX_FIELD_BYTES && f.bytes().all(|b| b == b' ' || b.is_ascii_graphic()))
}

fn field(text: &str) -> String {
    let mut text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.retain(|c| c == ' ' || c.is_ascii_graphic());
    text.truncate(MAX_FIELD_BYTES);
    text
}

#[cfg(test)]
mod tests;
