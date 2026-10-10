use super::*;
use crate::info::report::{Accelerator, Cpu, Memory};

#[test]
fn unknown_values_and_machine_units_remain_explicit() {
    let report = Report {
        schema_version: 1,
        os: "linux",
        architecture: "x86_64",
        cpu: Cpu::default(),
        memory: Memory::default(),
        gpus: Vec::new(),
        accelerators: Vec::new(),
        warnings: vec!["SMBIOS permission denied".into()],
    };
    let text = text(&report);
    assert!(text.contains("Manufacturer: unavailable"));
    assert!(text.contains("OS reported: unavailable"));
    assert!(text.contains("SMBIOS permission denied"));
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["memory"]["total_bytes"].is_null());
    assert!(json["cpu"]["max_mhz"].is_null());
    assert_eq!(json["schema_version"], 1);
}

#[test]
fn pcie_generations_and_byte_units_are_not_confused() {
    assert_eq!(
        link(&PcieLink {
            speed_gt_s: Some(32.0),
            width: Some(16)
        }),
        "PCIe 5.0 x16 (32.0 GT/s)"
    );
    assert_eq!(
        link(&PcieLink {
            speed_gt_s: Some(8.0),
            width: Some(8)
        }),
        "PCIe 3.0 x8 (8.0 GT/s)"
    );
    assert_eq!(bytes(Some(32 * 1024_u64.pow(3))), "32.00 GiB");
    assert!(link(&PcieLink::default()).contains("unavailable"));
}

#[test]
fn coral_accelerator_renders_as_detected_without_backend() {
    let report = Report {
        schema_version: 1,
        os: "linux",
        architecture: "x86_64",
        cpu: Cpu::default(),
        memory: Memory::default(),
        gpus: Vec::new(),
        accelerators: vec![Accelerator {
            kind: "coral_edge_tpu",
            bus: "pci",
            address: Some("0000:01:00.0".into()),
            vendor_id: "0x1ac1".into(),
            device_id: "0x089a".into(),
            driver: Some("apex".into()),
            device_node: Some("/dev/apex_0".into()),
            status: Some("ALIVE".into()),
            temperature_c: Some(64.8),
            current_link: PcieLink { speed_gt_s: Some(5.0), width: Some(1) },
            ..Accelerator::default()
        }],
        warnings: Vec::new(),
    };
    let text = text(&report);
    assert!(text.contains("Kind: Coral Edge TPU (pci) — detected, no mdma backend yet"), "{text}");
    assert!(text.contains("Device node: /dev/apex_0 / status: ALIVE / temperature: 64.8 °C"), "{text}");
    assert!(text.contains("PCIe link now: PCIe 2.0 x1 (5.0 GT/s)"), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["accelerators"][0]["kind"], "coral_edge_tpu");
    assert!(json["accelerators"][0]["firmware_loaded"].is_null());
}
