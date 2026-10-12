use super::super::config::{SigningKey, Window};
use super::super::tls::tests::Fixture;
use super::*;
use std::assert_matches;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::http::grpc::Code;

fn setup() -> (Fixture, ControlService, Binding) {
    let fixture = Fixture::new();
    let key = KeyPairHandle::generate().unwrap();
    fixture
        .store
        .approve_local_signer(SigningKey {
            key_id: "ed25519:local".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.public_key_bytes()),
            window: Window {
                valid_from_ms: fixture.now_ms - 60_000,
                valid_until_ms: fixture.now_ms + 300_000,
            },
        })
        .unwrap();
    let signer = Signer::new(&fixture.store, "ed25519:local".into(), key).unwrap();
    let (binding, _) = fixture.bindings();
    let service = ControlService::new(fixture.store.clone(), signer);
    (fixture, service, binding)
}

fn request<T>(message: T, binding: &Binding) -> Request<T> {
    let req = Request::new(message);
    let ingress = Extensions::new();
    ingress.insert(binding.clone());
    req.extensions().insert(Ingress(ingress));
    req
}

fn identify() -> IdentifyRequest {
    IdentifyRequest {
        origin: "remote.mdma.test".into(),
        destination: "local.mdma.test".into(),
        supported_major_versions: vec![1],
    }
}

fn scan() -> ScanCapabilitiesRequest {
    ScanCapabilitiesRequest {
        origin: "remote.mdma.test".into(),
        destination: "local.mdma.test".into(),
        major_version: 1,
        nonce: vec![9; 32],
    }
}

fn approve_exports(fixture: &Fixture, exports: &[&str]) {
    let mut policy = fixture.store.snapshot().unwrap()
        .peer("remote.mdma.test").unwrap().policy.clone();
    policy.catalog_exports = exports.iter().map(|name| (*name).into()).collect();
    fixture.store.approve_policy(policy).unwrap();
}

fn verify_reply(service: &ControlService, reply: SignedCatalog) -> CatalogBody {
    let now = utc_ms().unwrap();
    catalog::verify_signed_catalog(
        &catalog::SignedCatalogEnvelope {
            key_id: reply.key_id, body_json: reply.body_json,
            signature: reply.signature.try_into().unwrap(),
        },
        &service.signer.public_key(),
        &catalog::VerifyContext {
            now_ms: now,
            expected_origin: "local.mdma.test",
            expected_destination: "remote.mdma.test",
            expected_nonce: &[9; 32],
            key_valid_from_ms: now - 60_000,
            key_valid_until_ms: now + 300_000,
        },
    ).unwrap()
}

#[tokio::test]
async fn native_catalog_is_recipient_filtered_and_disclosure_revocation_withdraws_it() {
    let (f, service, binding) = setup();
    let native = crate::federation::inventory::tests::native_service();
    let inventory = native.federation_inventory();
    let expected = inventory.snapshot(&["native".into()]);
    let service = service.with_inventory(inventory);
    assert!(verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
        .await.unwrap().into_inner()).models.is_empty());
    approve_exports(&f, &["native", "unknown"]);
    let body = verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
        .await.unwrap().into_inner());
    assert_eq!(body.models, expected.models);
    assert!(f.store.snapshot().unwrap().peer("remote.mdma.test")
        .unwrap().policy.execute_exports.is_empty());
    approve_exports(&f, &[]);
    assert!(verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
        .await.unwrap().into_inner()).models.is_empty());
}

#[tokio::test]
async fn signed_native_catalog_follows_live_power_and_drain_without_changing_profile() {
    use crate::power::{PowerStatus, DegradedLimits, ServerState as PowerState};
    let (f, service, binding) = setup();
    approve_exports(&f, &["native", "alias"]);
    let (tx, rx) = tokio::sync::watch::channel(PowerStatus::default());
    let native = crate::federation::inventory::tests::native_service()
        .with_power(rx, DegradedLimits { n_ctx: 12, max_tokens: 3 });
    let service = service.with_inventory(native.federation_inventory());
    let serving = verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
        .await.unwrap().into_inner());
    for (power, state) in [
        (PowerState::Degraded, ServerState::Degraded),
        (PowerState::Critical, ServerState::Critical),
        (PowerState::Draining, ServerState::Draining),
    ] {
        tx.send(PowerStatus { state: power, reading: None }).unwrap();
        let body = verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
            .await.unwrap().into_inner());
        assert_eq!(body.server_state, state);
        assert_eq!(body.models.len(), 2);
        for (model, original) in body.models.iter().zip(&serving.models) {
            assert_eq!(model.profile_id, original.profile_id);
            assert_eq!(model.artifact_id, original.artifact_id);
            assert_eq!(model.ready, !state.forces_unready());
        }
    }
}

#[tokio::test]
async fn readiness_or_policy_change_during_signing_cannot_publish_a_stale_snapshot() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Changing {
        calls: AtomicUsize,
        store: Option<Arc<Store>>,
    }
    impl Inventory for Changing {
        fn snapshot(&self, _: &[String]) -> inventory::Snapshot {
            let second = self.calls.fetch_add(1, Ordering::SeqCst) > 0;
            if second
                && let Some(store) = &self.store
            {
                store.suspend("remote.mdma.test").unwrap();
            }
            inventory::Snapshot {
                state: if second && self.store.is_none() {
                    ServerState::Critical
                } else { ServerState::Serving },
                models: vec![],
                host: crate::grpc::Host::default(),
            }
        }
    }
    for change_policy in [false, true] {
        let (f, service, binding) = setup();
        let service = service.with_inventory(Arc::new(Changing {
            calls: AtomicUsize::new(0),
            store: change_policy.then(|| f.store.clone()),
        }));
        let result = service.scan_capabilities(request(scan(), &binding)).await.unwrap_err();
        assert_eq!(result.code(), if change_policy { Code::PermissionDenied } else { Code::Aborted });
    }
}

#[tokio::test]
async fn control_only_shutdown_signs_draining_empty_catalog() {
    let (_f, service, binding) = setup();
    let trigger = tokio_util::sync::CancellationToken::new();
    let shutdown = rama::graceful::Shutdown::new(trigger.clone().cancelled_owned());
    let weak = shutdown.guard_weak();
    let service = service.with_shutdown(Some(weak.clone()));
    trigger.cancel();
    weak.cancelled().await;
    let body = verify_reply(&service, service.scan_capabilities(request(scan(), &binding))
        .await.unwrap().into_inner());
    assert_eq!(body.server_state, ServerState::Draining);
    assert!(body.models.is_empty());
}

#[tokio::test]
async fn identify_authenticates_but_never_changes_enrollment_or_grants() {
    let (f, service, binding) = setup();
    let before = f.store.snapshot().unwrap();
    let result = service
        .identify(request(identify(), &binding))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.server_id, "local.mdma.test");
    assert_eq!(result.supported_major_versions, vec![1]);
    let after = f.store.snapshot().unwrap();
    assert_eq!(before.generation, after.generation);
    assert!(
        after
            .peer("remote.mdma.test")
            .unwrap()
            .policy
            .execute_exports
            .is_empty()
    );
}

#[tokio::test]
async fn missing_tls_ingress_or_forged_origin_fails() {
    let (_f, service, binding) = setup();
    assert_eq!(
        service
            .identify(Request::new(identify()))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
    let mut message = identify();
    message.origin = "other.mdma.test".into();
    assert_eq!(
        service
            .identify(request(message, &binding))
            .await
            .unwrap_err()
            .code(),
        Code::Unauthenticated
    );
}

#[tokio::test]
async fn destination_and_version_are_checked_on_every_rpc() {
    let (_f, service, binding) = setup();
    let mut message = identify();
    message.destination = "other.mdma.test".into();
    assert_eq!(
        service
            .identify(request(message, &binding))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    let mut message = identify();
    message.supported_major_versions = vec![2];
    assert_eq!(
        service
            .identify(request(message, &binding))
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut message = scan();
    message.major_version = 2;
    assert_eq!(
        service
            .scan_capabilities(request(message, &binding))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
}

#[tokio::test]
async fn signed_empty_catalog_is_bound_to_recipient_and_challenge() {
    let (_f, service, binding) = setup();
    let reply = service
        .scan_capabilities(request(scan(), &binding))
        .await
        .unwrap()
        .into_inner();
    let envelope = catalog::SignedCatalogEnvelope {
        key_id: reply.key_id,
        body_json: reply.body_json,
        signature: reply.signature.try_into().unwrap(),
    };
    let now = utc_ms().unwrap();
    let context = catalog::VerifyContext {
        now_ms: now,
        expected_origin: "local.mdma.test",
        expected_destination: "remote.mdma.test",
        expected_nonce: &[9; 32],
        key_valid_from_ms: now - 60_000,
        key_valid_until_ms: now + 300_000,
    };
    let catalog =
        catalog::verify_signed_catalog(&envelope, &service.signer.public_key(), &context).unwrap();
    assert!(catalog.models.is_empty());
    assert!(catalog.expires_at_ms - catalog.issued_at_ms <= 60_000);
    let wrong = catalog::VerifyContext {
        expected_nonce: &[8; 32],
        ..context
    };
    assert!(
        catalog::verify_signed_catalog(&envelope, &service.signer.public_key(), &wrong).is_err()
    );
}

#[tokio::test]
async fn removing_scan_grant_or_suspending_peer_denies_existing_connection() {
    let (f, service, binding) = setup();
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.control_permissions.scan_capabilities = false;
    f.store.approve_policy(policy).unwrap();
    assert_eq!(
        service
            .scan_capabilities(request(scan(), &binding))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    service
        .identify(request(identify(), &binding))
        .await
        .unwrap();
    f.store.suspend("remote.mdma.test").unwrap();
    assert_eq!(
        service
            .identify(request(identify(), &binding))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
}

#[tokio::test]
async fn bounded_control_quota_and_exports_never_invent_models() {
    let (f, service, binding) = setup();
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.limits.control_per_minute = 1;
    f.store.approve_policy(policy).unwrap();
    service
        .identify(request(identify(), &binding))
        .await
        .unwrap();
    assert_eq!(
        service
            .identify(request(identify(), &binding))
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    let (f, service, binding) = setup();
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.catalog_exports.push("not-integrated".into());
    f.store.approve_policy(policy).unwrap();
    let reply = service.scan_capabilities(request(scan(), &binding)).await.unwrap().into_inner();
    let body = catalog::CatalogBody::from_canonical(
        &crate::federation::canonical_json::parse(&reply.body_json, catalog::MAX_JSON_DEPTH).unwrap(),
    ).unwrap();
    assert!(body.models.is_empty(), "a grant is not native load provenance");
}

#[tokio::test]
async fn local_signer_compromise_denies_catalog_without_auto_fallback() {
    let (f, service, binding) = setup();
    f.store
        .revoke_signing(&URL_SAFE_NO_PAD.encode(service.signer.public_key()))
        .unwrap();
    assert_eq!(
        service
            .scan_capabilities(request(scan(), &binding))
            .await
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
}

#[test]
fn local_signer_selection_requires_approved_matching_private_material() {
    let f = Fixture::new();
    let key = KeyPairHandle::generate().unwrap();
    assert!(Signer::new(&f.store, "ed25519:unapproved".into(), key).is_err());
    let key = KeyPairHandle::generate().unwrap();
    f.store
        .approve_local_signer(SigningKey {
            key_id: "ed25519:local".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.public_key_bytes()),
            window: Window {
                valid_from_ms: f.now_ms - 1,
                valid_until_ms: f.now_ms + 100_000,
            },
        })
        .unwrap();
    assert!(
        Signer::new(
            &f.store,
            "ed25519:local".into(),
            KeyPairHandle::generate().unwrap()
        )
        .is_err()
    );
    let bytes = key.to_pkcs8().unwrap();
    assert_eq!(
        KeyPairHandle::from_pkcs8(&bytes)
            .unwrap()
            .public_key_bytes(),
        key.public_key_bytes()
    );
}

struct WireTransport(
    rama::service::BoxService<
        rama::http::Request,
        rama::http::Response,
        rama::error::extra::OpaqueError,
    >,
);

#[tokio::test]
async fn actual_mtls_synthesis_requires_deadline_grant_and_exact_selection_then_completes() {
    use rama::{Layer, http::{Body, client::EasyHttpConnectorBuilder, server::HttpServer}, rt::Executor, tcp::server::TcpListener};
    use rama::tls::{client::TlsClientConfig, server::TlsServerConfig};
    use rama::tls::rustls::{client::RustlsClientConfigExt, server::{RustlsServerConfigExt, TlsAcceptorLayer}};
    use crate::federation::{config::{ExecuteExport, Operation}, execution::ingress::{Gate, Connections}};
    let (f, service, _) = setup();
    let native = crate::federation::inventory::tests::native_service();
    let descriptor = native.federation_executor().descriptor("native").unwrap();
    let service = service.with_execution(native.federation_executor());
    let (tls_client, tls_server) = f.configs();
    let executor = Executor::default();
    let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut http = HttpServer::new_h2(executor);
    http.h2_mut().set_max_send_buf_size(1);
    http.h2_mut().set_max_concurrent_streams(2);
    let server = Connections::new(TlsAcceptorLayer::new(TlsServerConfig::new()
        .with_modify_rustls_config(move |_| Ok(tls_server.clone())))
        .into_layer(crate::federation::tls::BindPeerService::new(
            f.store.clone(), f.now_ms + 100_000, http.service(Gate::new(service.into_server())))));
    let task = tokio::spawn(async move { listener.serve(server).await });
    let tls = TlsClientConfig::new()
        .with_server_name(rama::net::address::Host::from_static("local.mdma.test"))
        .with_modify_rustls_config(move |_| Ok(tls_client.clone()));
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector().with_default_dns_connector()
        .without_tls_proxy_support().without_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector::<Body>(Executor::default())
        .without_connection_pool().build_client::<Body, Body, _>();
    let client = FederationClient::new(WireTransport(rama::service::BoxService::new(connector)),
        format!("https://{addr}").parse().unwrap());
    let message = SynthesizeRequest {
        envelope: Some(ExecutionEnvelope { origin: "remote.mdma.test".into(), destination: "local.mdma.test".into(),
            request_id: vec![44; 16], model: "native".into(), artifact_id: descriptor.artifact_id, profile_id: descriptor.profile_id, timeout_ms: 2000 }),
        prompt: "abc".into(), max_tokens: 3, sampler: Some(SamplerConfig { temperature: Some(0.), ..Default::default() }),
    };
    let deadline_request = |message| {
        let mut request = Request::new(message);
        request.try_set_timeout(Duration::from_secs(2)).unwrap();
        request
    };
    assert_matches!(client.synthesize(message.clone()).await, Err(s) if s.code() == Code::InvalidArgument);
    assert_matches!(client.synthesize(deadline_request(message.clone())).await, Err(s) if s.code() == Code::PermissionDenied);
    let mut policy = f.store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    policy.execute_exports = vec![ExecuteExport { model: "native".into(), operation: Operation::Synthesize }];
    f.store.approve_policy(policy).unwrap();
    let mut wrong = message.clone();
    wrong.envelope.as_mut().unwrap().profile_id = format!("sha256:{}", "0".repeat(64));
    wrong.envelope.as_mut().unwrap().request_id = vec![43; 16];
    assert_matches!(client.synthesize(deadline_request(wrong)).await, Err(s) if s.code() == Code::FailedPrecondition);
    let mut stream = client.synthesize(deadline_request(message.clone())).await.unwrap().into_inner();
    let Some(synthesize_event::Event::Accepted(accepted)) = stream.message().await.unwrap().unwrap().event else { panic!("first event"); };
    assert_eq!(accepted.request_id, vec![44; 16]);
    assert_eq!(accepted.sampler.unwrap().temperature, Some(0.));
    let mut count = 0;
    loop {
        let event = stream.message().await.unwrap().unwrap().event.unwrap();
        match event {
            synthesize_event::Event::Token(token) => { assert_eq!(token.index, count); count += 1; }
            synthesize_event::Event::Completed(completed) => { assert_eq!(completed.emitted_tokens, count); break; }
            synthesize_event::Event::Accepted(_) => panic!("duplicate Accepted"),
        }
    }
    assert!(stream.message().await.unwrap().is_none(), "gRPC OK after completion");
    assert_matches!(client.synthesize(deadline_request(message)).await, Err(s) if s.code() == Code::AlreadyExists);
    task.abort();
    let _ = task.await;
}

impl rama::Service<rama::http::Request> for WireTransport {
    type Output = rama::http::Response;
    type Error = Status;

    async fn serve(&self, request: rama::http::Request) -> Result<Self::Output, Self::Error> {
        self.0
            .serve(request)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))
    }
}

#[tokio::test]
async fn actual_http2_mtls_listener_preserves_binding_and_signed_native_catalog() {
    use rama::Layer;
    use rama::http::Body;
    use rama::http::client::EasyHttpConnectorBuilder;
    use rama::http::server::HttpServer;
    use rama::rt::Executor;
    use rama::tcp::server::TcpListener;
    use rama::tls::rustls::client::RustlsClientConfigExt;
    use rama::tls::rustls::server::{RustlsServerConfigExt, TlsAcceptorLayer};
    use rama::tls::{client::TlsClientConfig, server::TlsServerConfig};

    let (f, service, _) = setup();
    approve_exports(&f, &["native"]);
    let native = crate::federation::inventory::tests::native_service();
    let expected = native.federation_inventory().snapshot(&["native".into()]);
    let service = service.with_inventory(native.federation_inventory());
    let signing_public_key = service.signer.public_key();
    let (client, server) = f.configs();
    let grpc = service.into_server();
    let executor = Executor::default();
    let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptorLayer::new(
        TlsServerConfig::new().with_modify_rustls_config(move |_| Ok(server.clone())),
    )
    .into_layer(super::super::tls::BindPeerService::new(
        f.store.clone(),
        f.now_ms + 100_000,
        HttpServer::auto(executor).service(grpc),
    ));
    let task = tokio::spawn(async move { listener.serve(acceptor).await });
    let tls = TlsClientConfig::new()
        .with_server_name(rama::net::address::Host::from_static("local.mdma.test"))
        .with_modify_rustls_config(move |_| Ok(client.clone()));
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector::<Body>(Executor::default())
        .without_connection_pool()
        .build_client::<Body, Body, _>();
    let client = FederationClient::new(
        WireTransport(rama::service::BoxService::new(connector)),
        format!("https://{addr}").parse().unwrap(),
    );
    let response = tokio::time::timeout(Duration::from_secs(2), client.identify(identify()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.into_inner().server_id, "local.mdma.test");
    let reply = tokio::time::timeout(Duration::from_secs(2), client.scan_capabilities(scan()))
        .await
        .unwrap()
        .unwrap()
        .into_inner();
    let envelope = catalog::SignedCatalogEnvelope {
        key_id: reply.key_id,
        body_json: reply.body_json,
        signature: reply.signature.try_into().unwrap(),
    };
    let now = utc_ms().unwrap();
    let body = catalog::verify_signed_catalog(
        &envelope,
        &signing_public_key,
        &catalog::VerifyContext {
            now_ms: now,
            expected_origin: "local.mdma.test",
            expected_destination: "remote.mdma.test",
            expected_nonce: &[9; 32],
            key_valid_from_ms: f.now_ms - 60_000,
            key_valid_until_ms: f.now_ms + 300_000,
        },
    )
    .unwrap();
    assert_eq!(body.models, expected.models);
    f.store.suspend("remote.mdma.test").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), client.identify(identify()))
            .await
            .unwrap()
            .is_err()
    );
    task.abort();
    let _ = task.await;
}

impl ControlService {
    pub(crate) fn execution_idle(&self) -> bool {
        self.execution.as_ref().is_none_or(crate::federation::execution::Service::is_idle)
    }
}
