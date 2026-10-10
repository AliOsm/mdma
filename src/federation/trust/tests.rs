use super::super::config::tests::peer;
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    (dir, store)
}

fn activate(store: &Store) {
    store.stage(peer()).unwrap();
    store
        .activate_control(
            "remote.mdma.test",
            ControlPermissions {
                identify: true,
                scan_capabilities: true,
            },
        )
        .unwrap();
}

#[test]
fn staged_enrollment_erases_proposed_grants() {
    let (_dir, store) = store();
    let mut p = peer();
    p.control_permissions = ControlPermissions {
        identify: true,
        scan_capabilities: true,
    };
    p.catalog_exports.push("model".into());
    p.execute_exports.push(config::ExecuteExport {
        model: "model".into(),
        operation: Operation::Synthesize,
    });
    store.stage(p).unwrap();
    assert!(
        store
            .authorize("remote.mdma.test", Action::Identify, 200)
            .is_err()
    );
    assert!(
        store
            .authorize("remote.mdma.test", Action::ScanCapabilities, 200)
            .is_err()
    );
    store.allow_staged_identify("remote.mdma.test").unwrap();
    store
        .authorize("remote.mdma.test", Action::Identify, 200)
        .unwrap();
    assert!(
        store
            .authorize("remote.mdma.test", Action::ScanCapabilities, 200)
            .is_err()
    );
    assert!(
        store
            .authorize(
                "remote.mdma.test",
                Action::Execute {
                    model: "model",
                    operation: Operation::Synthesize
                },
                200
            )
            .is_err()
    );
}

#[test]
fn initial_activation_only_grants_control() {
    let (_dir, store) = store();
    activate(&store);
    store
        .authorize("remote.mdma.test", Action::ScanCapabilities, 200)
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let record = snapshot.peer("remote.mdma.test").unwrap();
    assert_eq!(record.state, State::Active);
    assert!(record.policy.catalog_exports.is_empty());
    assert!(record.policy.execute_exports.is_empty());
}

#[test]
fn revocation_survives_restart_and_recovery_clears_grants() {
    let (dir, store) = store();
    activate(&store);
    store.revoke("remote.mdma.test").unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(
        store
            .authorize("remote.mdma.test", Action::Identify, 200)
            .is_err()
    );
    assert!(
        store
            .activate_control("remote.mdma.test", ControlPermissions::default())
            .is_err()
    );
    store.stage(peer()).unwrap();
    assert_eq!(
        store
            .snapshot()
            .unwrap()
            .peer("remote.mdma.test")
            .unwrap()
            .state,
        State::Staged
    );
    assert!(
        store
            .authorize("remote.mdma.test", Action::Identify, 200)
            .is_err()
    );
}

#[test]
fn compromised_material_cannot_be_relabeled_or_reenrolled() {
    let (_dir, store) = store();
    activate(&store);
    let p = peer();
    store.revoke_signing(&p.signing_keys[0].public_key).unwrap();
    assert!(
        store
            .authorize(&p.server_id, Action::Identify, 200)
            .is_err()
    );
    let mut relabeled = p.clone();
    relabeled.signing_keys[0].key_id = "ed25519:other".into();
    relabeled.server_id = "other.mdma.test".into();
    assert!(store.stage(relabeled).is_err());
    store.revoke_tls(&p.tls_pins[0].spki_sha256).unwrap();
    let mut replacement = p;
    replacement.signing_keys[0].public_key = URL_SAFE_NO_PAD.encode([8; 32]);
    replacement.signing_keys[0].key_id = "ed25519:new".into();
    assert!(store.stage(replacement).is_err());
}

#[test]
fn signing_id_history_is_immutable_after_removal_and_restart() {
    let (dir, store) = store();
    activate(&store);
    let mut p = peer();
    p.signing_keys.clear();
    store.approve_policy(p).unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    let mut p = peer();
    p.signing_keys[0].public_key = URL_SAFE_NO_PAD.encode([8; 32]);
    assert!(store.approve_policy(p).is_err());
}

#[test]
fn invalid_candidate_does_not_advance_generation_or_publish() {
    let (_dir, store) = store();
    activate(&store);
    let before = store.snapshot().unwrap().generation;
    let mut p = peer();
    p.endpoint = "http://remote.mdma.test:443".into();
    assert!(store.approve_policy(p).is_err());
    assert_eq!(store.snapshot().unwrap().generation, before);
    store
        .authorize("remote.mdma.test", Action::Identify, 200)
        .unwrap();
}

#[test]
fn removed_grants_are_checked_again_without_reusing_old_snapshot() {
    let (_dir, store) = store();
    activate(&store);
    let old = store
        .authorize("remote.mdma.test", Action::Identify, 200)
        .unwrap();
    let mut notify = store.subscribe();
    store.suspend("remote.mdma.test").unwrap();
    assert!(notify.has_changed().unwrap());
    assert!(*notify.borrow_and_update() > old.generation);
    assert!(
        store
            .authorize("remote.mdma.test", Action::Identify, 200)
            .is_err()
    );
}

#[test]
fn signer_retirement_and_clock_floor_survive_reload() {
    let (dir, store) = store();
    activate(&store);
    store
        .signing_key("remote.mdma.test", "ed25519:first", 999)
        .unwrap();
    assert!(
        store
            .signing_key("remote.mdma.test", "ed25519:first", 1000)
            .is_err()
    );
    assert!(
        store
            .signing_key("remote.mdma.test", "ed25519:first", 999)
            .is_err()
    );
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(
        store
            .signing_key("remote.mdma.test", "ed25519:first", 999)
            .is_err()
    );
    assert!(store.approve_policy(peer()).is_err());
}

#[test]
fn lost_wrong_identity_or_corrupt_state_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trust.json");
    assert!(Store::open(&path, "local.mdma.test").is_err());
    let store = Store::initialize(&path, "local.mdma.test").unwrap();
    drop(store);
    assert!(Store::open(&path, "other.mdma.test").is_err());
    std::fs::write(&path, "{}").unwrap();
    assert!(Store::open(&path, "local.mdma.test").is_err());
    assert!(Store::initialize(&path, "local.mdma.test").is_err());
}

#[test]
fn one_store_writer_and_restart_releases_lock() {
    let (dir, store) = store();
    let path = dir.path().join("trust.json");
    assert!(Store::open(&path, "local.mdma.test").is_err());
    drop(store);
    Store::open(&path, "local.mdma.test").unwrap();
}

#[test]
fn peer_self_identity_is_rejected_without_publication() {
    let (_dir, store) = store();
    let mut p = peer();
    p.server_id = "local.mdma.test".into();
    assert!(store.stage(p).is_err());
    assert!(store.snapshot().unwrap().peers.is_empty());
}

#[test]
fn removed_signer_and_pin_windows_still_retire() {
    let (dir, store) = store();
    activate(&store);
    let mut p = peer();
    p.signing_keys.clear();
    p.tls_pins.clear();
    store.approve_policy(p).unwrap();
    store.observe_time(2000).unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(store.approve_policy(peer()).is_err());
    assert!(
        store
            .authorize("remote.mdma.test", Action::Identify, 1999)
            .is_err()
    );
}

#[test]
fn material_denial_and_id_history_survive_restart() {
    let (dir, store) = store();
    activate(&store);
    let p = peer();
    store.revoke_tls(&p.tls_pins[0].spki_sha256).unwrap();
    store.revoke_signing(&p.signing_keys[0].public_key).unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(store.stage(p).is_err());
}

#[test]
fn explicit_execution_grant_never_implies_another_operation() {
    let (_dir, store) = store();
    activate(&store);
    let mut p = peer();
    p.execute_exports.push(config::ExecuteExport {
        model: "model".into(),
        operation: Operation::Synthesize,
    });
    store.approve_policy(p.clone()).unwrap();
    store
        .authorize(
            &p.server_id,
            Action::Execute {
                model: "model",
                operation: Operation::Synthesize,
            },
            200,
        )
        .unwrap();
    assert!(
        store
            .authorize(
                &p.server_id,
                Action::Execute {
                    model: "model",
                    operation: Operation::Decide
                },
                200
            )
            .is_err()
    );
    p.execute_exports.clear();
    store.approve_policy(p).unwrap();
    assert!(
        store
            .authorize(
                "remote.mdma.test",
                Action::Execute {
                    model: "model",
                    operation: Operation::Synthesize
                },
                200
            )
            .is_err()
    );
}

#[test]
fn local_signer_retirement_is_persistent_and_has_no_fallback() {
    let (dir, store) = store();
    let key = peer().signing_keys[0].clone();
    store.approve_local_signer(key.clone()).unwrap();
    store.local_signer(&key.key_id, 999).unwrap();
    assert!(store.local_signer(&key.key_id, 1000).is_err());
    drop(store);
    let store = Store::open(&dir.path().join("trust.json"), "local.mdma.test").unwrap();
    assert!(store.local_signer(&key.key_id, 999).is_err());
    assert!(store.approve_local_signer(key).is_err());
}
