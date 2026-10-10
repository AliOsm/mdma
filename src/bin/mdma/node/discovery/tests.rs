use super::*;

fn ad() -> Advertisement {
    Advertisement {
        name: "test".into(),
        instance: "boot-one".into(),
        endpoints: vec!["http://192.0.2.10:50052".into()],
        identity: Some(version::local()),
        rejection: None,
    }
}

#[test]
fn withdrawn_peer_rejects_in_flight_result_and_can_recover() {
    let now = Instant::now();
    let mut peer = Peer::new(ad(), now);
    peer.removed(now);
    assert!(!peer.complete(0, Ok(json!({"memory_total_bytes": 1})), now));
    assert_eq!(peer.report("test", now)["status"], "offline");
    peer.resolved(ad(), now);
    assert!(peer.complete(peer.generation, Ok(json!({"memory_total_bytes": 2})), now));
    assert_eq!(peer.report("test", now)["status"], "online");
}

#[test]
fn failed_rpc_keeps_last_snapshot_but_marks_it_unreachable_with_age() {
    let now = Instant::now();
    let mut peer = Peer::new(ad(), now);
    peer.complete(0, Ok(json!({"memory_total_bytes": 123})), now);
    peer.complete(0, Err("connection refused".into()), now + POLL);
    let report = peer.report("test", now + POLL);
    assert_eq!(report["status"], "unreachable");
    assert_eq!(report["resources"]["memory_total_bytes"], 123);
    assert_eq!(report["last_success_age_ms"], 10000);
}

#[test]
fn only_usable_unicast_addresses() {
    for ip in [
        "127.0.0.1",
        "0.0.0.0",
        "::",
        "::1",
        "fe80::1",
        "224.0.0.251",
        "255.255.255.255",
    ] {
        assert!(!usable_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["192.0.2.10", "fd00::42"] {
        assert!(usable_ip(ip.parse().unwrap()), "{ip}");
    }
}

fn service(protocol: &str, instance: &str) -> ResolvedService {
    ServiceInfo::new(
        SERVICE_TYPE,
        "test",
        "test.local.",
        "192.0.2.10",
        50052,
        &[
            ("poc", protocol),
            ("node", "test"),
            ("instance", instance),
            ("version", env!("CARGO_PKG_VERSION")),
            ("build", env!("MDMA_BUILD_ID")),
            ("protocol", "2"),
        ][..],
    )
    .unwrap()
    .as_resolved_service()
}

#[test]
fn ignores_self_and_incompatible_protocol_but_accepts_peer() {
    let policy = version::Policy::new(None, None).unwrap();
    assert!(advertisement(&service(PROTOCOL, "own"), Some("own"), &policy).is_none());
    assert!(advertisement(&service("other-protocol", "peer"), Some("own"), &policy).is_none());
    let ad = advertisement(&service(PROTOCOL, "peer"), Some("own"), &policy).unwrap();
    assert!(ad.rejection.is_none());
    let legacy = advertisement(&service("resources-1", "peer"), Some("own"), &policy).unwrap();
    assert!(legacy.rejection.is_some());
    assert_eq!(ad.name, "test");
    assert_eq!(ad.endpoints, ["http://192.0.2.10:50052"]);
}

#[test]
fn unchanged_announcement_does_not_cancel_query_or_force_repoll() {
    let now = Instant::now();
    let mut peer = Peer::new(ad(), now);
    peer.in_flight = true;
    peer.resolved(ad(), now + POLL);
    assert!(peer.in_flight);
    assert_eq!(peer.generation, 0);
    peer.complete(0, Ok(json!({})), now + POLL);
    peer.resolved(ad(), now + POLL);
    assert_eq!(peer.next_poll, now + POLL + POLL);
}

#[test]
fn restart_rejects_result_from_previous_process_even_at_same_endpoint() {
    let now = Instant::now();
    let mut peer = Peer::new(ad(), now);
    let mut restarted = ad();
    restarted.instance = "boot-two".into();
    peer.resolved(restarted, now);
    assert!(!peer.complete(0, Ok(json!({"old": true})), now));
    assert!(peer.complete(peer.generation, Ok(json!({"new": true})), now));
}

#[test]
fn restart_keeps_history_but_does_not_claim_it_is_current() {
    let now = Instant::now();
    let mut peer = Peer::new(ad(), now);
    assert!(peer.complete(0, Ok(json!({"memory_total_bytes": 123})), now));
    let mut restarted = ad();
    restarted.instance = "new-boot".into();
    peer.resolved(restarted, now + POLL);
    let report = peer.report("test", now + POLL);
    assert_eq!(report["status"], "discovered");
    assert_eq!(report["resources"]["memory_total_bytes"], 123);
    assert_eq!(report["last_success_age_ms"], 10000);
    assert!(peer.complete(
        peer.generation,
        Ok(json!({"memory_total_bytes": 456})),
        now + POLL
    ));
    assert_eq!(peer.report("test", now + POLL)["status"], "online");
}

#[test]
fn rejected_peer_is_visible_and_upgrade_can_recover() {
    let now = Instant::now();
    let mut legacy = ad();
    legacy.rejection = Some("legacy protocol".into());
    let mut peer = Peer::new(legacy, now);
    assert!(peer.rejected);
    assert_eq!(peer.report("test", now)["status"], "rejected");
    peer.resolved(ad(), now);
    assert!(!peer.rejected);
    assert_eq!(peer.report("test", now)["status"], "discovered");
    assert!(peer.complete(peer.generation, Ok(json!({})), now));
    assert_eq!(peer.report("test", now)["status"], "online");
    peer.complete(
        peer.generation,
        Err(version::QueryError::Rejected {
            reason: "RPC mismatch".into(),
            identity: None,
        }),
        now,
    );
    assert_eq!(peer.report("test", now)["status"], "rejected");
}
