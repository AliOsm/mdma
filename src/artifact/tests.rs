use super::*;

#[test]
fn frozen_copy_hashes_all_bytes_and_survives_original_writes() {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source
        .write_all(b"metadata\0tokenizer\0weights\0trailer")
        .unwrap();
    let original = std::fs::read(source.path()).unwrap();
    let (mut frozen, id) = freeze_file(source.reopen().unwrap()).unwrap();
    assert_eq!(id, ArtifactId::digest(&original));
    std::fs::write(source.path(), b"replacement").unwrap();
    let mut loaded = Vec::new();
    frozen.read_to_end(&mut loaded).unwrap();
    assert_eq!(loaded, original);
}

#[test]
fn nonregular_source_cannot_be_frozen() {
    let dir = tempfile::tempdir().unwrap();
    assert!(freeze_file(File::open(dir.path()).unwrap()).is_err());
}

#[test]
fn changed_source_is_rejected_instead_of_publishing_its_snapshot() {
    let source = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(source.path(), b"initial").unwrap();
    let error = freeze_file_with(source.reopen().unwrap(), || {
        std::fs::write(source.path(), b"changed source").unwrap();
    })
    .unwrap_err();
    assert!(error.to_string().contains("changed while reading"));
}

#[test]
fn laya_manifest_is_canonical_sorted_and_hashes_original_bytes() {
    let files = [
        ("z/config.json", b"abc".to_vec()),
        ("a/weights", Vec::new()),
    ];
    let manifest = concat!(
        r#"{"files":[{"path":"a/weights","sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"},"#,
        r#"{"path":"z/config.json","sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}],"format":"mdma.laya-artifact.v1"}"#
    );
    assert_eq!(laya_id(&files), ArtifactId::digest(manifest.as_bytes()));
    let reversed = [files[1].clone(), files[0].clone()];
    assert_eq!(laya_id(&files), laya_id(&reversed));
    let mut changed = files.clone();
    changed[0].1.push(b' ');
    assert_ne!(laya_id(&files), laya_id(&changed));
}
