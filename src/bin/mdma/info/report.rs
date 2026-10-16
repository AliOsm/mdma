use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub os: &'static str,
    pub architecture: &'static str,
    pub cpu: Cpu,
    pub memory: Memory,
    pub gpus: Vec<Gpu>,
    pub accelerators: Vec<Accelerator>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Cpu {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub sockets: Option<u32>,
    pub physical_cores: Option<u32>,
    pub logical_threads: Option<u32>,
    pub current_mhz: Option<f64>,
    pub nominal_mhz: Option<f64>,
    pub min_mhz: Option<f64>,
    pub max_mhz: Option<f64>,
    pub caches: Vec<Cache>,
    /// Instruction sets mdma's kernels can use, detected at run time.
    pub instruction_sets: Vec<&'static str>,
    /// The exact kernels' variant this CPU runs.
    pub kernel_level: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Cache {
    pub level: String,
    pub size: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Memory {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub available_is_estimate: bool,
    pub installed_bytes: Option<u64>,
    pub modules: Vec<MemoryModule>,
}

#[derive(Debug, Serialize)]
pub struct MemoryModule {
    pub slot: Option<String>,
    pub bank: Option<String>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
    pub size_bytes: Option<u64>,
    pub memory_type: Option<String>,
    pub form_factor: Option<String>,
    pub rated_speed: Option<MemorySpeed>,
    pub configured_speed: Option<MemorySpeed>,
}

#[derive(Debug, Serialize)]
pub struct MemorySpeed {
    pub value: u64,
    pub unit: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Gpu {
    pub pci_address: Option<String>,
    pub bus: Option<String>,
    pub memory_kind: Option<String>,
    pub vendor_id: Option<String>,
    pub device_id: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub board_manufacturer: Option<String>,
    pub driver: Option<String>,
    pub vram_bytes: Option<u64>,
    pub vram_manufacturer: Option<String>,
    pub core_clock: Clock,
    pub memory_clock: Clock,
    pub physical_slot: Option<String>,
    pub pcie_path: Vec<String>,
    pub current_link: PcieLink,
    pub max_link: PcieLink,
    pub display_ports: Vec<DisplayPort>,
}

/// A detected non-GPU accelerator. Detection only: mdma has no backend for it yet.
#[derive(Debug, Default, Serialize)]
pub struct Accelerator {
    pub kind: &'static str,
    pub bus: &'static str,
    /// PCI address, or the USB sysfs port such as `2-1`.
    pub address: Option<String>,
    pub vendor_id: String,
    pub device_id: String,
    pub driver: Option<String>,
    pub device_node: Option<String>,
    pub status: Option<String>,
    pub temperature_c: Option<f64>,
    /// USB Accelerator only: whether its runtime firmware has been uploaded.
    pub firmware_loaded: Option<bool>,
    pub usb_speed_mbps: Option<u32>,
    pub current_link: PcieLink,
    pub max_link: PcieLink,
}

#[derive(Debug, Default, Serialize)]
pub struct Clock {
    pub current_mhz: Option<f64>,
    pub max_reported_mhz: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct PcieLink {
    pub speed_gt_s: Option<f64>,
    pub width: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct DisplayPort {
    pub name: String,
    pub status: Option<String>,
}
