use super::{Args, Command};
use std::{ffi::OsStr, path::Path};

#[test]
fn accepts_json_and_optional_saved_dmi() {
    let args = Args::parse_from(&[
        OsStr::new("info"),
        OsStr::new("--json"),
        OsStr::new("--dmi-file"),
        OsStr::new("memory.txt"),
    ])
    .unwrap();
    let Command::Info(args) = args.command else { panic!("expected info") };
    assert!(args.json);
    assert_eq!(args.dmi_file.as_deref(), Some(Path::new("memory.txt")));
    let args = Args::parse_from(&[OsStr::new("info")]).unwrap();
    let Command::Info(args) = args.command else { panic!("expected info") };
    assert!(!args.json);
    assert!(args.dmi_file.is_none());
}

#[test]
fn rejects_unknown_flags_and_missing_file_argument() {
    for args in [
        vec![],
        vec!["--json"],
        vec!["unknown"],
        vec!["info", "--unknown"],
        vec!["info", "--dmi-file"],
        vec!["info", "model.gguf"],
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        assert!(Args::parse_from(&args).is_err());
    }
}

#[test]
fn accepts_runtime_commands_and_offload_controls() {
    for args in [
        vec!["doctor", "--backend", "cpu", "--json"],
        vec!["plan", "model.gguf", "--ctx", "64", "--prompt-tokens", "8", "--backend", "vulkan", "--gpu-layers", "auto"],
        vec!["run", "model.gguf", "hello", "--gpu-layers", "2", "--max-tokens", "4"],
        vec!["bench", "--size-mib", "1", "--iterations", "2", "--json"],
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        Args::parse_from(&args).unwrap();
    }
    for args in [
        vec!["plan"],
        vec!["run", "model.gguf"],
        vec!["bench", "--size-mib", "not-a-number"],
        vec!["doctor", "--gpu-layers", "2"],
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        assert!(Args::parse_from(&args).is_err());
    }
}

#[test]
fn accepts_node_discovery_and_explicit_fallback() {
    for args in [
        vec!["node", "discover"],
        vec!["node", "discover", "--seconds", "8"],
        vec!["node", "discover", "--watch"],
        vec!["node", "discover", "--watch", "--table"],
        vec!["node", "discover", "--seconds", "8", "--table"],
        vec!["node", "discover", "--min-peer-version", "0.2.0"],
        vec!["node", "scan", "--peers", "http://192.0.2.10:50052", "--require-peer-build", "release-build"],
        vec!["node", "serve", "--listen", "192.0.2.20:50052", "--storage", "."],
        vec!["node", "serve", "--listen", "127.0.0.1:50052", "--storage", ".", "--no-discovery"],
        vec!["node", "scan", "--peers", "http://192.0.2.10:50052"],
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        Args::parse_from(&args).unwrap();
    }
    for args in [
        vec!["node", "serve", "--storage", "."],
        vec!["node", "scan"],
        vec!["node", "discover", "--seconds", "invalid"],
    ] {
        let args: Vec<_> = args.iter().map(OsStr::new).collect();
        assert!(Args::parse_from(&args).is_err());
    }
}
