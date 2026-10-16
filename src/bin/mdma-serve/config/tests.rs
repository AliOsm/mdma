use super::*;
use std::{assert_matches, collections::HashMap};

fn environment() -> HashMap<&'static str, String> {
    [
        ("MDMA_MODELS_DIR", "/models"),
        ("MDMA_LISTEN", "127.0.0.1:6362"),
        ("MDMA_TLS_CERT", "/certs/server.pem"),
        ("MDMA_TLS_KEY", "/certs/server-key.pem"),
        ("MDMA_TLS_CA", "/certs/ca.pem"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_string()))
    .collect()
}

fn load(env: &HashMap<&str, String>) -> Result<Config, BoxError> {
    Config::load(|key| env.get(key).cloned().ok_or(VarError::NotPresent))
}

#[test]
fn defaults_and_overrides() {
    let mut env = environment();
    env.remove("MDMA_LISTEN");
    assert_eq!(
        load(&env).unwrap().listen.to_string(),
        "127.0.0.1:6362",
        "loopback on the mdma keypad port"
    );
    env = environment();
    let config = load(&env).unwrap();
    assert_eq!(config.backend, "cpu");
    assert!(!config.rocm_f32);
    assert_eq!(config.n_ctx, None, "each model's trained context");
    assert_eq!(
        config.max_output_tokens, 4096,
        "bounds a request's up-front KV"
    );
    assert_eq!(config.native_tensor_bytes, None);
    assert_eq!(config.device_buffer_bytes, None);
    assert_eq!(config.host_buffer_bytes, None);
    assert_eq!(config.shutdown_timeout, Duration::from_secs(30));
    assert_eq!(
        config.warmup_rounds, 1,
        "warm-up delays a model's first request"
    );
    assert_eq!(config.idle_unload, None, "models stay until evicted");
    assert!(config.federation_config.is_none());
    env.insert("MDMA_BACKEND", "rocm".into());
    env.insert("MDMA_N_CTX", "128".into());
    env.insert("MDMA_MAX_OUTPUT_TOKENS", "512".into());
    env.insert("MDMA_NATIVE_TENSOR_BYTES", "8589934592".into());
    env.insert("MDMA_DEVICE_BUFFER_BYTES", "17179869184".into());
    env.insert("MDMA_HOST_BUFFER_BYTES", "1073741824".into());
    env.insert("MDMA_SHUTDOWN_TIMEOUT_SECS", "90".into());
    let config = load(&env).unwrap();
    assert_eq!(config.backend, "rocm");
    assert_eq!(config.n_ctx, Some(128));
    assert_eq!(config.max_output_tokens, 512);
    assert_eq!(config.native_tensor_bytes, Some(8 * 1024 * 1024 * 1024));
    assert_eq!(config.device_buffer_bytes, Some(16 * 1024 * 1024 * 1024));
    assert_eq!(config.host_buffer_bytes, Some(1024 * 1024 * 1024));
    assert_eq!(config.shutdown_timeout, Duration::from_secs(90));
}

#[test]
fn cpu_matmul_is_exact_unless_packed_is_named() {
    use mdma::backend::cpu::Matmul;
    let mut env = environment();
    assert_eq!(load(&env).unwrap().cpu_matmul, Matmul::Exact);
    for (value, mode) in [("exact", Matmul::Exact), ("packed", Matmul::Packed)] {
        env.insert("MDMA_CPU_MATMUL", value.into());
        assert_eq!(load(&env).unwrap().cpu_matmul, mode, "{value}");
    }
    for value in ["", "1", "Packed", "fast"] {
        env.insert("MDMA_CPU_MATMUL", value.into());
        assert!(load(&env).is_err(), "{value:?}");
    }
}

#[test]
fn rocm_reference_mode_is_explicit_and_validated() {
    let mut env = environment();
    env.insert("MDMA_BACKEND", "rocm".into());
    env.insert("MDMA_HIP_F32", "1".into());
    assert!(load(&env).unwrap().rocm_f32);
    env.insert("MDMA_HIP_F32", "0".into());
    assert!(!load(&env).unwrap().rocm_f32);
    for value in ["", "true", "2"] {
        env.insert("MDMA_HIP_F32", value.into());
        assert!(load(&env).is_err());
    }
}

#[test]
fn federation_is_explicitly_opt_in_and_nonempty() {
    let mut env = environment();
    env.insert("MDMA_FEDERATION_CONFIG", String::new());
    assert!(load(&env).is_err());
    env.insert("MDMA_FEDERATION_CONFIG", "/private/federation.toml".into());
    assert_eq!(
        load(&env).unwrap().federation_config.as_deref(),
        Some("/private/federation.toml")
    );
    let error = Config::load(|name| {
        if name == "MDMA_FEDERATION_CONFIG" {
            Err(VarError::NotUnicode(std::ffi::OsString::new()))
        } else {
            env.get(name).cloned().ok_or(VarError::NotPresent)
        }
    })
    .err()
    .unwrap();
    assert!(error.to_string().contains("MDMA_FEDERATION_CONFIG"));
}

#[test]
fn control_only_bootstrap_never_loads_models_or_initializes_compute() {
    let mut env = environment();
    env.insert("MDMA_FEDERATION_CONTROL_ONLY", "1".into());
    env.insert("MDMA_FEDERATION_CONFIG", "/private/federation.toml".into());
    assert!(load(&env).is_err()); // A models folder conflicts with control-only mode.
    env.remove("MDMA_MODELS_DIR");
    assert_matches!(load(&env).unwrap().models, ModelsSource::ControlOnly);
    env.remove("MDMA_FEDERATION_CONFIG");
    assert!(load(&env).is_err());
    env.insert("MDMA_FEDERATION_CONFIG", "/private/federation.toml".into());
    env.insert("MDMA_BACKEND", "metal".into());
    assert!(load(&env).is_err());
    for value in ["", "true", "auto", "2"] {
        env.insert("MDMA_FEDERATION_CONTROL_ONLY", value.into());
        assert!(load(&env).is_err());
    }
}

#[test]
fn native_tensor_budget_requires_a_positive_byte_count() {
    for value in ["", " ", "0", "-1", "1.5", "8GiB", "18446744073709551616"] {
        let mut env = environment();
        env.insert("MDMA_NATIVE_TENSOR_BYTES", value.into());
        assert!(
            load(&env)
                .err()
                .unwrap()
                .to_string()
                .contains("MDMA_NATIVE_TENSOR_BYTES")
        );
    }
    let env = environment();
    let error = Config::load(|name| {
        if name == "MDMA_NATIVE_TENSOR_BYTES" {
            Err(VarError::NotUnicode(std::ffi::OsString::new()))
        } else {
            env.get(name).cloned().ok_or(VarError::NotPresent)
        }
    })
    .err()
    .unwrap();
    assert!(error.to_string().contains("MDMA_NATIVE_TENSOR_BYTES"));
}

#[test]
fn buffer_limits_are_strict_and_require_a_supported_backend() {
    for key in ["MDMA_DEVICE_BUFFER_BYTES", "MDMA_HOST_BUFFER_BYTES"] {
        for value in ["", " ", "0", "-1", "1.5", "8GiB", "18446744073709551616"] {
            let mut env = environment();
            env.insert("MDMA_BACKEND", "vulkan".into());
            env.insert(key, value.into());
            assert!(load(&env).err().unwrap().to_string().contains(key));
        }
        let mut env = environment();
        env.insert(key, "1024".into());
        for backend in ["cpu", "level0", "metal"] {
            env.insert("MDMA_BACKEND", backend.into());
            assert!(load(&env).err().unwrap().to_string().contains(key));
        }
        for backend in ["rocm", "cuda", "vulkan"] {
            env.insert("MDMA_BACKEND", backend.into());
            assert!(load(&env).is_ok());
        }
        let error = Config::load(|name| {
            if name == key {
                Err(VarError::NotUnicode(std::ffi::OsString::new()))
            } else {
                env.get(name).cloned().ok_or(VarError::NotPresent)
            }
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains(key));
    }
}

#[test]
fn models_come_from_one_folder_with_an_optional_default() {
    let mut env = environment();
    assert_matches!(load(&env).unwrap().models, ModelsSource::Dir { path, default: None } if path == "/models");
    env.insert("MDMA_DEFAULT_MODEL", " minicheck ".into());
    assert_matches!(load(&env).unwrap().models, ModelsSource::Dir { default: Some(name), .. } if name == "minicheck");
    env.insert("MDMA_DEFAULT_MODEL", " ".into());
    assert!(
        load(&env)
            .err()
            .unwrap()
            .to_string()
            .contains("MDMA_DEFAULT_MODEL")
    );
}

#[test]
fn required_values_cannot_be_missing_or_blank() {
    for key in environment().keys() {
        let mut env = environment();
        if *key != "MDMA_LISTEN" {
            env.remove(key);
            assert!(load(&env).err().unwrap().to_string().contains(key));
        }
        env.insert(key, "  ".into());
        assert!(load(&env).err().unwrap().to_string().contains(key));
    }
}

#[test]
fn invalid_numeric_configuration_is_not_silently_defaulted() {
    for key in [
        "MDMA_N_CTX",
        "MDMA_MAX_OUTPUT_TOKENS",
        "MDMA_SHUTDOWN_TIMEOUT_SECS",
    ] {
        for value in ["", "0", "-1", "many", "1.5", "4294967296"] {
            let mut env = environment();
            env.insert(key, value.into());
            assert!(load(&env).err().unwrap().to_string().contains(key));
        }
    }
    let mut env = environment();
    env.insert("MDMA_SHUTDOWN_TIMEOUT_SECS", "86401".into());
    assert!(load(&env).is_err());
}

#[test]
fn invalid_listen_address_is_reported() {
    let mut env = environment();
    env.insert("MDMA_LISTEN", "not a socket".into());
    assert!(
        load(&env)
            .err()
            .unwrap()
            .to_string()
            .contains("MDMA_LISTEN")
    );
}

#[test]
fn power_defaults_and_overrides() {
    let mut env = environment();
    let power = load(&env).unwrap().power.expect("auto by default");
    assert_eq!(power.watch, WatchPolicy::default());
    assert_eq!(power.degraded, DegradedLimits::default());
    env.insert("MDMA_POWER_POLL_SECS", "1".into());
    env.insert("MDMA_POWER_DEBOUNCE", "3".into());
    env.insert("MDMA_POWER_CRITICAL_PCT", "0".into());
    env.insert("MDMA_POWER_GPU_CAP_PCT", "0".into());
    env.insert("MDMA_DEGRADED_MAX_TOKENS", "64".into());
    env.insert("MDMA_DEGRADED_N_CTX", "512".into());
    let power = load(&env).unwrap().power.unwrap();
    assert_eq!(
        power.watch,
        WatchPolicy {
            poll: Duration::from_secs(1),
            debounce: 3,
            critical_percent: 0,
            gpu_cap_percent: 0,
        }
    );
    assert_eq!(
        power.degraded,
        DegradedLimits {
            max_tokens: 64,
            n_ctx: 512
        }
    );
    env.insert("MDMA_POWER", "off".into());
    assert!(load(&env).unwrap().power.is_none());
}

#[test]
fn invalid_power_configuration_is_rejected() {
    for (key, value) in [
        ("MDMA_POWER", "on"),
        ("MDMA_POWER", ""),
        ("MDMA_POWER_POLL_SECS", "0"),
        ("MDMA_POWER_POLL_SECS", "3601"),
        ("MDMA_POWER_DEBOUNCE", "0"),
        ("MDMA_POWER_CRITICAL_PCT", "100"),
        ("MDMA_POWER_CRITICAL_PCT", "-1"),
        ("MDMA_POWER_GPU_CAP_PCT", "101"),
        ("MDMA_DEGRADED_MAX_TOKENS", "0"),
        ("MDMA_DEGRADED_N_CTX", "many"),
    ] {
        let mut env = environment();
        env.insert(key, value.into());
        assert!(
            load(&env).err().unwrap().to_string().contains(key),
            "{key}={value}"
        );
    }
}

#[test]
fn partial_power_override_keeps_other_defaults() {
    let mut env = environment();
    env.insert("MDMA_POWER_DEBOUNCE", "7".into());
    let power = load(&env).unwrap().power.unwrap();
    assert_eq!(
        power.watch,
        WatchPolicy {
            debounce: 7,
            ..WatchPolicy::default()
        }
    );
    assert_eq!(power.degraded, DegradedLimits::default());
}

#[test]
fn numeric_settings_do_not_default_non_unicode_values() {
    let env = environment();
    for key in [
        "MDMA_N_CTX",
        "MDMA_MAX_OUTPUT_TOKENS",
        "MDMA_SHUTDOWN_TIMEOUT_SECS",
        "MDMA_WARMUP_ROUNDS",
        "MDMA_POWER_POLL_SECS",
        "MDMA_POWER_DEBOUNCE",
        "MDMA_POWER_CRITICAL_PCT",
        "MDMA_POWER_GPU_CAP_PCT",
        "MDMA_DEGRADED_MAX_TOKENS",
        "MDMA_DEGRADED_N_CTX",
    ] {
        let error = Config::load(|name| {
            if name == key {
                Err(VarError::NotUnicode(std::ffi::OsString::new()))
            } else {
                env.get(name).cloned().ok_or(VarError::NotPresent)
            }
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains(key), "{key}: {error}");
    }
}

#[test]
fn warmup_rounds_are_bounded_and_zero_explicitly_disables() {
    let mut env = environment();
    for rounds in [0, 1, 8] {
        env.insert("MDMA_WARMUP_ROUNDS", rounds.to_string());
        assert_eq!(load(&env).unwrap().warmup_rounds, rounds);
    }
    for value in ["", " ", "-1", "1.5", "many", "9", "4294967296"] {
        env.insert("MDMA_WARMUP_ROUNDS", value.into());
        assert!(
            load(&env)
                .err()
                .unwrap()
                .to_string()
                .contains("MDMA_WARMUP_ROUNDS")
        );
    }
}

#[test]
fn idle_unload_is_bounded_and_zero_means_never() {
    let mut env = environment();
    for (value, expected) in [
        ("0", None),
        ("1", Some(Duration::from_secs(1))),
        ("86400", Some(Duration::from_hours(24))),
    ] {
        env.insert("MDMA_IDLE_UNLOAD_SECS", value.into());
        assert_eq!(load(&env).unwrap().idle_unload, expected, "{value}");
    }
    for value in ["", "-1", "86401", "soon"] {
        env.insert("MDMA_IDLE_UNLOAD_SECS", value.into());
        assert!(
            load(&env)
                .err()
                .unwrap()
                .to_string()
                .contains("MDMA_IDLE_UNLOAD_SECS"),
            "{value}"
        );
    }
}

#[test]
fn operator_pins_are_validated() {
    let mut env = environment();
    assert!(load(&env).unwrap().operators.is_empty());
    let pin = format!("sha256:{}", "ab".repeat(32));
    env.insert(
        "MDMA_OPERATOR_SPKI",
        format!(" {pin} , sha256:{} ,", "0".repeat(64)),
    );
    assert_eq!(
        load(&env).unwrap().operators,
        [pin, format!("sha256:{}", "0".repeat(64))]
    );
    for bad in [
        "ab".repeat(32),
        format!("sha256:{}", "AB".repeat(32)),
        "sha256:abc".into(),
        format!("sha1:{}", "0".repeat(64)),
    ] {
        env.insert("MDMA_OPERATOR_SPKI", bad.clone());
        let error = load(&env).err().unwrap().to_string();
        assert!(error.contains("MDMA_OPERATOR_SPKI"), "{bad}: {error}");
    }
}

#[test]
fn node_name_defaults_to_the_short_host_name() {
    let mut env = environment();
    let host = sysinfo::System::host_name().unwrap_or_default();
    assert_eq!(
        load(&env).unwrap().node,
        host.split('.').next().unwrap_or_default()
    );
    env.insert("MDMA_NODE_NAME", " foundry ".into());
    assert_eq!(load(&env).unwrap().node, "foundry");
    env.insert("MDMA_NODE_NAME", " ".into());
    assert!(
        load(&env)
            .err()
            .unwrap()
            .to_string()
            .contains("MDMA_NODE_NAME")
    );
}
