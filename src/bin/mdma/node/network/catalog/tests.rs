use super::*;

#[test]
fn bundled_catalog_is_valid() {
    Catalog::parse(DATA).unwrap();
}

#[test]
fn hardware_is_not_a_negotiated_rate() {
    let mut nic = proto::NetworkInterface {
        hardware_identity: Some(proto::NetworkHardwareIdentity {
            bus: "pci".into(),
            vendor: "10ec".into(),
            device: "8168".into(),
            ..Default::default()
        }),
        interface_kind: "ethernet".into(),
        link_speed_bps: Some(100_000_000),
        ..Default::default()
    };
    apply(&mut nic);
    assert_eq!(
        nic.hardware_capabilities.as_ref().unwrap().max_phy_bps,
        Some(1_000_000_000)
    );
    assert_eq!(nic.link_speed_bps, Some(100_000_000));
    nic.link_speed_bps = None;
    apply(&mut nic);
    assert_eq!(nic.link_speed_bps, None);
    nic.visibility = "guest".into();
    nic.hardware_capabilities = None;
    apply(&mut nic);
    assert!(nic.hardware_capabilities.is_none());
}

#[test]
fn versioned_driver_ceiling_is_separate() {
    let catalog = Catalog::parse(DATA).unwrap();
    let mut nic = proto::NetworkInterface {
        driver: "iwlwifi".into(),
        os_version: "14.2-RELEASE-p3".into(),
        ..Default::default()
    };
    assert_eq!(
        catalog.driver(&nic, "freebsd").unwrap().max_phy_bps,
        Some(54_000_000)
    );
    nic.os_version = "14.3-RELEASE".into();
    let rule = catalog.driver(&nic, "freebsd").unwrap();
    assert!(rule.standards.contains(&"802.11ac".to_owned()));
    assert!(!rule.standards.contains(&"802.11ax".to_owned()));
    assert_eq!(rule.max_phy_bps, None);
    nic.os_version = "14.4-RELEASE".into();
    assert!(catalog.driver(&nic, "freebsd").is_none());
    assert!(catalog.driver(&nic, "linux").is_none());
    assert!(version("unknown").is_none());
    assert!(version("14.10").unwrap() > version("14.3").unwrap());
    nic.os_version = "14.3".into(); // sysinfo may omit the development suffix.
    nic.kernel_build = "FreeBSD 14.3-STABLE".into();
    assert!(catalog.driver(&nic, "freebsd").is_none());
}

#[test]
fn specific_identity_wins_and_ties_are_unknown() {
    let extra = r#"
[[hardware]]
id = "specific"
bus = "pci"
vendor = "10ec"
device = "8168"
revision = "15"
model = "specific"
sources = ["test"]
notes = ""
"#;
    let data = format!("{DATA}{extra}");
    let catalog = Catalog::parse(&data).unwrap();
    let mut identity = proto::NetworkHardwareIdentity {
        bus: "pci".into(),
        vendor: "10ec".into(),
        device: "8168".into(),
        revision: "15".into(),
        ..Default::default()
    };
    assert_eq!(catalog.hardware(&identity).unwrap().id, "specific");
    let data = format!(
        "{data}{}",
        extra.replace("id = \"specific\"", "id = \"duplicate-selector\"")
    );
    assert!(Catalog::parse(&data).unwrap().hardware(&identity).is_none());
    identity.revision.clear();
    assert_eq!(
        catalog.hardware(&identity).unwrap().id,
        "realtek-8168-family"
    );
    identity.bus = "apple_airport".into();
    assert!(catalog.hardware(&identity).is_none());
}

#[test]
fn rejects_invalid_catalog_and_current_requires_hash() {
    for data in [
        DATA.replace("schema_version = 1", "schema_version = 2"),
        DATA.replace("vendor = \"10ec\"", "vendor = \"10EC\""),
        DATA.replace("max_phy_bps = 1000000000", "max_phy_bps = 0"),
        format!("{DATA}\n[[hardware]]\nid = \"incomplete\"\n"),
    ] {
        assert!(Catalog::parse(&data).is_err());
    }
    let catalog = Catalog::parse(DATA).unwrap();
    let mut nic = proto::NetworkInterface {
        driver: "re".into(),
        os_version: "16.0-CURRENT".into(),
        hardware_identity: Some(proto::NetworkHardwareIdentity {
            bus: "pci".into(),
            vendor: "10ec".into(),
            device: "8168".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(catalog.driver(&nic, "freebsd").is_none());
    nic.kernel_build = "FreeBSD 16.0-CURRENT unstable-n289723-7c94a196f533:".into();
    assert!(catalog.driver(&nic, "freebsd").is_some());
}
