use super::*;

#[test]
fn ids_are_normalized_not_guessed() {
    assert_eq!(id("0x14E4", 4).as_deref(), Some("14e4"));
    assert_eq!(id("0xf", 2).as_deref(), Some("0f"));
    for value in ["", "junk", "0x12345", "0x-1"] {
        assert!(id(value, 4).is_none());
    }
}

#[test]
fn airport_uses_separate_bus_and_discards_network_information() {
    let value = serde_json::json!({"SPAirPortDataType": [{
        "spairport_airport_interfaces": [{"_name": "en0",
            "spairport_wireless_card_type": "spairport_wireless_card_type_wifi (0x14E4, 0x4378)",
            "spairport_airport_other_local_wireless_networks": [{"_name": "private"}]}]
    }]});
    let cards = airport(&value);
    assert_eq!(cards["en0"].bus, "apple_airport");
    assert_eq!(cards["en0"].device, "4378");
    assert_eq!(cards.len(), 1);
}

#[test]
fn freebsd_selects_exact_device() {
    let line = "re0@pci0:1:0:0: class=0x020000 rev=0x15 hdr=0x00 vendor=0x10ec device=0x8168 subvendor=0x10ec subdevice=0x0123";
    let identity = freebsd_pci(line, "re0").unwrap();
    assert_eq!(identity.device, "8168");
    assert_eq!(identity.subsystem_device, "0123");
    assert!(freebsd_pci(line, "re").is_none());
    assert!(freebsd_pci(line, "wlan0").is_none());
}
