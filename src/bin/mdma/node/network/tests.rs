use super::*;

#[test]
fn medium_requires_evidence_not_speed() {
    assert_eq!(
        selected_medium("media: autoselect (1000baseT <full-duplex>)"),
        "copper"
    );
    assert_eq!(
        selected_medium("media: autoselect (10Gbase-SR <full-duplex>)"),
        "fiber"
    );
    assert_eq!(
        selected_medium("media: autoselect (10Gbase-CR <full-duplex>)"),
        "unknown"
    );
    assert_eq!(selected_medium("media: autoselect"), "unknown");
    assert_eq!(ethtool_medium("Speed: 10000Mb/s\nPort: Other"), "unknown");
    assert_eq!(ethtool_medium("Port: Twisted Pair"), "copper");
    assert_eq!(ethtool_medium("Port: FIBRE"), "fiber");
    assert!(virtual_driver("virtio_net"));
    assert!(!virtual_driver("e1000")); // Can be physical or emulated.
    assert!(hardware_wifi("Hardware Port: Wi-Fi\nDevice: en0\n", "en0"));
    assert!(!hardware_wifi(
        "Hardware Port: Ethernet\nDevice: en0\n",
        "en0"
    ));
}

#[test]
fn guest_link_claims_are_not_physical_capacity() {
    let mut nic = proto::NetworkInterface {
        interface_kind: "ethernet".into(),
        visibility: "guest".into(),
        physical_medium: "copper".into(),
        link_speed_bps: Some(10_000_000_000),
        ..Default::default()
    };
    suppress_guest_physical_claims(&mut nic);
    assert_eq!(nic.physical_medium, "unknown");
    assert_eq!(nic.link_speed_bps, None);
    let old = json(Some(proto::NetworkInterface::default()));
    assert_eq!(old["interface_kind"], "unknown");
    assert_eq!(old["physical_medium"], "unknown");
}

#[test]
fn rates_are_positive_and_checked() {
    assert_eq!(mbps("1000"), Some(1_000_000_000));
    for value in ["-1", "0", "unknown", "18446744073709551615"] {
        assert_eq!(mbps(value), None);
    }
}

#[test]
fn only_negotiated_media_is_reported() {
    assert_eq!(
        media_speed("media: autoselect (1000baseT <full-duplex>)"),
        Some(1_000_000_000)
    );
    assert_eq!(
        media_speed("media: autoselect (10Gbase-T <full-duplex>)"),
        Some(10_000_000_000)
    );
    assert_eq!(media_speed("media: autoselect"), None);
    assert_eq!(media_speed("media: autoselect (none)"), None);
}

#[test]
fn tcp_probe_uses_connected_source_address() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let result = probe(listener.local_addr().unwrap()).unwrap();
    assert!(result["tcp_connect_rtt_ms"].as_f64().unwrap() >= 0.0);
    assert_eq!(result["local_interface"]["address"], "127.0.0.1");
}
