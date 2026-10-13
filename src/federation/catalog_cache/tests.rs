use super::super::config::Window;
use super::super::signing::KeyPairHandle;
use super::super::tls::tests::Fixture;
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

fn setup() -> (Fixture, Binding, KeyPairHandle, Cache) {
    let f = Fixture::new();
    let key = KeyPairHandle::generate().unwrap();
    let mut policy = f
        .remote_store
        .snapshot()
        .unwrap()
        .peer("local.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.signing_keys[0].key_id = "ed25519:catalog".into();
    policy.signing_keys[0].public_key = URL_SAFE_NO_PAD.encode(key.public_key_bytes());
    f.remote_store.approve_policy(policy).unwrap();
    let (_, binding) = f.bindings();
    (f, binding, key, Cache::default())
}

fn wire(poll: &Poll, key: &KeyPairHandle, now_ms: u64, lifetime_ms: u64) -> SignedCatalog {
    let body = CatalogBody {
        origin: "local.mdma.test".into(),
        destination: "remote.mdma.test".into(),
        nonce: poll.nonce().to_vec(),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + lifetime_ms,
        server_state: catalog::ServerState::Serving,
        host: crate::grpc::Host::default(),
        models: vec![],
    };
    let envelope = catalog::sign_catalog(&body, "ed25519:catalog", key).unwrap();
    SignedCatalog {
        key_id: envelope.key_id,
        body_json: envelope.body_json,
        signature: envelope.signature.to_vec(),
    }
}

#[test]
fn one_outstanding_poll_and_cancellation_consumes_nonce() {
    let (f, binding, _key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let nonce = *poll.nonce();
    assert!(
        cache
            .begin(&f.remote_store, &binding, f.now_ms, now)
            .is_err()
    );
    drop(poll);
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    assert_ne!(poll.nonce(), &nonce);
}

#[test]
fn dialing_budget_caps_challenge_and_runtime_invalidation_consumes_it() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    assert!(
        cache
            .begin_until(&f.remote_store, &binding, f.now_ms, now, now)
            .is_err()
    );
    let end = now + Duration::from_millis(100);
    let poll = cache
        .begin_until(&f.remote_store, &binding, f.now_ms, now, end)
        .unwrap();
    assert_eq!(poll.deadline(), end);
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    assert!(poll.finish(&f.remote_store, reply, f.now_ms, end).is_err());
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    cache.invalidate_all().unwrap();
    assert!(poll.finish(&f.remote_store, reply, f.now_ms, now).is_err());
    assert!(
        cache
            .snapshot(&f.remote_store, binding.server_id(), f.now_ms, now)
            .unwrap()
            .is_none()
    );
}

#[test]
fn verification_crossing_deadline_or_expiry_does_not_replace_good_cache() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    let good = poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    assert!(
        poll.finish_at(&f.remote_store, reply, f.now_ms, now, || (
            f.now_ms + 2000,
            now + POLL_TIMEOUT
        ))
        .is_err()
    );
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 1);
    assert!(
        poll.finish_at(&f.remote_store, reply, f.now_ms, now, || (
            f.now_ms + 1,
            now + Duration::from_millis(1)
        ))
        .is_err()
    );
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    let mut reads = 0;
    assert!(
        poll.finish_at(&f.remote_store, reply, f.now_ms, now, || {
            reads += 1;
            if reads == 1 {
                (f.now_ms, now)
            } else {
                (f.now_ms + 2000, now + POLL_TIMEOUT)
            }
        })
        .is_err()
    );
    assert_eq!(
        cache
            .snapshot(&f.remote_store, binding.server_id(), f.now_ms, now)
            .unwrap()
            .unwrap()
            .nonce,
        good.nonce
    );
}

#[test]
fn challenge_timeout_late_response_and_trust_change_are_rejected() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    assert!(
        poll.finish(&f.remote_store, reply, f.now_ms + 2000, now + POLL_TIMEOUT)
            .is_err()
    );
    let old = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&old, &key, f.now_ms, 60_000);
    let new = cache
        .begin(
            &f.remote_store,
            &binding,
            f.now_ms + 2100,
            now + Duration::from_millis(2100),
        )
        .unwrap();
    assert!(
        old.finish(
            &f.remote_store,
            reply,
            f.now_ms + 2100,
            now + Duration::from_millis(2100)
        )
        .is_err()
    );
    drop(new);
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    f.remote_store.suspend("local.mdma.test").unwrap();
    assert!(poll.finish(&f.remote_store, reply, f.now_ms, now).is_err());
}

#[test]
fn invalid_reply_never_replaces_good_cache_and_empty_snapshot_is_verified() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    let body = poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    assert!(body.models.is_empty());
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let mut reply = wire(&poll, &key, f.now_ms, 60_000);
    reply.signature[0] ^= 1;
    assert!(poll.finish(&f.remote_store, reply, f.now_ms, now).is_err());
    assert!(
        cache
            .snapshot(&f.remote_store, "local.mdma.test", f.now_ms, now)
            .unwrap()
            .is_some()
    );
    assert!(
        cache
            .snapshot(
                &f.remote_store,
                "local.mdma.test",
                f.now_ms,
                now + Duration::from_secs(61)
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn key_window_caps_monotonic_lifetime_and_rollback_cannot_revive_cache() {
    let (f, binding, key, cache) = setup();
    let mut policy = f
        .remote_store
        .snapshot()
        .unwrap()
        .peer("local.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.signing_keys[0].window = Window {
        valid_from_ms: f.now_ms - 1000,
        valid_until_ms: f.now_ms + 5000,
    };
    f.remote_store.approve_policy(policy).unwrap();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    assert!(
        cache
            .snapshot(
                &f.remote_store,
                "local.mdma.test",
                f.now_ms - 1,
                now + Duration::from_secs(6)
            )
            .unwrap()
            .is_none()
    );
    assert!(
        cache
            .snapshot(&f.remote_store, "local.mdma.test", f.now_ms, now)
            .unwrap()
            .is_none()
    );
}

#[test]
fn generation_change_invalidates_cached_routes_even_without_peer_suspension() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    let policy = f
        .remote_store
        .snapshot()
        .unwrap()
        .peer("local.mdma.test")
        .unwrap()
        .policy
        .clone();
    f.remote_store.approve_policy(policy).unwrap();
    assert!(
        cache
            .snapshot(&f.remote_store, "local.mdma.test", f.now_ms, now)
            .unwrap()
            .is_none()
    );
}

#[test]
fn outbound_poll_permission_does_not_grant_remote_access_to_local_catalog() {
    let (f, binding, key, cache) = setup();
    let mut policy = f
        .remote_store
        .snapshot()
        .unwrap()
        .peer("local.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.control_permissions.scan_capabilities = false;
    f.remote_store.approve_policy(policy).unwrap();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    assert!(
        f.remote_store
            .authorize("local.mdma.test", Action::ScanCapabilities, f.now_ms)
            .is_err()
    );
}

#[test]
fn a_new_empty_snapshot_withdraws_previously_advertised_models() {
    let (f, binding, key, cache) = setup();
    let now = Instant::now();
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let body = CatalogBody {
        origin: "local.mdma.test".into(),
        destination: "remote.mdma.test".into(),
        nonce: poll.nonce().to_vec(),
        issued_at_ms: f.now_ms,
        expires_at_ms: f.now_ms + 60_000,
        server_state: catalog::ServerState::Serving,
        host: crate::grpc::Host {
            name: "citadel".into(),
            os: "freebsd".into(),
            os_name: "FreeBSD 16.0-CURRENT".into(),
            ..Default::default()
        },
        models: vec![catalog::ModelDescriptor {
            model: "model".into(),
            kind: catalog::Kind::Gguf,
            artifact_id: format!("sha256:{}", "ab".repeat(32)),
            profile_id: format!("sha256:{}", "cd".repeat(32)),
            architecture: "llama".into(),
            backend: "cpu".into(),
            ready: true,
            max_input_bytes: 1024,
            class: crate::registry::Class::Decision,
            capabilities: vec![crate::registry::Capability::Reasoning],
            resident: true,
            limits: catalog::Limits::Synthesize {
                n_ctx: 64,
                max_output_tokens: 32,
            },
            parameters: 2648,
            quantization: "F32".into(),
            trained_ctx: 64,
        }],
    };
    let envelope = catalog::sign_catalog(&body, "ed25519:catalog", &key).unwrap();
    poll.finish(
        &f.remote_store,
        SignedCatalog {
            key_id: envelope.key_id,
            body_json: envelope.body_json,
            signature: envelope.signature.to_vec(),
        },
        f.now_ms,
        now,
    )
    .unwrap();
    assert_eq!(
        cache
            .snapshot(&f.remote_store, "local.mdma.test", f.now_ms, now)
            .unwrap()
            .unwrap()
            .models
            .len(),
        1
    );
    let topology = crate::federation::topology::Topology::new(
        f.remote_store.clone(),
        Some(cache.clone()),
        vec![],
    )
    .snapshot();
    let peer = &topology.peers[0];
    assert_eq!(
        peer.host.as_ref().map(|h| h.os_name.as_str()),
        Some("FreeBSD 16.0-CURRENT"),
        "operators see the peer's host"
    );
    let model = &peer.catalog.as_ref().unwrap().models[0];
    assert_eq!(
        (model.class.as_str(), model.capabilities.as_slice()),
        ("decision", ["reasoning".to_owned()].as_slice())
    );
    assert_eq!(
        (
            model.parameters,
            model.quantization.as_str(),
            model.trained_ctx
        ),
        (2648, "F32", 64)
    );
    let poll = cache
        .begin(&f.remote_store, &binding, f.now_ms, now)
        .unwrap();
    let reply = wire(&poll, &key, f.now_ms, 60_000);
    poll.finish(&f.remote_store, reply, f.now_ms, now).unwrap();
    assert!(
        cache
            .snapshot(&f.remote_store, "local.mdma.test", f.now_ms, now)
            .unwrap()
            .unwrap()
            .models
            .is_empty()
    );
}
