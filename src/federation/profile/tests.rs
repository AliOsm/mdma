use super::*;
use std::io::{Seek, Write};

fn fake_executor() -> Arc<ExecutorIdentity> {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"canonical profile test only").unwrap();
    let (hash, stamp) = fingerprint(&mut file).unwrap();
    Arc::new(ExecutorIdentity {
        file,
        stamp,
        build_id: format!("sha256:{hash}"),
    })
}

#[test]
fn cpu_profile_is_canonical_and_workload_specific_not_a_vcs_token() {
    let executor = fake_executor();
    let decoder = ExecutionProfile::cpu(executor.clone(), false);
    let laya = ExecutionProfile::cpu(executor, true);
    let encoded = decoder.manifest().encode();
    assert_eq!(
        decoder.as_str(),
        format!("sha256:{}", hex::encode(Sha256::digest(encoded.as_bytes())))
    );
    let decoded = super::super::canonical_json::parse(encoded.as_bytes(), 8).unwrap();
    assert_eq!(&decoded, decoder.manifest());
    let object = decoded.as_object().unwrap();
    assert_eq!(object["build_id"].as_str(), Some(decoder.build_id()));
    assert_ne!(decoder.build_id(), env!("MDMA_BUILD_ID"));
    assert_eq!(object["backend"].as_str(), Some("cpu"));
    assert_eq!(object["activation_dtype"].as_str(), Some("f32"));
    assert_eq!(object["kv_dtype"].as_str(), Some("f32"));
    assert_eq!(
        laya.manifest().as_object().unwrap()["kv_dtype"].as_str(),
        Some("none")
    );
    assert_ne!(decoder.as_str(), laya.as_str());
    assert!(
        !decoder.is_current(),
        "a fake file must never pass actual-image validation"
    );
}

#[test]
fn executor_bytes_and_every_selected_mode_are_part_of_the_profile_digest() {
    let first = ExecutionProfile::cpu(fake_executor(), false);
    let mut different = tempfile::tempfile().unwrap();
    different.write_all(b"different executor").unwrap();
    let (hash, stamp) = fingerprint(&mut different).unwrap();
    let second = ExecutionProfile::cpu(
        Arc::new(ExecutorIdentity {
            file: different,
            stamp,
            build_id: format!("sha256:{hash}"),
        }),
        false,
    );
    assert_ne!(first.as_str(), second.as_str());
    let original = first.manifest().clone();
    let modes = original.as_object().unwrap()["modes"].as_object().unwrap();
    for key in modes.keys() {
        let Value::Object(mut changed) = original.clone() else {
            unreachable!()
        };
        let Value::Object(mut changed_modes) = changed.remove("modes").unwrap() else {
            unreachable!()
        };
        changed_modes.insert(key.clone(), Value::String("different-mode".into()));
        changed.insert("modes".into(), Value::Object(changed_modes));
        let digest = hex::encode(Sha256::digest(Value::Object(changed).encode().as_bytes()));
        assert_ne!(first.as_str(), format!("sha256:{digest}"), "{key}");
    }
}

fn test_gpu(device: &str, health: gpu::Health) -> gpu::Gpu {
    gpu::Gpu::immutable(
        "metal",
        "f32",
        "f32",
        gpu::fields([
            ("vocabulary", "mdma.metal.v1".into()),
            ("device_registry_id", device.into()),
            ("kernel_source_id", "test-shader".into()),
            ("execution", "request-owned-command-batches".into()),
        ]),
        health,
    )
}

#[test]
fn gpu_device_modes_and_kernel_identity_are_in_the_canonical_digest() {
    let executor = fake_executor();
    let first = ExecutionProfile::gpu(
        executor.clone(),
        test_gpu("device-a", gpu::Health::default()),
    );
    let second = ExecutionProfile::gpu(
        executor.clone(),
        test_gpu("device-b", gpu::Health::default()),
    );
    assert_ne!(first.as_str(), second.as_str());
    let mut changed = test_gpu("device-a", gpu::Health::default());
    let Value::Object(ref mut modes) = changed.modes else {
        unreachable!()
    };
    modes.insert(
        "kernel_source_id".into(),
        Value::String("changed-shader".into()),
    );
    assert_ne!(
        first.as_str(),
        ExecutionProfile::gpu(executor, changed).as_str()
    );
    assert_eq!(
        first.manifest().as_object().unwrap()["backend"].as_str(),
        Some("metal")
    );
    let parsed =
        super::super::canonical_json::parse(first.manifest().encode().as_bytes(), 8).unwrap();
    assert_eq!(&parsed, first.manifest());
}

#[test]
fn mutable_gpu_modes_and_device_failure_withdraw_captured_profiles_without_aba() {
    let state = gpu::Modes::default();
    let health = gpu::Health::default();
    let snapshot = state
        .capture(|| test_gpu("device", health.clone()))
        .unwrap();
    assert!(snapshot.current());
    state.change(|| {});
    assert!(!snapshot.current());
    state.change(|| {});
    assert!(
        !snapshot.current(),
        "restoring modes cannot resurrect an old snapshot"
    );
    let renewed = state
        .capture(|| test_gpu("device", health.clone()))
        .unwrap();
    assert!(renewed.current());
    let _: crate::backend::Result<()> = health.check(Err(crate::backend::Error::Backend(
        "GPU join failed".into(),
    )));
    assert!(
        !renewed.current(),
        "an unjoined failed device cannot remain exportable"
    );
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
fn dropped_gpu_backend_withdraws_retained_provenance() {
    let backend = crate::backend::metal::MetalBackend::new().unwrap();
    let snapshot = backend.federation_profile().unwrap();
    assert!(snapshot.current());
    drop(backend);
    assert!(!snapshot.current());
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
#[test]
fn running_executable_digest_matches_the_complete_kernel_backed_descriptor() {
    let captured = ExecutorIdentity::capture().unwrap();
    assert!(captured.current());
    let mut actual = executable::open().unwrap();
    actual.rewind().unwrap();
    let (expected, _) = fingerprint(&mut actual).unwrap();
    assert_eq!(captured.build_id(), format!("sha256:{expected}"));
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
#[test]
fn executable_replacement_and_unlink_never_hash_the_new_path() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    for mode in ["replace", "unlink"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("executor");
        let mut copy = File::create(&path).unwrap();
        std::io::copy(&mut executable::open().unwrap(), &mut copy).unwrap();
        copy.set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        drop(copy);
        let output = Command::new(&path)
            .args([
                "--exact",
                "federation::profile::tests::child_image_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MDMA_PROFILE_PROBE_DIR", dir.path())
            .env("MDMA_PROFILE_PROBE_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
#[test]
#[ignore = "private subprocess executable mutation probe only"]
fn child_image_probe() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("MDMA_PROFILE_PROBE_DIR").expect("probe directory"),
    );
    let mode = std::env::var("MDMA_PROFILE_PROBE_MODE").expect("probe mode");
    let first = ExecutorIdentity::capture().unwrap();
    assert!(first.current());
    let path = dir.join("executor");
    match mode.as_str() {
        "replace" => std::fs::rename(&path, dir.join("running-moved")).unwrap(),
        "unlink" => std::fs::remove_file(&path).unwrap(),
        _ => panic!("invalid probe mode"),
    }
    let replacement = b"this later pathname is not the running executable";
    std::fs::write(&path, replacement).unwrap();
    assert!(
        !first.current(),
        "a deployment change conservatively withdraws the old snapshot"
    );
    if let Ok(new) = ExecutorIdentity::capture() {
        assert_eq!(new.build_id(), first.build_id());
        assert_ne!(
            new.build_id(),
            format!("sha256:{}", hex::encode(Sha256::digest(replacement)))
        );
    }
}
