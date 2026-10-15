use super::super::config::tests::peer;
use super::super::routing::Route;
use super::*;

#[test]
fn snapshot_lists_peers_and_routes_and_follows_trust_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::initialize(&dir.path().join("trust.json"), "local.mdma.test").unwrap());
    store.stage(peer()).unwrap();
    let route = Route {
        alias: "big".into(),
        peer_id: "remote.mdma.test".into(),
        model: "big".into(),
        artifact_id: format!("sha256:{}", "ab".repeat(32)),
        profile_id: format!("sha256:{}", "cd".repeat(32)),
        operation: Operation::Synthesize,
    };
    let routes = crate::federation::routing::RouteTable::new(vec![route], |_| false).unwrap();
    let topology = Topology::new(store.clone(), Some(Cache::default()), routes);
    let snapshot = topology.snapshot();
    assert!(snapshot.authorized && snapshot.enabled);
    assert_eq!(snapshot.server_id, "local.mdma.test");
    let [peer] = snapshot.peers.as_slice() else {
        panic!("one peer: {snapshot:?}")
    };
    assert_eq!(
        (
            peer.peer_id.as_str(),
            peer.state.as_str(),
            peer.endpoint.as_str()
        ),
        ("remote.mdma.test", "staged", "https://127.0.0.1:9443")
    );
    assert!(peer.catalog.is_none(), "nothing polled, nothing cached");
    assert_eq!(
        snapshot.routes,
        [FederationRoute {
            alias: "big".into(),
            peer_id: "remote.mdma.test".into(),
            model: "big".into(),
            operation: "synthesize".into()
        }]
    );

    let mut changes = topology.subscribe();
    changes.mark_unchanged();
    store.revoke("remote.mdma.test").unwrap();
    assert!(changes.has_changed().unwrap(), "trust changes wake Watch");
    let revoked = topology.snapshot();
    assert_eq!(revoked.peers[0].state, "revoked");
    assert!(revoked.trust_generation > snapshot.trust_generation);
}
