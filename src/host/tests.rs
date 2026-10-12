use super::*;

#[test]
fn detects_this_machine() {
    let host = detect("periscope", "Apple M4 Max");
    assert_eq!(host.name, "periscope");
    assert_eq!(host.device, "Apple M4 Max");
    assert_eq!(host.os, std::env::consts::OS);
    assert!(!host.os_name.is_empty());
    assert!(!host.arch.is_empty());
    assert!(host.memory_bytes > 0);
    assert!(is_valid(&host), "{host:?}");
}

#[test]
fn freebsd_keeps_its_branch() {
    for (banner, release) in [
        (
            "FreeBSD 16.0-CURRENT #1 unstable-n289560-b2fe7cd569e0: Sun Sep 20 15:02:34 UTC 2026\n    root@citadel:/usr/obj/GENERIC\n",
            "FreeBSD 16.0-CURRENT",
        ),
        (
            "FreeBSD 15.1-RELEASE releng/15.1-n270000-0123456789ab GENERIC",
            "FreeBSD 15.1-RELEASE",
        ),
        ("FreeBSD 15.1-RELEASE-p2 GENERIC", "FreeBSD 15.1-RELEASE-p2"),
    ] {
        assert_eq!(freebsd_release(banner), release, "{banner:?}");
    }
}

#[test]
fn fields_are_bounded_printable_ascii() {
    let host = detect("  node\tname\u{e9}\n", &"x".repeat(300));
    assert_eq!(host.name, "node name");
    assert_eq!(host.device.len(), MAX_FIELD_BYTES);
    assert!(is_valid(&host));
    assert!(!is_valid(&Host {
        cpu: "caf\u{e9}".into(),
        ..Host::default()
    }));
    assert!(!is_valid(&Host {
        os_name: "a".repeat(MAX_FIELD_BYTES + 1),
        ..Host::default()
    }));
}

#[test]
fn live_figures_follow_the_models_folder_to_its_disk() {
    let host = detect("periscope", "CPU");
    let dir = tempfile::tempdir().unwrap();
    let live = live(&host, Some(dir.path()), (Some(1 << 30), Some(8 << 30)));
    assert_eq!(
        (live.name.as_str(), live.os.as_str()),
        ("periscope", host.os.as_str()),
        "static fields are kept"
    );
    assert!(live.memory_available_bytes > 0 && live.memory_available_bytes <= live.memory_bytes);
    assert_eq!(
        (live.device_available_bytes, live.device_memory_bytes),
        (1 << 30, 8 << 30)
    );
    assert!(live.disk_bytes > 0 && live.disk_available_bytes <= live.disk_bytes);
    #[cfg(unix)]
    {
        let link = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path(), link.path().join("models")).unwrap();
        assert_eq!(
            disk_of(&link.path().join("models")).map(|(total, _)| total),
            Some(live.disk_bytes),
            "a linked folder reports its target's disk"
        );
    }
    assert_eq!(live_without_disk(&host).disk_bytes, 0);
}

fn live_without_disk(host: &Host) -> Host {
    live(host, None, (None, None))
}
