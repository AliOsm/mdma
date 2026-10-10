use super::super::{
    common::{capacity, known, memory_speed},
    report::{DisplayPort, Gpu, MemoryModule},
};
use serde_json::Value;

fn value(data: &Value, key: &str) -> Option<String> {
    known(data.get(key)?.as_str()?)
}

pub(super) fn memory(profile: &Value) -> Vec<MemoryModule> {
    fn walk(data: &Value, modules: &mut Vec<MemoryModule>) {
        if let Some(items) = data.get("_items").and_then(|v| v.as_array()) {
            for item in items {
                walk(item, modules);
            }
            return;
        }
        let size = value(data, "dimm_size").or_else(|| value(data, "SPMemoryDataType"));
        if matches!(
            size.as_deref(),
            None | Some("Empty" | "No Module Installed")
        ) {
            return;
        }
        let unified = data.get("SPMemoryDataType").is_some();
        modules.push(MemoryModule {
            slot: if unified {
                Some("Unified memory".into())
            } else {
                value(data, "_name")
            },
            bank: None,
            manufacturer: value(data, "dimm_manufacturer"),
            part_number: value(data, "dimm_part_number"),
            size_bytes: size.as_deref().and_then(capacity),
            memory_type: value(data, "dimm_type"),
            form_factor: None,
            rated_speed: None,
            configured_speed: value(data, "dimm_speed").as_deref().and_then(memory_speed),
        });
    }
    let mut modules = Vec::new();
    if let Some(items) = profile.get("SPMemoryDataType").and_then(|v| v.as_array()) {
        for item in items {
            walk(item, &mut modules);
        }
    }
    modules
}

fn vendor(data: &Value) -> Option<String> {
    let raw = value(data, "spdisplays_vendor")?;
    let label = raw.to_ascii_lowercase();
    if label.contains("apple") {
        Some("Apple".into())
    } else if label.contains("0x1002") || label.contains("amd") || label.contains("ati") {
        Some("AMD".into())
    } else if label.contains("0x10de") || label.contains("nvidia") || label.contains("nvda") {
        Some("NVIDIA".into())
    } else if label.contains("0x8086") || label.contains("intel") {
        Some("Intel".into())
    } else {
        Some(raw)
    }
}

pub(super) fn gpus(profile: &Value) -> Vec<Gpu> {
    profile
        .get("SPDisplaysDataType")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .map(|data| {
            let manufacturer = vendor(data);
            let model = value(data, "sppci_model").or_else(|| value(data, "_name"));
            let bus = value(data, "sppci_bus")
                .or_else(|| value(data, "spdisplays_bus"))
                .map(|v| match v.as_str() {
                    "spdisplays_builtin" | "sppci_builtin" => "built_in".into(),
                    "spdisplays_pcie" | "sppci_pcie" => "pcie".into(),
                    _ => v,
                });
            let shared = data.get("spdisplays_vram_shared").is_some();
            let unified =
                manufacturer.as_deref() == Some("Apple") && bus.as_deref() == Some("built_in");
            let vram_bytes = if shared || unified {
                None
            } else {
                value(data, "spdisplays_vram").as_deref().and_then(capacity)
            };
            let mut gpu = Gpu {
                manufacturer,
                model,
                bus,
                vram_bytes,
                memory_kind: if unified {
                    Some("unified".into())
                } else if shared {
                    Some("shared".into())
                } else {
                    vram_bytes.map(|_| "dedicated".into())
                },
                vendor_id: value(data, "spdisplays_vendor-id"),
                device_id: value(data, "spdisplays_device-id"),
                physical_slot: value(data, "sppci_slot").or_else(|| value(data, "spdisplays_slot")),
                ..Gpu::default()
            };
            gpu.current_link.width = value(data, "spdisplays_pcie_lane_width")
                .and_then(|v| v.trim_start_matches('x').parse::<u32>().ok())
                .filter(|v| (1..=32).contains(v));
            gpu.display_ports = data
                .get("spdisplays_ndrvs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|display| {
                    let name = value(display, "_name")?;
                    let connection = value(display, "spdisplays_connection_type");
                    let name = connection
                        .map(|c| format!("{name} ({})", c.trim_start_matches("spdisplays_")))
                        .unwrap_or(name);
                    let status = value(display, "spdisplays_online").map(|v| match v.as_str() {
                        "spdisplays_yes" | "Yes" => "connected".into(),
                        "spdisplays_no" | "No" => "disconnected".into(),
                        _ => v,
                    });
                    Some(DisplayPort { name, status })
                })
                .collect();
            gpu
        })
        .collect()
}
