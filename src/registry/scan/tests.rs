use super::*;

fn laya(dir: &Path) {
    for file in LAYA_FILES {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }
}

fn names(found: &[(String, Source)]) -> Vec<&str> {
    found.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn ggufs_and_complete_laya_folders_are_models_sorted_by_name() {
    let dir = tempfile::tempdir().unwrap();
    for file in [
        "tiny.gguf",
        "Minicheck-7B.Q4.gguf",
        ".hidden.gguf",
        "notes.txt",
        "bad name.gguf",
    ] {
        std::fs::write(dir.path().join(file), b"").unwrap();
    }
    laya(&dir.path().join("laya"));
    std::fs::create_dir_all(dir.path().join("partial/encoder")).unwrap();
    std::fs::write(dir.path().join("partial/model.safetensors"), b"").unwrap();
    let found = scan(dir.path()).unwrap();
    assert_eq!(names(&found), ["Minicheck-7B.Q4", "laya", "tiny"]);
    assert_eq!(found[1].1, Source::Laya(dir.path().join("laya")));
    assert_eq!(found[2].1, Source::Gguf(dir.path().join("tiny.gguf")));
}

#[cfg(unix)]
#[test]
fn symlinks_count_where_they_point_and_dangling_ones_are_skipped() {
    let models = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("real.gguf"), b"").unwrap();
    laya(&elsewhere.path().join("checkpoint"));
    std::os::unix::fs::symlink(
        elsewhere.path().join("real.gguf"),
        models.path().join("alias.gguf"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        elsewhere.path().join("checkpoint"),
        models.path().join("decider"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        elsewhere.path().join("gone.gguf"),
        models.path().join("gone.gguf"),
    )
    .unwrap();
    assert_eq!(names(&scan(models.path()).unwrap()), ["alias", "decider"]);
}

#[test]
fn a_gguf_and_a_laya_folder_cannot_share_a_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("judge.gguf"), b"").unwrap();
    laya(&dir.path().join("judge"));
    let error = scan(dir.path()).unwrap_err().to_string();
    assert!(error.contains("\"judge\" is both"), "{error}");
    assert!(scan(&dir.path().join("missing")).is_err());
}
