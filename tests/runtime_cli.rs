//! Bounded runtime-tool smoke tests of the actual command-line binary.
use serde_json::Value;
use std::process::{Command, Output};

#[path = "runtime_cli/fixture.rs"]
mod fixture;

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mdma"))
        .args(args)
        .env("RAYON_NUM_THREADS", "1")
        .env("MDMA_CACHE", "off")
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn doctor_and_help_expose_backend_readiness() {
    let report = json(&cli(&["doctor", "--backend", "cpu", "--json"]));
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["checks"][0]["status"], "ready");
    assert_eq!(report["checks"][0]["capabilities"]["device"], "CPU");
    assert!(!cli(&["doctor", "--backend", "unknown"]).status.success());
    let output = cli(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["info", "doctor", "plan", "run", "bench"] {
        assert!(help.contains(command));
    }
}

#[test]
fn plan_is_metadata_only_and_reports_nonfits_with_failure_status() {
    let file = fixture::model(8);
    let path = file.path().to_str().unwrap();
    let report = json(&cli(&[
        "plan",
        path,
        "--ctx",
        "16",
        "--prompt-tokens",
        "3",
        "--ram-bytes",
        "1000000",
        "--json",
    ]));
    let g = mdma::gguf::GgufModel::open(file.path()).unwrap();
    let expected =
        mdma::model::planning::estimate(&mdma::backend::cpu::CpuBackend, &g, 16, 3).unwrap();
    assert_eq!(report["fits"], true);
    assert_eq!(report["estimate"]["weights_bytes"], expected.weights_bytes);
    assert_eq!(report["device_required_bytes"], 0);
    let small = json(&cli(&[
        "plan",
        path,
        "--ctx",
        "16",
        "--ram-bytes",
        "1000000",
        "--json",
    ]));
    assert_eq!(small["estimate"]["prompt_tokens"], 16);
    let failed = cli(&[
        "plan",
        path,
        "--ctx",
        "16",
        "--prompt-tokens",
        "3",
        "--ram-bytes",
        "1",
        "--json",
    ]);
    assert!(!failed.status.success());
    let report: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(report["fits"], false);
    for flags in [
        ["--ctx", "0"],
        ["--gpu-layers", "invalid"],
        ["--gpu-layers", "3"],
    ] {
        assert!(
            !cli(&["plan", path, flags[0], flags[1], "--ram-bytes", "1000000"])
                .status
                .success()
        );
    }
}

#[test]
fn cpu_generation_and_zero_offload_keep_compatibility() {
    let file = fixture::model(8);
    let path = file.path().to_str().unwrap();
    let args = [
        "run",
        path,
        "hello",
        "--max-tokens",
        "3",
        "--ram-bytes",
        "1000000",
        "--json",
    ];
    let ordinary = json(&cli(&args));
    let mut hybrid = args.to_vec();
    hybrid.extend(["--gpu-layers", "0"]);
    let hybrid = json(&cli(&hybrid));
    assert_eq!(ordinary["tokens"], hybrid["tokens"]);
    assert_eq!(hybrid["gpu_layers"], 0);
    let compatible = Command::new(env!("CARGO_BIN_EXE_mdma-run"))
        .args([path, "hello", "3"])
        .env("MDMA_BACKEND", "cpu")
        .env("MDMA_JSON", "1")
        .env_remove("MDMA_GPU_LAYERS")
        .env("MDMA_CACHE", "off")
        .env("RAYON_NUM_THREADS", "1")
        .output()
        .unwrap();
    assert_eq!(json(&compatible)["tokens"], ordinary["tokens"]);
    assert!(
        !cli(&["run", path, "hello", "--max-tokens", "0"])
            .status
            .success()
    );
    assert!(
        !cli(&["run", path, "hello", "--ram-bytes", "1"])
            .status
            .success()
    );
}

#[test]
fn memory_bench_is_bounded_and_checks_budgets() {
    let report = json(&cli(&[
        "bench",
        "--size-mib",
        "1",
        "--iterations",
        "2",
        "--ram-bytes",
        "10000000",
        "--json",
    ]));
    assert_eq!(report["measurements"][0]["operation"], "ram_copy");
    assert_eq!(report["measurements"][0]["bytes_per_copy"], 1024 * 1024);
    assert!(
        report["measurements"][0]["payload_gb_per_second"]
            .as_f64()
            .unwrap()
            > 0.
    );
    for flags in [
        ["--size-mib", "257"],
        ["--iterations", "33"],
        ["--ram-bytes", "1"],
    ] {
        assert!(!cli(&["bench", flags[0], flags[1]]).status.success());
    }
}

#[cfg(feature = "vulkan")]
#[test]
#[ignore = "requires a Vulkan GPU; tiny generated model and 1 MiB copy buffers"]
fn vulkan_cli_planning_offload_and_transfers() {
    let file = fixture::model(32);
    let path = file.path().to_str().unwrap();
    let cpu = json(&cli(&[
        "run",
        path,
        "hello",
        "--max-tokens",
        "3",
        "--ram-bytes",
        "10000000",
        "--json",
    ]));
    for count in ["0", "1", "2", "auto"] {
        let gpu = json(&cli(&[
            "run",
            path,
            "hello",
            "--backend",
            "vulkan",
            "--gpu-layers",
            count,
            "--max-tokens",
            "3",
            "--ram-bytes",
            "10000000",
            "--json",
        ]));
        assert_eq!(gpu["tokens"], cpu["tokens"]);
    }
    let plan = json(&cli(&[
        "plan",
        path,
        "--backend",
        "vulkan",
        "--ctx",
        "16",
        "--prompt-tokens",
        "5",
        "--gpu-layers",
        "auto",
        "--ram-bytes",
        "10000000",
        "--json",
    ]));
    assert_eq!(plan["estimate"]["gpu_layers"], 2);
    assert_eq!(plan["fits"], true);
    let doctor = json(&cli(&["doctor", "--backend", "vulkan", "--json"]));
    assert_eq!(doctor["checks"][0]["status"], "ready");
    let bench = json(&cli(&[
        "bench",
        "--backend",
        "vulkan",
        "--size-mib",
        "1",
        "--iterations",
        "2",
        "--ram-bytes",
        "10000000",
        "--json",
    ]));
    assert_eq!(bench["measurements"].as_array().unwrap().len(), 4);
}
