use super::*;

fn checkpoint() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for path in PATHS {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"original").unwrap();
    }
    dir
}

#[test]
fn every_consumed_file_affects_identity_but_unused_files_and_location_do_not() {
    let a = checkpoint();
    let b = checkpoint();
    let initial = Sources::capture(a.path()).unwrap().id();
    assert_eq!(initial, Sources::capture(b.path()).unwrap().id());
    std::fs::write(b.path().join("unused.json"), b"not consumed").unwrap();
    assert_eq!(initial, Sources::capture(b.path()).unwrap().id());
    for path in PATHS {
        std::fs::write(b.path().join(path), b"modified").unwrap();
        assert_ne!(initial, Sources::capture(b.path()).unwrap().id(), "{path}");
        std::fs::write(b.path().join(path), b"original").unwrap();
    }
}

#[test]
fn parsers_receive_the_captured_set_after_paths_change() {
    let dir = checkpoint();
    let sources = Sources::capture(dir.path()).unwrap();
    let id = sources.id();
    for path in PATHS {
        std::fs::remove_file(dir.path().join(path)).unwrap();
    }
    assert_eq!(sources.weights(), b"original");
    for text in [
        sources.agent(),
        sources.encoder(),
        sources.tokenizer(),
        sources.tokenizer_config(),
    ] {
        assert_eq!(text.unwrap(), "original");
    }
    assert_eq!(id, sources.id());
    assert!(Sources::capture(dir.path()).is_err());
}

#[test]
fn a_writer_anywhere_in_the_consumed_set_during_capture_is_rejected() {
    let dir = checkpoint();
    let result = Sources::capture_with(dir.path(), || {
        std::fs::write(
            dir.path().join("rl_agent_config.json"),
            b"changed calibration",
        )
        .unwrap();
    });
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("changed while reading")
    );
}

#[test]
fn missing_nonregular_or_non_utf8_consumed_file_fails_closed() {
    let dir = checkpoint();
    let path = dir.path().join("encoder/config.json");
    std::fs::remove_file(&path).unwrap();
    assert!(Sources::capture(dir.path()).is_err());
    std::fs::create_dir(&path).unwrap();
    assert!(Sources::capture(dir.path()).is_err());
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, [0xff]).unwrap();
    assert!(Sources::capture(dir.path()).unwrap().encoder().is_err());
}

#[cfg(unix)]
#[test]
fn symlink_target_paths_are_not_identity_and_retargeting_does_not_change_captured_bytes() {
    let a = checkpoint();
    let b = checkpoint();
    let path = a.path().join("model.safetensors");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(b.path().join("model.safetensors"), &path).unwrap();
    let sources = Sources::capture(a.path()).unwrap();
    assert_eq!(sources.id(), Sources::capture(b.path()).unwrap().id());
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    assert_eq!(sources.weights(), b"original");
    assert_ne!(sources.id(), Sources::capture(a.path()).unwrap().id());
}
