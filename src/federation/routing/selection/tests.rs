use super::*;
use crate::federation::{
    catalog::{self, CatalogBody, Limits, ServerState},
    config::{RouteImport, Window},
    control::SignedCatalog,
    signing::KeyPairHandle,
    tls::tests::Fixture,
    trust::Disclosure,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::extensions::Extensions;

struct Setup {
    f: Fixture,
    cache: Cache,
    route: Route,
    caller: Caller,
    digest: String,
    key: KeyPairHandle,
    binding: Binding,
}
impl Setup {
    fn new(import: bool, consent: bool) -> Self {
        let f = Fixture::new();
        let key = KeyPairHandle::generate().unwrap();
        let route = Route {
            alias: "remote-model".into(),
            peer_id: "local.mdma.test".into(),
            model: "model".into(),
            artifact_id: format!("sha256:{}", "11".repeat(32)),
            profile_id: format!("sha256:{}", "22".repeat(32)),
            operation: Operation::Synthesize,
        };
        let mut policy = f
            .remote_store
            .snapshot()
            .unwrap()
            .peer(&route.peer_id)
            .unwrap()
            .policy
            .clone();
        policy.signing_keys[0].key_id = "ed25519:catalog".into();
        policy.signing_keys[0].public_key = URL_SAFE_NO_PAD.encode(key.public_key_bytes());
        if import {
            policy.route_imports.push(RouteImport {
                model: route.model.clone(),
                artifact_id: route.artifact_id.clone(),
                profile_id: route.profile_id.clone(),
                operation: route.operation,
            });
        }
        f.remote_store.approve_policy(policy).unwrap();
        let (caller, digest) = tls::caller::tests::authenticated();
        if consent {
            f.remote_store
                .approve_disclosure(Disclosure {
                    caller_spki_sha256: digest.clone(),
                    peer_id: route.peer_id.clone(),
                    model: route.model.clone(),
                    operation: route.operation,
                    window: Window {
                        valid_from_ms: f.now_ms - 1000,
                        valid_until_ms: f.now_ms + 100_000,
                    },
                })
                .unwrap();
        }
        let (_, binding) = f.bindings();
        let setup = Self {
            f,
            cache: Cache::default(),
            route,
            caller,
            digest,
            key,
            binding,
        };
        setup.publish(true, ServerState::Serving, 60_000, false);
        setup
    }

    fn publish(&self, ready: bool, state: ServerState, lifetime: u64, changed_artifact: bool) {
        let now = tls::utc_ms().unwrap();
        let poll = self
            .cache
            .begin(&self.f.remote_store, &self.binding, now, Instant::now())
            .unwrap();
        let descriptor = ModelDescriptor {
            model: self.route.model.clone(),
            kind: Kind::Gguf,
            artifact_id: if changed_artifact {
                format!("sha256:{}", "33".repeat(32))
            } else {
                self.route.artifact_id.clone()
            },
            profile_id: self.route.profile_id.clone(),
            architecture: "test".into(),
            backend: "cpu".into(),
            ready,
            max_input_bytes: 65536,
            limits: Limits::Synthesize {
                n_ctx: 32,
                max_output_tokens: 8,
            },
        };
        let body = CatalogBody {
            origin: self.route.peer_id.clone(),
            destination: "remote.mdma.test".into(),
            nonce: poll.nonce().to_vec(),
            issued_at_ms: now,
            expires_at_ms: now + lifetime,
            server_state: state,
            models: vec![descriptor],
        };
        let signed = catalog::sign_catalog(&body, "ed25519:catalog", &self.key).unwrap();
        poll.finish(
            &self.f.remote_store,
            SignedCatalog {
                key_id: signed.key_id,
                body_json: signed.body_json,
                signature: signed.signature.to_vec(),
            },
            now,
            Instant::now(),
        )
        .unwrap();
    }

    fn request(&self, timeout: &str) -> Request<()> {
        let request = Request::new(());
        let transport = Extensions::new();
        transport.insert(self.caller.clone());
        request.extensions().insert(TransportIngress(transport));
        let mut headers = rama::http::HeaderMap::new();
        headers.insert("grpc-timeout", timeout.parse().unwrap());
        request
            .extensions()
            .insert(Receipt(Ingress::capture(&headers)));
        request
    }
    fn select(&self, request: &Request<()>) -> Result<Selection, Status> {
        Selection::new(&self.f.remote_store, &self.cache, &self.route, request)
    }
}

#[test]
fn readiness_requires_actual_caller_consent_import_and_exact_fresh_catalog_not_deadline() {
    let setup = Setup::new(true, true);
    let request = setup.request("invalid");
    assert!(
        Selection::visible(&setup.f.remote_store, &setup.cache, &setup.route, &request).is_ok()
    );
    assert!(
        Selection::visible(
            &setup.f.remote_store,
            &setup.cache,
            &setup.route,
            &Request::new(())
        )
        .is_err()
    );
    for (import, consent) in [(false, true), (true, false)] {
        let setup = Setup::new(import, consent);
        assert!(
            Selection::visible(
                &setup.f.remote_store,
                &setup.cache,
                &setup.route,
                &setup.request("1m")
            )
            .is_err()
        );
    }
    setup.publish(false, ServerState::Serving, 60_000, false);
    assert!(
        Selection::visible(&setup.f.remote_store, &setup.cache, &setup.route, &request).is_err()
    );
}

#[test]
fn explicit_import_and_actual_caller_consent_are_both_mandatory() {
    for (import, consent) in [(false, false), (true, false), (false, true)] {
        let setup = Setup::new(import, consent);
        assert!(setup.select(&setup.request("3000m")).is_err());
    }
    let setup = Setup::new(true, true);
    let first = setup.select(&setup.request("3000m")).unwrap();
    let second = setup.select(&setup.request("3000m")).unwrap();
    assert_ne!(first.envelope.request_id, second.envelope.request_id);
    assert_eq!(first.envelope.origin, "remote.mdma.test");
    assert_eq!(first.envelope.destination, "local.mdma.test");
    assert_eq!(first.envelope.model, "model");
    assert!(first.envelope.timeout_ms <= 3000);
    first
        .dispatch(&setup.f.remote_store, &setup.cache, &setup.binding)
        .unwrap();
}

#[test]
fn stale_unready_changed_and_wrong_operation_catalogs_never_route() {
    let setup = Setup::new(true, true);
    for (ready, state, changed) in [
        (false, ServerState::Serving, false),
        (false, ServerState::Critical, false),
        (false, ServerState::Draining, false),
        (true, ServerState::Serving, true),
    ] {
        setup.publish(ready, state, 60_000, changed);
        assert!(setup.select(&setup.request("3000m")).is_err());
    }
    setup.publish(true, ServerState::Serving, 60_000, false);
    let mut route = setup.route.clone();
    route.operation = Operation::Decide;
    assert!(
        Selection::new(
            &setup.f.remote_store,
            &setup.cache,
            &route,
            &setup.request("3000m")
        )
        .is_err()
    );
    setup.cache.invalidate_all().unwrap();
    assert!(setup.select(&setup.request("3000m")).is_err());
}

#[tokio::test]
async fn receipt_subtracts_routing_time_and_cannot_reset_on_dispatch() {
    let setup = Setup::new(true, true);
    let request = setup.request("80m");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let selected = setup.select(&request).unwrap();
    assert!(selected.envelope.timeout_ms <= 60);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let envelope = selected
        .dispatch(&setup.f.remote_store, &setup.cache, &setup.binding)
        .unwrap();
    assert!(envelope.timeout_ms < selected.envelope.timeout_ms);
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(
        selected
            .dispatch(&setup.f.remote_store, &setup.cache, &setup.binding)
            .is_err()
    );
}

#[test]
fn grant_removal_and_catalog_replacement_prevent_dispatch_without_reselection() {
    let setup = Setup::new(true, true);
    let selected = setup.select(&setup.request("3000m")).unwrap();
    setup.publish(true, ServerState::Serving, 60_000, true);
    assert!(
        selected
            .dispatch(&setup.f.remote_store, &setup.cache, &setup.binding)
            .is_err()
    );
    setup.publish(true, ServerState::Serving, 60_000, false);
    let selected = setup.select(&setup.request("3000m")).unwrap();
    setup
        .f
        .remote_store
        .remove_disclosure(
            &setup.digest,
            &setup.route.peer_id,
            &setup.route.model,
            setup.route.operation,
        )
        .unwrap();
    assert!(
        selected
            .authorize(&setup.f.remote_store, Some(&setup.binding))
            .is_err()
    );
    assert!(
        selected
            .dispatch(&setup.f.remote_store, &setup.cache, &setup.binding)
            .is_err()
    );
}

#[test]
fn absent_or_forged_metadata_principals_and_receipts_do_not_route() {
    let setup = Setup::new(true, true);
    let mut request = Request::new(());
    request
        .metadata_mut()
        .insert("caller-spki", setup.digest.parse().unwrap());
    request
        .metadata_mut()
        .insert("grpc-timeout", "1000m".parse().unwrap());
    assert!(setup.select(&request).is_err());
    let transport = Extensions::new();
    transport.insert(setup.caller.clone());
    request.extensions().insert(TransportIngress(transport));
    assert!(setup.select(&request).is_err());
    assert!(setup.select(&setup.request("0m")).is_err());
}
