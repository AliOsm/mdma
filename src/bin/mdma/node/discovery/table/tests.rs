use super::*;
use serde_json::json;

#[test]
fn unknowns_do_not_turn_into_zeroes_or_catalog_link_rates() {
    let report = json!({"node_name":"test","status":"online","last_success_age_ms":2000,
        "resources":{"network_interface":{"name":"en0","physical_medium":"wireless",
            "hardware_capabilities":{"model":"card","max_phy_bps":1000000000}},
            "network_observation":{"tcp_connect_rtt_ms":1.25}}});
    let table = render(&[report]);
    assert!(table.contains("1.0G"));
    assert!(table.contains("1.25"));
    assert!(table.contains("2.0s"));
    assert!(table.contains('?'));
    assert!(!table.contains("0.0G"));
}

#[test]
fn retained_values_are_marked_and_controls_are_removed() {
    let report = json!({"node_name":"evil\u{1b}[2J\nhost", "status":"unreachable",
        "last_success_age_ms":15000,"resources":{"logical_cpu_count":8,
            "memory_available_bytes":1073741824u64,"memory_total_bytes":2147483648u64}});
    let table = render(&[report]);
    assert!(table.contains("~8"));
    assert!(table.contains("~1.0/2.0G"));
    assert!(table.contains("15.0s"));
    assert!(!table.contains('\u{1b}'));
    assert!(!table.contains("\nhost"));
}

#[test]
fn bridge_members_are_not_summed() {
    let resources = json!({"network_interface":{"name":"br0","underlying_interfaces":[
        {"name":"eth0","link_speed_bps":1000000000u64},
        {"name":"eth1","link_speed_bps":1000000000u64}]}});
    let (nic, name) = nic(&resources);
    assert_eq!(name, "br0+2members");
    assert!(nic["link_speed_bps"].is_null());
}

#[test]
fn rejected_identity_and_reason_are_visible() {
    let report = json!({"node_name":"old","status":"rejected",
        "identity":{"mdma_version":"0.0.9","protocol_version":2,"build_id":"old-build"},
        "error":"MDMA outdated"});
    let table = render(&[report]);
    assert!(table.contains("0.0.9"));
    assert!(table.contains("old-build"));
    assert!(table.contains("Rejected: MDMA outdated"));
}
