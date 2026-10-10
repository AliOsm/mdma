use super::*;
use crate::federation::{
    config::{SigningKey, Window},
    control::{ControlService, Federation, FederationServer, IdentifyRequest, IdentifyResponse, Signer, SignedCatalog},
    signing::KeyPairHandle, tls::tests::Fixture,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::{Layer, graceful::Shutdown, http::{grpc, server::HttpServer}, tcp::server::TcpListener};
use rama::tls::{server::TlsServerConfig, rustls::server::{RustlsServerConfigExt, TlsAcceptorLayer}};
use std::{sync::atomic::{AtomicBool, Ordering}, time::Duration};
use tokio::sync::{Notify, oneshot};

#[test]
fn diagnostic_stage_keeps_private_cause_out_of_formatting() {
    let error = stage("tcp_connect", std::io::Error::new(
        std::io::ErrorKind::PermissionDenied, "private endpoint and credentials"));
    assert_eq!(format!("{error}"), "tcp_connect");
    assert_eq!(format!("{error:?}"), "tcp_connect");
    assert_eq!(error.downcast_ref::<Stage>().unwrap().io_kind(),
        Some(std::io::ErrorKind::PermissionDenied));
}

#[derive(Clone)]
struct Remote {
    service: ControlService,
    stalled: Arc<AtomicBool>,
    tampered: Arc<AtomicBool>,
    oversized: Arc<AtomicBool>,
    dispatched: Arc<Notify>,
}

impl Federation for Remote {
    type SynthesizeStream = crate::federation::execution::EventStream;
    async fn decide(&self, request: grpc::Request<crate::federation::control::DecideRequest>)
        -> Result<grpc::Response<crate::federation::control::DecideResponse>, Status> {
        self.service.decide(request).await
    }
    async fn synthesize(&self, request: grpc::Request<crate::federation::control::SynthesizeRequest>)
        -> Result<grpc::Response<Self::SynthesizeStream>, Status> {
        self.service.synthesize(request).await
    }
    async fn identify(&self, request: grpc::Request<IdentifyRequest>) -> Result<grpc::Response<IdentifyResponse>, Status> {
        self.service.identify(request).await
    }
    async fn scan_capabilities(&self, request: grpc::Request<ScanCapabilitiesRequest>) -> Result<grpc::Response<SignedCatalog>, Status> {
        self.dispatched.notify_one();
        if self.stalled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let mut response = self.service.scan_capabilities(request).await?;
        if self.tampered.load(Ordering::SeqCst) { response.get_mut().signature[0] ^= 1; }
        if self.oversized.load(Ordering::SeqCst) {
            response.get_mut().body_json = vec![0; catalog::MAX_BODY_JSON_BYTES + 1024];
        }
        Ok(response)
    }
}

struct Live {
    fixture: Fixture,
    credentials: Credentials,
    cache: Cache,
    remote: Remote,
    stop: Option<oneshot::Sender<()>>,
    shutdown: Option<Shutdown>,
    task: tokio::task::JoinHandle<()>,
}

impl Live {
    async fn new() -> Self {
        let fixture = Fixture::new();
        let key = KeyPairHandle::generate().unwrap();
        let window = Window { valid_from_ms: fixture.now_ms - 60_000, valid_until_ms: fixture.now_ms + 300_000 };
        let signing = SigningKey { key_id: "ed25519:wire".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.public_key_bytes()), window };
        fixture.store.approve_local_signer(signing.clone()).unwrap();
        let signer = Signer::new(&fixture.store, signing.key_id.clone(), key).unwrap();
        let remote = Remote { service: ControlService::new(fixture.store.clone(), signer),
            stalled: Arc::new(AtomicBool::new(false)), tampered: Arc::new(AtomicBool::new(false)),
            oversized: Arc::new(AtomicBool::new(false)),
            dispatched: Arc::new(Notify::new()) };
        let (stop, stopped) = oneshot::channel();
        let shutdown = Shutdown::new(async { let _ = stopped.await; });
        let executor = Executor::graceful(shutdown.guard());
        let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone()).await.unwrap();
        let (_, server) = fixture.configs();
        let mut policy = fixture.remote_store.snapshot().unwrap().peer("local.mdma.test").unwrap().policy.clone();
        policy.endpoint = format!("https://{}", listener.local_addr().unwrap());
        policy.signing_keys = vec![signing];
        fixture.remote_store.approve_policy(policy).unwrap();
        let service = TlsAcceptorLayer::new(TlsServerConfig::new()
            .with_modify_rustls_config(move |_| Ok(server.clone())))
            .into_layer(tls::BindPeerService::new(fixture.store.clone(), fixture.now_ms + 300_000,
                HttpServer::auto(executor).service(FederationServer::new(remote.clone())
                    .with_max_decoding_message_size(4096)
                    .with_max_encoding_message_size(catalog::MAX_BODY_JSON_BYTES + 16 * 1024))));
        let task = tokio::spawn(async move { listener.serve(service).await });
        let (chain, key) = fixture.outbound_material();
        let credentials = Credentials { store: fixture.remote_store.clone(), chain, key: Arc::new(key),
            signing_key: [42; 32], local_expires_ms: fixture.now_ms + 300_000 };
        Self { fixture, credentials, cache: Cache::default(), remote,
            stop: Some(stop), shutdown: Some(shutdown), task }
    }

    async fn poll(&self) -> Result<CatalogBody, BoxError> {
        self.credentials.poll(&self.cache, "local.mdma.test", Executor::default()).await
    }

    fn cached(&self) -> Option<CatalogBody> {
        self.cache.snapshot(&self.credentials.store, "local.mdma.test",
            tls::utc_ms().unwrap(), Instant::now()).unwrap()
    }

    async fn close(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.shutdown.take().unwrap().shutdown_with_limit(Duration::from_secs(2)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), &mut self.task).await.unwrap().unwrap();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        self.task.abort();
    }
}

#[tokio::test]
async fn completed_connection_binding_populates_verified_cache_without_workload_grants() {
    let live = Live::new().await;
    let body = live.poll().await.unwrap();
    assert_eq!(body.origin, "local.mdma.test");
    assert_eq!(body.destination, "remote.mdma.test");
    assert!(body.models.is_empty());
    assert_eq!(live.cached().unwrap().nonce, body.nonce);
    let next = live.poll().await.unwrap();
    assert_ne!(next.nonce, body.nonce);
    let snapshot = live.fixture.remote_store.snapshot().unwrap();
    let peer = &snapshot.peer("local.mdma.test").unwrap().policy;
    assert!(peer.catalog_exports.is_empty() && peer.execute_exports.is_empty() && peer.route_imports.is_empty());
    live.close().await;
}

#[tokio::test]
async fn invalid_signature_does_not_replace_last_verified_snapshot() {
    let live = Live::new().await;
    let body = live.poll().await.unwrap();
    live.remote.tampered.store(true, Ordering::SeqCst);
    assert!(live.poll().await.is_err());
    assert_eq!(live.cached().unwrap().nonce, body.nonce);
    live.remote.tampered.store(false, Ordering::SeqCst);
    live.remote.oversized.store(true, Ordering::SeqCst);
    assert!(live.poll().await.is_err());
    assert_eq!(live.cached().unwrap().nonce, body.nonce);
    live.remote.oversized.store(false, Ordering::SeqCst);
    assert!(live.poll().await.is_ok());
    live.close().await;
}

#[tokio::test]
async fn timeout_consumes_challenge_closes_connection_and_allows_fresh_poll() {
    let live = Live::new().await;
    live.remote.stalled.store(true, Ordering::SeqCst);
    let start = Instant::now();
    assert!(live.poll().await.is_err());
    assert!(start.elapsed() >= POLL_TIMEOUT && start.elapsed() < Duration::from_secs(3));
    assert!(live.cached().is_none());
    live.remote.stalled.store(false, Ordering::SeqCst);
    assert!(live.poll().await.is_ok());
    live.close().await;
}

#[tokio::test]
async fn current_trust_change_cancels_stalled_poll_before_network_deadline() {
    let live = Live::new().await;
    live.remote.stalled.store(true, Ordering::SeqCst);
    {
        let poll = live.poll();
        tokio::pin!(poll);
        tokio::select! {
            () = live.remote.dispatched.notified() => {},
            result = &mut poll => panic!("unexpected early poll completion: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("control request not dispatched"),
        }
        live.fixture.remote_store.suspend("local.mdma.test").unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(500), poll).await.unwrap().is_err());
    }
    assert!(live.poll().await.is_err());
    live.close().await;
}

#[tokio::test]
async fn dropping_poll_cancels_socket_and_consumes_challenge() {
    let live = Live::new().await;
    live.remote.stalled.store(true, Ordering::SeqCst);
    {
        let poll = live.poll();
        tokio::pin!(poll);
        tokio::select! {
            () = live.remote.dispatched.notified() => {},
            result = &mut poll => panic!("unexpected early result: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("request not dispatched"),
        }
    }
    live.remote.stalled.store(false, Ordering::SeqCst);
    assert!(live.poll().await.is_ok());
    live.close().await;
}

#[tokio::test]
async fn wrong_reference_name_and_removed_role_pin_fail_actual_handshakes() {
    let live = Live::new().await;
    let original = live.fixture.remote_store.snapshot().unwrap().peer("local.mdma.test").unwrap().policy.clone();
    let mut changed = original.clone();
    changed.tls_name = "wrong.mdma.test".into();
    live.fixture.remote_store.approve_policy(changed).unwrap();
    assert!(live.poll().await.is_err());
    let mut changed = original;
    changed.tls_pins.retain(|p| p.role != crate::federation::config::TlsRole::ServerAuth);
    live.fixture.remote_store.approve_policy(changed).unwrap();
    assert!(live.poll().await.is_err());
    live.close().await;
}

#[tokio::test]
async fn runtime_scheduler_populates_cache_and_stops_without_detached_polls() {
    let live = Live::new().await;
    let poller = super::super::polling::Poller::new(live.credentials.clone());
    let cache = poller.cache.clone();
    let (stop, stopped) = oneshot::channel();
    let shutdown = Shutdown::new(async { let _ = stopped.await; });
    let task = tokio::spawn(poller.run(Executor::graceful(shutdown.guard())));
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if cache.snapshot(&live.credentials.store, "local.mdma.test",
                tls::utc_ms().unwrap(), Instant::now()).unwrap().is_some() { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    stop.send(()).unwrap();
    shutdown.shutdown_with_limit(Duration::from_secs(2)).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(cache.snapshot(&live.credentials.store, "local.mdma.test",
        tls::utc_ms().unwrap(), Instant::now()).unwrap().is_none());
    live.close().await;
}
