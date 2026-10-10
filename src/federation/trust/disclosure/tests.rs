use super::*;

fn fixture() -> (tempfile::TempDir, Store, Disclosure) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    store
        .stage(crate::federation::config::tests::peer())
        .unwrap();
    store
        .activate_control("remote.mdma.test", ControlPermissions::default())
        .unwrap();
    let grant = Disclosure {
        caller_spki_sha256: format!("sha256:{}", "ab".repeat(32)),
        peer_id: "remote.mdma.test".into(),
        model: "model".into(),
        operation: Operation::Decide,
        window: config::Window {
            valid_from_ms: 100,
            valid_until_ms: 900,
        },
    };
    (dir, store, grant)
}

fn allowed(store: &Store, grant: &Disclosure, time: u64) -> bool {
    store
        .disclosure_window(
            &grant.caller_spki_sha256,
            &grant.peer_id,
            &grant.model,
            grant.operation,
            time,
        )
        .is_ok()
}

#[test]
fn consent_is_exact_current_persisted_and_never_an_import() {
    let (dir, store, grant) = fixture();
    assert!(!allowed(&store, &grant, 200));
    let mut changes = store.subscribe();
    store.approve_disclosure(grant.clone()).unwrap();
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert!(allowed(&store, &grant, 200));
    for changed in [
        Disclosure {
            operation: Operation::Synthesize,
            ..grant.clone()
        },
        Disclosure {
            model: "other".into(),
            ..grant.clone()
        },
        Disclosure {
            caller_spki_sha256: format!("sha256:{}", "cd".repeat(32)),
            ..grant.clone()
        },
        Disclosure {
            peer_id: "other.mdma.test".into(),
            ..grant.clone()
        },
    ] {
        assert!(!allowed(&store, &changed, 200));
    }
    assert!(
        store
            .authorize(
                &grant.peer_id,
                Action::Import {
                    model: &grant.model,
                    artifact_id: "unused",
                    profile_id: "unused",
                    operation: grant.operation,
                },
                200
            )
            .is_err()
    );
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(allowed(&store, &grant, 200));
    store
        .remove_disclosure(
            &grant.caller_spki_sha256,
            &grant.peer_id,
            &grant.model,
            grant.operation,
        )
        .unwrap();
    assert!(!allowed(&store, &grant, 200));
}

#[test]
fn caller_key_compromise_is_durable_and_cannot_be_relabeled() {
    let (dir, store, grant) = fixture();
    store.approve_disclosure(grant.clone()).unwrap();
    store.revoke_tls(&grant.caller_spki_sha256).unwrap();
    assert!(!allowed(&store, &grant, 200));
    assert!(store.approve_disclosure(grant.clone()).is_err());
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(
        store
            .approve_disclosure(Disclosure {
                model: "new".into(),
                ..grant.clone()
            })
            .is_err()
    );
    assert!(!allowed(&store, &grant, 200));
}

#[test]
fn expiry_cannot_be_reversed_by_clock_rollback_or_restart() {
    let (dir, store, grant) = fixture();
    store.approve_disclosure(grant.clone()).unwrap();
    assert!(!allowed(&store, &grant, 99));
    assert!(allowed(&store, &grant, 899));
    assert!(!allowed(&store, &grant, 900));
    assert!(!allowed(&store, &grant, 899));
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(!allowed(&store, &grant, 899));
    assert!(store.approve_disclosure(grant).is_err());
}

#[test]
fn suspended_peer_recovery_does_not_inherit_disclosure() {
    let (_dir, store, grant) = fixture();
    store.approve_disclosure(grant.clone()).unwrap();
    store.suspend(&grant.peer_id).unwrap();
    assert!(store.approve_disclosure(grant.clone()).is_err());
    store
        .stage(crate::federation::config::tests::peer())
        .unwrap();
    store
        .activate_control(&grant.peer_id, ControlPermissions::default())
        .unwrap();
    assert!(!allowed(&store, &grant, 200));
}

#[test]
fn old_state_without_disclosure_field_is_deny_by_default() {
    let (dir, store, grant) = fixture();
    let path = dir.path().join("trust.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("disclosures");
    drop(store);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let store = Store::open(&path, "local.mdma.test").unwrap();
    assert!(!allowed(&store, &grant, 200));
}
