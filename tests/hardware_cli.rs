use std::{fs, process::Command};

#[test]
fn saved_firmware_metadata_reaches_json_without_privileged_collection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("firmware.txt");
    for second_size in ["32 GB", "Unknown"] {
        fs::write(&path, format!(
            "Handle 1\nMemory Device\n Size: 32 GB\n Locator: DIMM_A2\n Type: DDR5\n Manufacturer: Kingston\n Speed: 6000 MT/s\n Configured Memory Speed: 5600 MT/s\n Serial Number: do-not-report-this\n\nHandle 2\nMemory Device\n Size: {second_size}\n Locator: DIMM_B2\n"
        )).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mdma"))
            .arg("info")
            .arg("--json")
            .arg("--dmi-file")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["memory"]["modules"].as_array().unwrap().len(), 2);
        assert_eq!(json["memory"]["modules"][0]["memory_type"], "DDR5");
        assert_eq!(
            json["memory"]["modules"][0]["configured_speed"]["value"],
            5600
        );
        assert_eq!(
            json["memory"]["modules"][0]["configured_speed"]["unit"],
            "MT/s"
        );
        if second_size == "Unknown" {
            assert!(json["memory"]["installed_bytes"].is_null());
        } else {
            assert_eq!(json["memory"]["installed_bytes"], 64 * 1024_u64.pow(3));
        }
        assert!(
            json["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| !v.as_str().unwrap().contains("dmidecode"))
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("do-not-report-this"));
    }
}

#[test]
fn help_and_unknown_options_do_not_need_hardware_access() {
    let root_help = Command::new(env!("CARGO_BIN_EXE_mdma"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(root_help.status.success());
    assert!(String::from_utf8_lossy(&root_help.stdout).contains("info"));
    let help = Command::new(env!("CARGO_BIN_EXE_mdma"))
        .arg("info")
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--dmi-file"));
    for args in [
        ["info", "--unknown"].as_slice(),
        ["unknown"].as_slice(),
        ["--json"].as_slice(),
    ] {
        let invalid = Command::new(env!("CARGO_BIN_EXE_mdma"))
            .args(args)
            .output()
            .unwrap();
        assert!(!invalid.status.success());
    }
}

#[test]
fn missing_explicit_firmware_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mdma"))
        .arg("info")
        .arg("--dmi-file")
        .arg(dir.path().join("missing.txt"))
        .output()
        .unwrap();
    assert!(!output.status.success());
}
