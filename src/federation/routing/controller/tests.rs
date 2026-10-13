use super::*;
use crate::{
    backend::cpu::CpuBackend,
    federation::{
        catalog::ModelDescriptor,
        config::{ExecuteExport, RouteImport, SigningKey, Window},
        control::{
            ControlService, Federation, FederationServer, IdentifyRequest, IdentifyResponse,
            ScanCapabilitiesRequest, SignedCatalog, Signer,
        },
        execution,
        routing::selection::Receipt,
        signing::KeyPairHandle,
        tls::{self, tests::Fixture},
        trust::Disclosure,
    },
    registry::{Holder, LeaseError, Options, Phase, Registry, Source},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::{
    Layer,
    extensions::{Extensions, Ingress},
    graceful::Shutdown,
    http::{HeaderMap, grpc::Code, server::HttpServer},
    tcp::server::TcpListener,
    tls::{
        rustls::server::{RustlsServerConfigExt, TlsAcceptorLayer},
        server::TlsServerConfig,
    },
};
use std::assert_matches;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use tokio::sync::oneshot;

#[derive(Clone)]
struct Remote {
    service: ControlService,
    mode: Arc<AtomicU8>,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    ids: Arc<Mutex<Vec<Vec<u8>>>>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Remote {
    fn enter(&self, envelope: Option<&control::ExecutionEnvelope>) -> Active {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        self.ids
            .lock()
            .unwrap()
            .push(envelope.unwrap().request_id.clone());
        Active(self.active.clone())
    }
}
impl Federation for Remote {
    type SynthesizeStream = execution::EventStream;
    async fn acquire_lease(
        &self,
        request: Request<crate::federation::control::LeaseRequest>,
    ) -> Result<Response<crate::federation::control::LeaseGrant>, Status> {
        self.service.acquire_lease(request).await
    }
    async fn renew_lease(
        &self,
        request: Request<crate::federation::control::LeaseRenewal>,
    ) -> Result<Response<crate::federation::control::LeaseGrant>, Status> {
        self.service.renew_lease(request).await
    }
    async fn release_lease(
        &self,
        request: Request<crate::federation::control::LeaseRelease>,
    ) -> Result<Response<crate::federation::control::LeaseReleased>, Status> {
        self.service.release_lease(request).await
    }
    async fn identify(
        &self,
        request: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        self.service.identify(request).await
    }
    async fn scan_capabilities(
        &self,
        request: Request<ScanCapabilitiesRequest>,
    ) -> Result<Response<SignedCatalog>, Status> {
        self.service.scan_capabilities(request).await
    }
    async fn decide(
        &self,
        request: Request<control::DecideRequest>,
    ) -> Result<Response<control::DecideResponse>, Status> {
        let _active = self.enter(request.get_ref().envelope.as_ref());
        let mode = self.mode.load(Ordering::SeqCst);
        if mode == 2 {
            return Err(Status::unavailable("private remote path"));
        }
        if mode == 6 {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let mut response = self.service.decide(request).await?;
        if mode == 3 {
            response.get_mut().profile_id.push('0');
        }
        if mode == 4 {
            response.get_mut().result_json = "{}".into();
        }
        Ok(response)
    }
    async fn synthesize(
        &self,
        request: Request<control::SynthesizeRequest>,
    ) -> Result<Response<Self::SynthesizeStream>, Status> {
        let active = self.enter(request.get_ref().envelope.as_ref());
        let mode = self.mode.load(Ordering::SeqCst);
        if mode == 7 {
            let sampler = request
                .get_ref()
                .sampler
                .as_ref()
                .expect("origin must send effective sampler");
            assert!(
                sampler.temperature.is_some()
                    && sampler.top_p.is_some()
                    && sampler.min_p.is_some()
                    && sampler.repetition_penalty.is_some()
                    && sampler.repetition_window.is_some()
            );
        }
        if mode == 2 {
            return Err(Status::unavailable("private remote path"));
        }
        if mode == 6 {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let response = self.service.synthesize(request).await?;
        let stream = response.into_inner().map(move |event| {
            let _active = &active;
            event.map(|mut event| {
                if let Some(control::synthesize_event::Event::Accepted(a)) = event.event.as_mut()
                    && mode == 3
                {
                    a.artifact_id.push('0');
                }
                if let Some(control::synthesize_event::Event::Token(t)) = event.event.as_mut()
                    && mode == 4
                {
                    t.index += 1;
                }
                event
            })
        });
        let extra = if mode == 5 {
            vec![Err(Status::internal("private remote path"))]
        } else {
            vec![]
        };
        Ok(Response::new(Box::pin(
            stream.chain(tokio_stream::iter(extra)),
        )))
    }
}

/// A model's phase and lease count on the executor.
type OnExecutor = Box<dyn Fn(&str) -> (Phase, u32) + Send + Sync>;

struct Live {
    fixture: Fixture,
    router: Router,
    remote: Remote,
    /// The executor's power state.
    power: tokio::sync::watch::Sender<crate::power::PowerStatus>,
    caller: tls::Caller,
    digest: String,
    expected: serde_json::Value,
    stop: Option<oneshot::Sender<()>>,
    shutdown: Option<Shutdown>,
    task: tokio::task::JoinHandle<()>,
    on_executor: OnExecutor,
    /// The executor loads from these on first use.
    _models: (tempfile::NamedTempFile, tempfile::TempDir),
}
impl Live {
    async fn new() -> Self {
        Self::with_backend(Arc::new(CpuBackend), "cpu").await
    }
    async fn with_backend<B>(backend: Arc<B>, name: &'static str) -> Self
    where
        B: crate::backend::Backend + Send + Sync + 'static,
        B::Buf: Send + Sync,
    {
        let fixture = Fixture::new();
        let gguf = crate::gguf::tests::Fixture::new(1, 8)
            .with_tokenizer()
            .write();
        let laya = crate::laya::compat_tests::checkpoint();
        let models = vec![
            ("native".into(), Source::Gguf(gguf.path().into())),
            ("laya".into(), Source::Laya(laya.path().into())),
        ];
        let options = Options {
            frozen: true,
            ..Options::new(32)
        };
        let mut registry = Registry::new(backend, models, Some("native"), options).unwrap();
        // Nothing is loaded: the first federated request loads each model.
        registry.hash_artifacts(|| false);
        assert_eq!(registry.capture_execution_profiles().unwrap(), 2);
        let (power, status) = tokio::sync::watch::channel(crate::power::PowerStatus::default());
        let native = grpc::MdmaService::new(registry, name).with_power(
            status,
            crate::power::DegradedLimits {
                n_ctx: 12,
                max_tokens: 2,
            },
        );
        let registry = native.registry().clone();
        let on_executor = Box::new(move |model: &str| {
            let entry = registry
                .entries()
                .find(|entry| entry.name == model)
                .unwrap();
            (entry.phase, entry.leases)
        });
        let inventory = native.federation_inventory();
        let descriptors = inventory.snapshot(&["native".into(), "laya".into()]).models;
        let executor = native.federation_executor();
        let expected = crate::laya::Predictor::load(CpuBackend, laya.path())
            .unwrap()
            .predict(
                &serde_json::json!({}),
                serde_json::from_str::<serde_json::Value>(&questions())
                    .unwrap()
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        let key = KeyPairHandle::generate().unwrap();
        let window = Window {
            valid_from_ms: fixture.now_ms - 60_000,
            valid_until_ms: fixture.now_ms + 300_000,
        };
        let signing = SigningKey {
            key_id: "ed25519:wire".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.public_key_bytes()),
            window,
        };
        fixture.store.approve_local_signer(signing.clone()).unwrap();
        let signer = Signer::new(&fixture.store, signing.key_id.clone(), key).unwrap();
        let mut policy = fixture
            .store
            .snapshot()
            .unwrap()
            .peer("remote.mdma.test")
            .unwrap()
            .policy
            .clone();
        policy.catalog_exports = vec!["native".into(), "laya".into()];
        policy.execute_exports = vec![
            ExecuteExport {
                model: "native".into(),
                operation: Operation::Synthesize,
            },
            ExecuteExport {
                model: "laya".into(),
                operation: Operation::Decide,
            },
        ];
        fixture.store.approve_policy(policy).unwrap();
        let remote = Remote {
            service: ControlService::new(fixture.store.clone(), signer)
                .with_inventory(inventory)
                .with_execution(executor),
            mode: Arc::new(AtomicU8::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            ids: Arc::new(Mutex::new(vec![])),
        };
        let (stop, stopped) = oneshot::channel();
        let shutdown = Shutdown::new(async {
            let _ = stopped.await;
        });
        let executor = Executor::graceful(shutdown.guard());
        let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone())
            .await
            .unwrap();
        let (_, server) = fixture.configs();
        let mut policy = fixture
            .remote_store
            .snapshot()
            .unwrap()
            .peer("local.mdma.test")
            .unwrap()
            .policy
            .clone();
        policy.endpoint = format!("https://{}", listener.local_addr().unwrap());
        policy.signing_keys = vec![signing];
        policy.route_imports = descriptors
            .iter()
            .map(|d| RouteImport {
                model: d.model.clone(),
                artifact_id: d.artifact_id.clone(),
                profile_id: d.profile_id.clone(),
                operation: if d.model == "native" {
                    Operation::Synthesize
                } else {
                    Operation::Decide
                },
            })
            .collect();
        fixture.remote_store.approve_policy(policy).unwrap();
        let (caller, digest) = tls::caller::tests::authenticated();
        for (model, operation) in [
            ("native", Operation::Synthesize),
            ("laya", Operation::Decide),
        ] {
            fixture
                .remote_store
                .approve_disclosure(Disclosure {
                    caller_spki_sha256: digest.clone(),
                    peer_id: "local.mdma.test".into(),
                    model: model.into(),
                    operation,
                    window,
                })
                .unwrap();
        }
        let mut http = HttpServer::new_h2(executor.clone());
        http.h2_mut().set_max_send_buf_size(1);
        let service = TlsAcceptorLayer::new(
            TlsServerConfig::new().with_modify_rustls_config(move |_| Ok(server.clone())),
        )
        .into_layer(tls::BindPeerService::new(
            fixture.store.clone(),
            fixture.now_ms + 300_000,
            http.service(execution::ingress::Gate::new(
                FederationServer::new(remote.clone())
                    .with_max_decoding_message_size(96 * 1024)
                    .with_max_encoding_message_size(524288 + 2048),
            )),
        ));
        let task = tokio::spawn(async move { listener.serve(service).await });
        let (chain, key) = fixture.outbound_material();
        let credentials = Credentials {
            store: fixture.remote_store.clone(),
            chain,
            key: Arc::new(key),
            signing_key: [42; 32],
            local_expires_ms: fixture.now_ms + 300_000,
        };
        let cache = Cache::default();
        credentials
            .poll(&cache, "local.mdma.test", Executor::default())
            .await
            .unwrap();
        let routes = descriptors
            .into_iter()
            .map(|d| Route {
                alias: format!("remote-{}", d.model),
                peer_id: "local.mdma.test".into(),
                model: d.model.clone(),
                artifact_id: d.artifact_id,
                profile_id: d.profile_id,
                operation: if d.model == "native" {
                    Operation::Synthesize
                } else {
                    Operation::Decide
                },
            })
            .collect();
        let router = Router::new(routes, credentials, cache, Executor::default(), &[]).unwrap();
        Self {
            fixture,
            router,
            remote,
            power,
            caller,
            digest,
            expected,
            stop: Some(stop),
            shutdown: Some(shutdown),
            task,
            on_executor,
            _models: (gguf, laya),
        }
    }
    fn request<T>(&self, message: T, timeout: &str) -> Request<T> {
        let request = Request::new(message);
        let transport = Extensions::new();
        transport.insert(self.caller.clone());
        request.extensions().insert(Ingress(transport));
        let mut headers = HeaderMap::new();
        headers.insert("grpc-timeout", timeout.parse().unwrap());
        request
            .extensions()
            .insert(Receipt(execution::deadline::Ingress::capture(&headers)));
        request
    }
    async fn synthesize(&self, timeout: &str) -> Result<Response<TokenStream>, Status> {
        self.router
            .synthesize(self.request(synth(false), timeout))
            .await
    }
    /// A fresh signed catalog from the executor.
    async fn poll_catalog(&self) {
        self.router
            .credentials
            .poll(&self.router.cache, "local.mdma.test", Executor::default())
            .await
            .unwrap();
    }
    /// The executor's `remote-native` descriptor in the origin's cached catalog.
    fn catalog(&self) -> (crate::federation::catalog::ServerState, ModelDescriptor) {
        let catalog = self
            .router
            .cache
            .snapshot(
                &self.router.credentials.store,
                "local.mdma.test",
                tls::utc_ms().unwrap(),
                std::time::Instant::now(),
            )
            .unwrap()
            .unwrap();
        let native = catalog.models.into_iter().find(|d| d.model == "native");
        (catalog.server_state, native.unwrap())
    }
    async fn decide(&self, timeout: &str) -> Result<Response<grpc::DecideResponse>, Status> {
        self.router
            .decide(self.request(
                grpc::DecideRequest {
                    model: "remote-laya".into(),
                    state_json: "{}".into(),
                    questions_json: questions(),
                },
                timeout,
            ))
            .await
    }
    async fn idle(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.router.slots.available_permits() != 1
                || self.remote.active.load(Ordering::SeqCst) != 0
                // A reset can drop the RPC before the actual native worker
                // joins. Resource exhaustion then is correct, not a protocol
                // validator failure. Wait for both actual charges to return.
                || !self.remote.service.execution_idle()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn close(mut self) {
        self.idle().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.shutdown
            .take()
            .unwrap()
            .shutdown_with_limit(Duration::from_secs(2))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}
/// Polls until the executor reports `expected` for `model`.
async fn executor_reaches(live: &Live, model: &str, expected: (Phase, u32)) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while (live.on_executor)(model) != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{model} stayed {:?}, expected {expected:?}",
            (live.on_executor)(model)
        )
    });
}

fn questions() -> String {
    r#"{"q":{"type":"choice","instructions":"hello","criteria":["world","hello"]}}"#.into()
}

fn synth(emit_events: bool) -> grpc::SynthesizeRequest {
    grpc::SynthesizeRequest {
        model: "remote-native".into(),
        prompt: "abc".into(),
        max_tokens: 3,
        sampler: Some(grpc::SynthesizeSamplerConfig {
            temperature: Some(0.),
            ..Default::default()
        }),
        emit_events,
        chat: false,
        system: String::new(),
    }
}

/// Every message of a routed run that must succeed.
async fn replies(
    stream: impl Stream<Item = Result<grpc::SynthesizeResponse, Status>>,
) -> Vec<grpc::SynthesizeResponse> {
    stream.map(Result::unwrap).collect().await
}

/// Without `emit_events` a routed run sends tokens only.
fn tokens_only(replies: &[grpc::SynthesizeResponse]) {
    assert!(
        replies
            .iter()
            .all(|r| r.accepted.is_none() && r.completed.is_none()),
        "{replies:?}"
    );
}

/// With `emit_events`: the executor's accepted first, its completed last.
fn events(
    replies: &[grpc::SynthesizeResponse],
) -> (grpc::SynthesizeAccepted, grpc::SynthesizeCompleted) {
    let [first, tokens @ .., last] = replies else {
        panic!("accepted and completed: {replies:?}");
    };
    let accepted = first.accepted.expect("accepted comes first");
    let completed = last.completed.clone().expect("completed comes last");
    tokens_only(tokens);
    assert_eq!(completed.emitted_tokens as usize, tokens.len());
    if completed.emitted_tokens < accepted.max_tokens {
        assert_eq!(completed.finish_reason, "eos");
    } else {
        assert_matches!(
            completed.finish_reason.as_str(),
            "max_tokens" | "context_limit"
        );
    }
    (accepted, completed)
}

#[tokio::test]
async fn actual_single_hop_native_synthesis_decision_and_response_quota() {
    let live = Live::new().await;
    let mut stream = live.synthesize("3000m").await.unwrap().into_inner();
    let tokens = replies(&mut stream).await;
    tokens_only(&tokens);
    assert!(tokens.len() <= 3);
    assert_eq!(
        live.decide("3000m").await.err().unwrap().code(),
        Code::ResourceExhausted
    );
    drop(stream);
    live.idle().await;
    let response = live.decide("3000m").await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response.get_ref().result_json).unwrap(),
        live.expected
    );
    assert_eq!(
        live.decide("3000m").await.err().unwrap().code(),
        Code::ResourceExhausted
    );
    drop(response);
    live.idle().await;
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 2);
    let ids = live.remote.ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0].len(), 16);
    assert_ne!(ids[0], ids[1]);
    live.close().await;
}

#[tokio::test]
async fn power_flips_keep_the_route_selected_and_events_report_the_executors_caps() {
    use crate::federation::catalog::ServerState;
    use crate::power::{PowerStatus, ServerState as PowerState};
    let live = Live::new().await;
    let (_, selected) = live.catalog();
    for (power, state, caps) in [
        (PowerState::Degraded, ServerState::Degraded, (12, 2)),
        (PowerState::Serving, ServerState::Serving, (32, 3)),
    ] {
        live.power
            .send(PowerStatus {
                state: power,
                reading: None,
            })
            .unwrap();
        live.poll_catalog().await;
        // A fresh signed catalog: new state, same selection.
        let (advertised, descriptor) = live.catalog();
        assert_eq!(advertised, state);
        assert!(descriptor.same_selection(&selected));
        let request = live.request(synth(true), "3000m");
        let stream = live.router.synthesize(request).await.unwrap().into_inner();
        let (accepted, completed) = events(&replies(stream).await);
        assert_eq!(
            (accepted.prompt_tokens, accepted.n_ctx, accepted.max_tokens),
            (3, caps.0, caps.1)
        );
        assert!(completed.prefill_ms + completed.decode_ms <= 3000);
        live.idle().await;
    }
    live.close().await;
}

#[tokio::test]
async fn a_local_lease_on_a_route_keeps_the_model_loaded_on_its_executor() {
    let live = Live::new().await;
    let (first, second) = (
        Holder::Client("first".into()),
        Holder::Client("second".into()),
    );
    assert_eq!((live.on_executor)("native"), (Phase::Available, 0));
    assert_eq!(
        live.router.lease(first.clone(), "native", None),
        Err(LeaseError::Missing("native".into()))
    );

    let a = live
        .router
        .lease(first.clone(), "remote-native", None)
        .unwrap();
    executor_reaches(&live, "native", (Phase::Resident, 1)).await;
    let b = live
        .router
        .lease(second.clone(), "remote-native", None)
        .unwrap();
    assert_eq!(
        live.router.release_lease(&second, &a.id),
        Err(LeaseError::Unknown)
    );
    assert_eq!(
        live.router.renew_lease(&first, &a.id, None).unwrap().model,
        "remote-native"
    );

    // The keeper renews its one executor lease while a local lease is left.
    live.router.release_lease(&first, &a.id).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!((live.on_executor)("native"), (Phase::Resident, 1));
    live.router.release_lease(&second, &b.id).unwrap();
    // Released, not unloaded: the executor keeps idle models until evicted.
    executor_reaches(&live, "native", (Phase::Resident, 0)).await;
    assert_eq!((live.on_executor)("laya"), (Phase::Available, 0));
    live.close().await;
}

#[tokio::test]
async fn executors_refuse_leases_on_a_changed_selection_or_without_a_grant() {
    let live = Live::new().await;
    let route = live.router.routes["remote-laya"].clone();
    let acquire = |model: &str, artifact_id: &str| control::LeaseRequest {
        origin: live.router.origin().unwrap(),
        destination: route.peer_id.clone(),
        model: model.into(),
        artifact_id: artifact_id.into(),
        profile_id: route.profile_id.clone(),
        ttl_ms: 0,
    };
    let refused = async |request: control::LeaseRequest| {
        live.router
            .remote(&route, |client| async move {
                client.acquire_lease(request).await
            })
            .await
            .unwrap_err()
            .code()
    };
    assert_eq!(
        refused(acquire("laya", "sha256:stale")).await,
        Code::FailedPrecondition
    );
    // Missing and ungranted models look alike.
    assert_eq!(
        refused(acquire("absent", &route.artifact_id)).await,
        Code::PermissionDenied
    );
    let mut policy = live
        .fixture
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy
        .execute_exports
        .retain(|export| export.model != "laya");
    live.fixture.store.approve_policy(policy).unwrap();
    assert_eq!(
        refused(acquire("laya", &route.artifact_id)).await,
        Code::PermissionDenied
    );
    assert_eq!((live.on_executor)("laya"), (Phase::Available, 0));
    live.close().await;
}

#[tokio::test]
async fn executor_protocol_mismatch_and_non_ok_never_redispatch() {
    let live = Live::new().await;
    for (mode, code) in [
        (2, Code::Unavailable),
        (3, Code::DataLoss),
        (4, Code::DataLoss),
        (5, Code::Internal),
    ] {
        for emit_events in [false, true] {
            live.remote.mode.store(mode, Ordering::SeqCst);
            let before = live.remote.calls.load(Ordering::SeqCst);
            let request = live.request(synth(emit_events), "3000m");
            let mut stream = live.router.synthesize(request).await.unwrap().into_inner();
            let mut forwarded = Vec::new();
            let error = loop {
                match stream
                    .next()
                    .await
                    .expect("protocol/transport error was hidden")
                {
                    Ok(reply) => forwarded.push(reply),
                    Err(status) => break status,
                }
            };
            assert_eq!(error.code(), code);
            assert!(!error.message().contains("private"));
            // Only validated events go out, and a failed run never completes.
            // Modes 2 and 3 fail before a valid Accepted.
            assert!(forwarded.iter().all(|r| r.completed.is_none()));
            assert_eq!(
                forwarded.iter().any(|r| r.accepted.is_some()),
                emit_events && mode >= 4,
                "mode {mode}"
            );
            drop(stream);
            live.idle().await;
            assert_eq!(live.remote.calls.load(Ordering::SeqCst), before + 1);
        }
    }
    for mode in [2, 3, 4] {
        live.remote.mode.store(mode, Ordering::SeqCst);
        let before = live.remote.calls.load(Ordering::SeqCst);
        assert!(live.decide("3000m").await.is_err());
        live.idle().await;
        assert_eq!(live.remote.calls.load(Ordering::SeqCst), before + 1);
    }
    live.close().await;
}

#[tokio::test]
async fn unpolled_origin_deadline_cancels_remote_and_retains_response_charge() {
    let live = Live::new().await;
    live.remote.mode.store(6, Ordering::SeqCst);
    let mut stream = live.synthesize("120m").await.unwrap().into_inner();
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(live.remote.active.load(Ordering::SeqCst), 0);
    assert_eq!(live.router.slots.available_permits(), 0);
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::DeadlineExceeded
    );
    drop(stream);
    live.idle().await;
    assert!(live.remote.calls.load(Ordering::SeqCst) <= 1);
    live.close().await;
}

#[tokio::test]
async fn disclosure_revocation_cancels_unpolled_work_and_blocks_queued_plaintext() {
    let live = Live::new().await;
    let mut stream = live.synthesize("3000m").await.unwrap().into_inner();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = live.remote.calls.load(Ordering::SeqCst);
    live.fixture
        .remote_store
        .remove_disclosure(
            &live.digest,
            "local.mdma.test",
            "native",
            Operation::Synthesize,
        )
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap_err().code(),
        Code::PermissionDenied
    );
    drop(stream);
    live.idle().await;
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), before);
    assert!(live.synthesize("3000m").await.is_err());
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), before);
    live.close().await;
}

#[tokio::test]
async fn origin_handler_and_stream_drop_close_single_owned_remote_connection() {
    let live = Live::new().await;
    live.remote.mode.store(6, Ordering::SeqCst);
    let stream = live.synthesize("3000m").await.unwrap().into_inner();
    tokio::time::timeout(Duration::from_secs(1), async {
        while live.remote.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(stream);
    live.idle().await;
    let before = live.remote.calls.load(Ordering::SeqCst);
    {
        let response = live.decide("3000m");
        tokio::pin!(response);
        tokio::select! {
            () = async {
                while live.remote.calls.load(Ordering::SeqCst) == before { tokio::time::sleep(Duration::from_millis(5)).await; }
            } => {},
            result = &mut response => panic!("stalled response returned: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(1)) => panic!("unary not dispatched"),
        }
    }
    live.idle().await;
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), before + 1);
    live.close().await;
}

#[tokio::test]
async fn alias_collisions_wrong_operation_missing_principal_and_invalid_input_do_not_dial() {
    let live = Live::new().await;
    let routes: Vec<_> = live.router.routes.values().cloned().collect();
    assert!(
        Router::new(
            routes.clone(),
            live.router.credentials.clone(),
            live.router.cache.clone(),
            Executor::default(),
            &["remote-native".into()]
        )
        .is_err()
    );
    let mut duplicate = routes.clone();
    duplicate.push(routes[0].clone());
    assert!(
        Router::new(
            duplicate,
            live.router.credentials.clone(),
            live.router.cache.clone(),
            Executor::default(),
            &[]
        )
        .is_err()
    );
    let request = Request::new(grpc::DecideRequest {
        model: "remote-laya".into(),
        state_json: "{}".into(),
        questions_json: questions(),
    });
    assert_eq!(
        live.router.decide(request).await.err().unwrap().code(),
        Code::PermissionDenied
    );
    let request = live.request(
        grpc::DecideRequest {
            model: "remote-native".into(),
            state_json: "{}".into(),
            questions_json: questions(),
        },
        "3000m",
    );
    assert_eq!(
        live.router.decide(request).await.err().unwrap().code(),
        Code::PermissionDenied
    );
    let request = live.request(
        grpc::SynthesizeRequest {
            model: "remote-native".into(),
            prompt: "abc".into(),
            max_tokens: 1,
            sampler: Some(grpc::SynthesizeSamplerConfig {
                top_p: Some(0.),
                ..Default::default()
            }),
            emit_events: false,
            chat: false,
            system: String::new(),
        },
        "3000m",
    );
    assert_eq!(
        live.router.synthesize(request).await.err().unwrap().code(),
        Code::InvalidArgument
    );
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 0);
    live.close().await;
}

struct Wire(
    rama::service::BoxService<
        rama::http::Request,
        rama::http::Response,
        rama::error::extra::OpaqueError,
    >,
);

#[tokio::test]
async fn origin_declares_effective_defaults_and_preserves_explicit_zero_on_the_wire() {
    let live = Live::new().await;
    live.remote.mode.store(7, Ordering::SeqCst);
    for sampler in [
        None,
        Some(grpc::SynthesizeSamplerConfig {
            temperature: Some(0.),
            ..Default::default()
        }),
    ] {
        let request = live.request(
            grpc::SynthesizeRequest {
                model: "remote-native".into(),
                prompt: "abc".into(),
                max_tokens: 2,
                sampler,
                emit_events: false,
                chat: false,
                system: String::new(),
            },
            "3000m",
        );
        let mut stream = live.router.synthesize(request).await.unwrap().into_inner();
        while let Some(item) = stream.next().await {
            item.unwrap();
        }
        drop(stream);
        live.idle().await;
    }
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 2);
    live.close().await;
}
impl rama::Service<rama::http::Request> for Wire {
    type Output = rama::http::Response;
    type Error = Status;
    async fn serve(&self, request: rama::http::Request) -> Result<Self::Output, Status> {
        self.0
            .serve(request)
            .await
            .map_err(|_| Status::unavailable("normal client transport failed"))
    }
}

#[tokio::test]
async fn actual_normal_client_tls_http2_routes_both_ops_requires_deadline_and_reset_cancels() {
    check_normal_client(Live::new().await).await;
}

async fn check_normal_client(live: Live) {
    use crate::federation::runtime::ClientPolicy;
    use rama::tls::rustls::client::RustlsClientConfigExt;
    use rama::{
        http::{Body, client::EasyHttpConnectorBuilder},
        tls::client::TlsClientConfig,
    };
    let mut material = tls::caller::tests::material(true, true);
    material.client.resumption = rama::tls::rustls::dep::rustls::client::Resumption::disabled();
    let actual_caller_digest = tls::certificate_spki_sha256(&material.caller_leaf).unwrap();
    for (model, operation) in [
        ("native", Operation::Synthesize),
        ("laya", Operation::Decide),
    ] {
        live.fixture
            .remote_store
            .approve_disclosure(Disclosure {
                caller_spki_sha256: actual_caller_digest.clone(),
                peer_id: "local.mdma.test".into(),
                model: model.into(),
                operation,
                window: Window {
                    valid_from_ms: live.fixture.now_ms - 1000,
                    valid_until_ms: live.fixture.now_ms + 100_000,
                },
            })
            .unwrap();
    }
    // Reviewed consent edits invalidate old cache generations; obtain a fresh
    // signed catalog, not a test-injected snapshot.
    live.poll_catalog().await;
    let mut normal = crate::federation::inventory::tests::native_service();
    normal.set_federation_router(live.router.clone());
    let policy = ClientPolicy::test_routing(
        std::slice::from_ref(&material.server_leaf),
        std::slice::from_ref(&material.root),
    );
    let executor = Executor::default();
    let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone())
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let mut http = HttpServer::new_h2(executor);
    http.h2_mut().set_max_send_buf_size(1);
    let server_config = material.server;
    let service = policy.connection_service(
        TlsAcceptorLayer::new(
            TlsServerConfig::new().with_modify_rustls_config(move |_| Ok(server_config.clone())),
        )
        .into_layer(
            policy.tls_service(http.service(policy.http_service(grpc::MdmaServer::new(normal)))),
        ),
    );
    let server = tokio::spawn(async move { listener.serve(service).await });
    let client_config = material.client;
    let tls = TlsClientConfig::new()
        .with_server_name(rama::net::address::Host::from_static("local.mdma.test"))
        .with_modify_rustls_config(move |_| Ok(client_config.clone()));
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector::<Body>(Executor::default())
        .without_connection_pool()
        .build_client::<Body, Body, _>();
    let client = grpc::MdmaClient::new(
        Wire(rama::service::BoxService::new(connector)),
        format!("https://{address}").parse().unwrap(),
    );
    let health = client
        .introspect(grpc::IntrospectRequest {})
        .await
        .unwrap()
        .into_inner();
    for (alias, trained_ctx) in [("remote-native", 64), ("remote-laya", 16)] {
        let status = health.models.iter().find(|m| m.name == alias).unwrap();
        assert!(status.ready);
        assert!(status.backend.starts_with("federated/"));
        // Header facts travel in the executor's catalog.
        assert_eq!(
            (
                status.parameters > 0,
                status.quantization.as_str(),
                status.trained_ctx
            ),
            (true, "F32", trained_ctx),
            "{alias}"
        );
    }
    live.fixture
        .remote_store
        .remove_disclosure(
            &actual_caller_digest,
            "local.mdma.test",
            "native",
            Operation::Synthesize,
        )
        .unwrap();
    live.poll_catalog().await;
    let health = client
        .introspect(grpc::IntrospectRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(!health.models.iter().any(|m| m.name == "remote-native"));
    assert!(health.models.iter().any(|m| m.name == "remote-laya"));
    live.fixture
        .remote_store
        .approve_disclosure(Disclosure {
            caller_spki_sha256: actual_caller_digest,
            peer_id: "local.mdma.test".into(),
            model: "native".into(),
            operation: Operation::Synthesize,
            window: Window {
                valid_from_ms: live.fixture.now_ms - 1000,
                valid_until_ms: live.fixture.now_ms + 100_000,
            },
        })
        .unwrap();
    live.poll_catalog().await;
    assert_eq!(
        client.synthesize(synth(false)).await.err().unwrap().code(),
        Code::InvalidArgument
    );
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 0);
    for emit_events in [false, true] {
        let mut request = Request::new(synth(emit_events));
        request.try_set_timeout(Duration::from_secs(3)).unwrap();
        let replies = replies(client.synthesize(request).await.unwrap().into_inner()).await;
        if emit_events {
            // The executor's admission: "abc" in its 32-token context.
            let (accepted, _) = events(&replies);
            assert_eq!(
                (accepted.prompt_tokens, accepted.n_ctx, accepted.max_tokens),
                (3, 32, 3)
            );
        } else {
            tokens_only(&replies);
        }
        live.idle().await;
    }
    let mut request = Request::new(grpc::DecideRequest {
        model: "remote-laya".into(),
        state_json: "{}".into(),
        questions_json: questions(),
    });
    request.try_set_timeout(Duration::from_secs(3)).unwrap();
    let result = client.decide(request).await.unwrap().into_inner();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.result_json).unwrap(),
        live.expected
    );
    live.idle().await;
    // Missing deadlines still work on ordinary local/default APIs.
    client.introspect(grpc::IntrospectRequest {}).await.unwrap();
    let local = grpc::SynthesizeRequest {
        model: String::new(),
        ..synth(false)
    };
    let mut stream = client.synthesize(local).await.unwrap().into_inner();
    while let Some(token) = stream.next().await {
        token.unwrap();
    }
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 3);
    // Leases on a routed alias are held on the executor.
    let lease = client
        .acquire(grpc::AcquireRequest {
            model: "remote-laya".into(),
            ttl_ms: 0,
            wait: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (lease.model.as_str(), lease.ttl_ms),
        ("remote-laya", 60_000)
    );
    executor_reaches(&live, "laya", (Phase::Resident, 1)).await;
    let health = client
        .introspect(grpc::IntrospectRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        health
            .models
            .iter()
            .find(|m| m.name == "remote-laya")
            .unwrap()
            .leases,
        1
    );
    let renewed = client
        .renew(grpc::RenewRequest {
            lease_id: lease.lease_id.clone(),
            ttl_ms: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renewed.lease_id, lease.lease_id);
    client
        .release(grpc::ReleaseRequest {
            lease_id: lease.lease_id,
        })
        .await
        .unwrap();
    executor_reaches(&live, "laya", (Phase::Resident, 0)).await;
    live.remote.mode.store(6, Ordering::SeqCst);
    let mut request = Request::new(grpc::DecideRequest {
        model: "remote-laya".into(),
        state_json: "{}".into(),
        questions_json: questions(),
    });
    request.try_set_timeout(Duration::from_secs(3)).unwrap();
    let call = tokio::spawn(async move { client.decide(request).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while live.remote.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    live.idle().await;
    assert_eq!(live.remote.calls.load(Ordering::SeqCst), 4);
    server.abort();
    let _ = server.await;
    live.close().await;
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[tokio::test]
async fn actual_normal_client_two_leg_mtls_executes_metal_and_keeps_cpu_decisions() {
    check_normal_client(
        Live::with_backend(
            Arc::new(crate::backend::metal::MetalBackend::new().unwrap()),
            "metal",
        )
        .await,
    )
    .await;
}

#[cfg(feature = "vulkan")]
#[tokio::test]
async fn actual_normal_client_two_leg_mtls_executes_vulkan_and_keeps_cpu_decisions() {
    check_normal_client(
        Live::with_backend(
            Arc::new(crate::backend::vulkan::VulkanBackend::new().unwrap()),
            "vulkan",
        )
        .await,
    )
    .await;
}

#[cfg(feature = "rocm")]
#[tokio::test]
async fn actual_normal_client_two_leg_mtls_executes_rocm_and_keeps_cpu_decisions() {
    check_normal_client(
        Live::with_backend(
            Arc::new(crate::backend::rocm::HipBackend::new_f32().unwrap()),
            "rocm",
        )
        .await,
    )
    .await;
}

#[cfg(feature = "cuda")]
#[tokio::test]
async fn actual_normal_client_two_leg_mtls_executes_cuda_and_keeps_cpu_decisions() {
    check_normal_client(
        Live::with_backend(
            Arc::new(crate::backend::cuda::CudaBackend::new().unwrap()),
            "cuda",
        )
        .await,
    )
    .await;
}
