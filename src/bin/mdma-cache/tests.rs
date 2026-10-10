use super::Args;
use std::{ffi::OsStr, path::Path};

#[test]
fn accepts_source_force_and_cache_dir_in_either_order() {
    for args in [
        vec!["model.gguf", "--force", "--cache-dir", "/tmp/cache"],
        vec!["--cache-dir", "/tmp/cache", "--force", "model.gguf"],
    ] {
        let values: Vec<_> = args.iter().map(OsStr::new).collect();
        let parsed = Args::parse_from(&values).unwrap();
        assert_eq!(parsed.model, Path::new("model.gguf"));
        assert_eq!(parsed.cache_dir.as_deref(), Some(Path::new("/tmp/cache")));
        assert!(parsed.force);
    }
    let parsed = Args::parse_from(&[OsStr::new("model.gguf")]).unwrap();
    assert!(!parsed.force);
    assert!(parsed.cache_dir.is_none());
}

#[test]
fn rejects_missing_source_unknown_flags_and_extra_arguments() {
    for args in [vec![], vec!["--force"], vec!["model.gguf", "--unknown"], vec!["a.gguf", "b.gguf"]] {
        let values: Vec<_> = args.iter().map(OsStr::new).collect();
        assert!(Args::parse_from(&values).is_err(), "{args:?}");
    }
}
