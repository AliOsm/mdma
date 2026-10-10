use super::*;
use crate::grpc::TokenStream;
use super::transport::Fixture;
use crate::power::{DegradedLimits, PowerStatus, ServerState};
use crate::registry::ModelsFile;
use proto::mdma_server::Mdma;
use proto::{DecideRequest, DecideResponse, IntrospectRequest, IntrospectResponse, ModelStatus, SynthesizeRequest, SynthesizeResponse};
use rama::graceful::Shutdown;
use rama::http::grpc::{Code, Request, Response, Status};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;

impl Fixture {
    fn service(&self, url: &str, model: &str) -> MdmaService<CpuBackend> {
        let file = ModelsFile::parse(&format!(
            "[[models]]\nname = \"alias\"\n[models.upstream]\nurl = {url:?}\nmodel = {model:?}\ntls_cert = {:?}\ntls_key = {:?}\ntls_ca = {:?}\n",
            self.dir.join("client.pem"), self.dir.join("client-key.pem"), self.dir.join("ca.pem"),
        )).unwrap();
        let registry = Registry::load(&CpuBackend, &file, 64, |_| {}).unwrap();
        MdmaService::new(Arc::new(CpuBackend), registry, "cpu")
    }

}

fn synthesis(prompt: &str) -> SynthesizeRequest {
    SynthesizeRequest { prompt: prompt.into(), max_tokens: 4, sampler: None, model: "alias".into() }
}

fn decision() -> DecideRequest {
    DecideRequest { model: "alias".into(), state_json: "{ \"value\": 1 }".into(), questions_json: "{ \"choice\": [1, 2] }".into() }
}

#[tokio::test]
async fn two_mdma_nodes_stream_native_synthesis_over_mtls() {
    let fixture = Fixture::new();
    let remote = fixture.start(|_| tiny_service(PowerStatus::default(), DegradedLimits::default())).await;
    for model in ["tiny", ""] {
        let gateway = fixture.start(|_| fixture.service(&remote.url, model)).await;
        let client = fixture.client(&gateway.url);
        let health = client.introspect(IntrospectRequest {}).await.unwrap().into_inner();
        assert!(health.ready);
        assert_eq!(health.default_model, "alias");
        assert_eq!(health.model_architecture, "tiny");
        let entry = &health.models[0];
        assert!(entry.ready);
        assert_eq!((&*entry.name, &*entry.kind, &*entry.backend, entry.n_ctx), ("alias", "upstream", "cpu", 64));
        assert_eq!(entry.weight_bytes, 0);
        assert_eq!(entry.upstream, remote.url);

        let request =
            SynthesizeRequest { sampler: Some(proto::SamplerConfig { temperature: Some(0.0), ..Default::default() }), ..synthesis("hello") };
        let tokens: Vec<_> = client.synthesize(request.clone()).await.unwrap().into_inner().collect().await;
        let tokens: Vec<_> = tokens.into_iter().collect::<Result<_, _>>().unwrap();
        let direct: Vec<_> = fixture
            .client(&remote.url)
            .synthesize(SynthesizeRequest { model: "tiny".into(), ..request })
            .await
            .unwrap()
            .into_inner()
            .collect()
            .await;
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens, direct.into_iter().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(client.decide(decision()).await.unwrap_err().code(), Code::InvalidArgument);
    }
}

enum Call {
    Synthesize(Request<SynthesizeRequest>),
    Decide(Request<DecideRequest>),
}

struct Peer {
    calls: mpsc::UnboundedSender<Call>,
    status: IntrospectResponse,
    stream_dropped: Arc<Notify>,
}

fn peer() -> (Peer, mpsc::UnboundedReceiver<Call>) {
    let (calls, received) = mpsc::unbounded_channel();
    let status = IntrospectResponse {
        ready: true,
        default_model: "remote-decoder".into(),
        models: vec![
            ModelStatus {
                name: "remote-decoder".into(),
                kind: "gguf".into(),
                architecture: "remote-arch".into(),
                backend: "metal".into(),
                n_ctx: 4096,
                ready: true,
                ..Default::default()
            },
            ModelStatus {
                name: "remote-laya".into(),
                kind: "laya".into(),
                architecture: "encoder".into(),
                backend: "cpu".into(),
                n_ctx: 512,
                ready: false,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    (Peer { calls, status, stream_dropped: Arc::new(Notify::new()) }, received)
}

impl Mdma for Peer {
    type SynthesizeStream = TokenStream;

    async fn synthesize(&self, request: Request<SynthesizeRequest>) -> Result<Response<TokenStream>, Status> {
        let message = request.get_ref().clone();
        self.calls.send(Call::Synthesize(request)).unwrap();
        if message.prompt == "fail" {
            return Err(Status::resource_exhausted("remote context is full"));
        }
        if message.prompt == "hold" {
            return Ok(Response::new(Box::pin(HeldStream { first: true, dropped: self.stream_dropped.clone() })));
        }
        let mut tokens =
            vec![Ok(SynthesizeResponse { token_id: 42, text: "hello".into() }), Ok(SynthesizeResponse { token_id: 999, text: "é".into() })];
        if message.prompt == "midstream" {
            tokens.push(Err(Status::data_loss("remote stream failed")));
        }
        Ok(Response::new(Box::pin(tokio_stream::iter(tokens))))
    }

    async fn decide(&self, request: Request<DecideRequest>) -> Result<Response<DecideResponse>, Status> {
        let result_json = request.get_ref().state_json.clone();
        self.calls.send(Call::Decide(request)).unwrap();
        if result_json == "invalid" {
            return Err(Status::invalid_argument("remote rejected state"));
        }
        Ok(Response::new(DecideResponse { result_json }))
    }

    async fn introspect(&self, _: Request<IntrospectRequest>) -> Result<Response<IntrospectResponse>, Status> {
        Ok(Response::new(self.status.clone()))
    }
}

struct HeldStream {
    first: bool,
    dropped: Arc<Notify>,
}

impl Stream for HeldStream {
    type Item = Result<SynthesizeResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if std::mem::take(&mut self.first) {
            Poll::Ready(Some(Ok(SynthesizeResponse { token_id: 1, text: "held".into() })))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for HeldStream {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

#[tokio::test]
async fn upstream_preserves_requests_tokens_and_remote_errors() {
    let fixture = Fixture::new();
    let (peer, mut received) = peer();
    let remote = fixture.start(|_| peer).await;
    let gateway = fixture.start(|_| fixture.service(&remote.url, "remote-decoder")).await;
    let client = fixture.client(&gateway.url);
    let message = SynthesizeRequest {
        sampler: Some(proto::SamplerConfig { temperature: Some(0.0), repetition_window: Some(0), ..Default::default() }),
        ..synthesis("hello")
    };
    let mut request = Request::new(message.clone());
    request.metadata_mut().insert("x-request-id", "trace-1".parse().unwrap());
    let tokens: Vec<_> = client.synthesize(request).await.unwrap().into_inner().collect().await;
    assert_eq!(
        tokens.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
        vec![SynthesizeResponse { token_id: 42, text: "hello".into() }, SynthesizeResponse { token_id: 999, text: "é".into() },]
    );
    let Call::Synthesize(request) = received.recv().await.unwrap() else { panic!("expected Synthesize") };
    assert_eq!(request.metadata().get("x-request-id").unwrap(), "trace-1");
    assert_eq!(request.metadata().get("x-mdma-hops").unwrap(), "1");
    assert_eq!(request.into_inner(), SynthesizeRequest { model: "remote-decoder".into(), ..message });

    let error = client.synthesize(synthesis("fail")).await.err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert_eq!(error.message(), "remote context is full");
    let mut stream = client.synthesize(synthesis("midstream")).await.unwrap().into_inner();
    assert_eq!(stream.next().await.unwrap().unwrap().token_id, 42);
    assert_eq!(stream.next().await.unwrap().unwrap().token_id, 999);
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(error.code(), Code::DataLoss);
    assert_eq!(error.message(), "remote stream failed");
}

#[tokio::test]
async fn upstream_decide_and_introspect_select_the_remote_model() {
    let fixture = Fixture::new();
    let (peer, mut received) = peer();
    let remote = fixture.start(|_| peer).await;
    let gateway = fixture.start(|_| fixture.service(&remote.url, "remote-laya")).await;
    let client = fixture.client(&gateway.url);
    let message = decision();
    let response = client.decide(message.clone()).await.unwrap().into_inner();
    assert_eq!(response.result_json, message.state_json);
    let Call::Decide(request) = received.recv().await.unwrap() else { panic!("expected Decide") };
    assert_eq!(request.metadata().get("x-mdma-hops").unwrap(), "1");
    assert_eq!(request.into_inner(), DecideRequest { model: "remote-laya".into(), ..message });
    let error = client.decide(DecideRequest { state_json: "invalid".into(), ..decision() }).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(error.message(), "remote rejected state");

    let health = client.introspect(IntrospectRequest {}).await.unwrap().into_inner();
    assert!(health.ready);
    assert!(!health.models[0].ready);
    assert_eq!(health.models[0].architecture, "encoder");
    assert_eq!(health.models[0].n_ctx, 512);
    let default = fixture.service(&remote.url, "").introspect(Request::new(IntrospectRequest {})).await.unwrap().into_inner();
    assert!(default.models[0].ready);
    assert_eq!(default.models[0].backend, "metal");
    assert_eq!(default.models[0].architecture, "remote-arch");
    let missing = fixture.service(&remote.url, "missing").introspect(Request::new(IntrospectRequest {})).await.unwrap().into_inner();
    assert!(!missing.models[0].ready);
}

#[tokio::test]
async fn dead_or_untrusted_upstreams_are_unavailable() {
    let fixture = Fixture::new();
    let (peer, _received) = peer();
    let remote = fixture.start(|_| peer).await;
    let untrusted = Fixture::new();
    let service = untrusted.service(&remote.url, "");
    assert_eq!(service.synthesize(Request::new(synthesis("hello"))).await.err().unwrap().code(), Code::Unavailable);
    assert!(!service.introspect(Request::new(IntrospectRequest {})).await.unwrap().into_inner().models[0].ready);
    std::fs::write(untrusted.dir.join("ca.pem"), &fixture.certs.ca_pem).unwrap();
    let service = untrusted.service(&remote.url, "");
    assert_eq!(service.decide(Request::new(decision())).await.unwrap_err().code(), Code::Unavailable);

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://localhost:{}", port.local_addr().unwrap().port());
    drop(port);
    let service = fixture.service(&url, "");
    let health = service.introspect(Request::new(IntrospectRequest {})).await.unwrap().into_inner();
    assert!(health.ready);
    assert!(!health.models[0].ready);
    assert_eq!(service.synthesize(Request::new(synthesis("hello"))).await.err().unwrap().code(), Code::Unavailable);
    assert_eq!(service.decide(Request::new(decision())).await.unwrap_err().code(), Code::Unavailable);
}

#[tokio::test]
async fn upstream_obeys_local_admission_and_drains_until_stream_drop() {
    let fixture = Fixture::new();
    let (peer, mut received) = peer();
    let dropped = peer.stream_dropped.clone();
    let remote = fixture.start(|_| peer).await;
    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let weak = shutdown.guard_weak();
    let (power, status) = tokio::sync::watch::channel(on_battery(ServerState::Degraded, 60));
    let service = fixture
        .service(&remote.url, "remote-decoder")
        .with_shutdown_guard(weak.clone())
        .with_power(status, DegradedLimits { max_tokens: 2, n_ctx: 64 });
    let mut stream = service.synthesize(Request::new(synthesis("hold"))).await.unwrap().into_inner();
    assert!(stream.next().await.unwrap().is_ok());
    let Call::Synthesize(request) = received.recv().await.unwrap() else { panic!("expected Synthesize") };
    assert_eq!(request.get_ref().max_tokens, 2);
    power.send(on_battery(ServerState::Critical, 10)).unwrap();
    assert_eq!(service.synthesize(Request::new(synthesis("hello"))).await.err().unwrap().code(), Code::Unavailable);
    assert_eq!(service.decide(Request::new(decision())).await.unwrap_err().code(), Code::Unavailable);
    assert!(received.try_recv().is_err());
    trigger.cancel();
    weak.cancelled().await;
    let drain = shutdown.shutdown_with_limit(Duration::from_secs(5));
    tokio::pin!(drain);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut drain).await.is_err());
    drop(stream);
    drain.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), dropped.notified()).await.expect("remote stream must be dropped");
}

#[tokio::test]
async fn cyclic_upstreams_stop_at_the_hop_limit() {
    let fixture = Fixture::new();
    let node = fixture.start(|url| fixture.service(url, "alias")).await;
    let client = fixture.client(&node.url);
    let error = tokio::time::timeout(Duration::from_secs(5), client.synthesize(synthesis("hello"))).await.unwrap().err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
    let error = tokio::time::timeout(Duration::from_secs(5), client.decide(decision())).await.unwrap().unwrap_err();
    assert_eq!(error.code(), Code::ResourceExhausted);
    let health = tokio::time::timeout(Duration::from_secs(5), client.introspect(IntrospectRequest {})).await.unwrap().unwrap().into_inner();
    assert!(!health.models[0].ready);
}
