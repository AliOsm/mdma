use super::{proto, sampler_config_from_proto, MdmaServer, MdmaService};
use crate::backend::cpu::CpuBackend;
use crate::gguf::GgufModel;
use crate::model::Model;
use crate::registry::{Decoder, Registry};
use crate::tokenizer::Tokenizer;
use rama::http::client::EasyHttpConnectorBuilder;
use rama::http::server::HttpServer;
use rama::rt::Executor;
use rama::tcp::server::TcpListener;
use rama::tls::client::{ClientAuth, ClientAuthData, TlsClientConfig, TlsServerTrust, TlsServerTrustAnchors};
use rama::tls::rustls::server::TlsAcceptorLayer;
use rama::tls::server::{ClientVerifyMode, ServerAuthData, TlsServerConfig};
use rama::Layer;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;
use tokio_stream::StreamExt;

mod upstream;
mod transport;
mod native;

#[tokio::test]
async fn machine_api_has_no_old_rpc_aliases() {
    use rama::Service;
    use rama::http::Body;

    let server = MdmaServer::new(tiny_service(crate::power::PowerStatus::default(), crate::power::DegradedLimits::default()));
    for method in ["Generate", "Predict", "Health"] {
        let request = rama::http::Request::builder()
            .method("POST")
            .uri(format!("/mdma.Mdma/{method}"))
            .header("content-type", "application/grpc")
            .body(Body::empty())
            .unwrap();
        let response = server.serve(request).await.unwrap();
        assert_eq!(response.headers().get("grpc-status").unwrap(), "12", "{method}");
    }
}

#[test]
fn runtime_oom_is_resource_exhausted_not_internal() {
    let status = super::native_status(crate::backend::Error::OutOfMemory("allocation race".into()));
    assert_eq!(status.code(), rama::http::grpc::Code::ResourceExhausted);
}

#[tokio::test]
async fn memory_pressure_keeps_introspect_available_with_no_ready_models() {
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};
    use crate::memory::NativeMemoryBudget;
    use crate::registry::ModelsFile;

    let file = crate::gguf::tests::Fixture::new(1, 8).with_tokenizer().write();
    let mut config = ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models[0].name = "minicheck".into();
    let budget = NativeMemoryBudget::new(0);
    let backend = Arc::new(CpuBackend);
    let mut registry = Registry::load_available_with_budget(&*backend, &config, 64, Some(&budget), |_| panic!("no upload")).unwrap();
    assert!(registry.warmup_available(&*backend, 2, || false).unwrap());
    let service = MdmaService::new(backend, registry, "cpu");
    for _ in 0..2 {
        let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
        assert!(!health.ready);
        assert_eq!(health.server_state, "degraded");
        assert_eq!(health.default_model, "minicheck");
        assert_eq!(health.models.len(), 1);
        assert_eq!(health.model_architecture, "internlm2");
        assert!(!health.models[0].ready);
    }
    for model in ["", "minicheck"] {
        let error = service.synthesize(Request::new(proto::SynthesizeRequest {
            prompt: "hello".into(), max_tokens: 1, sampler: None, model: model.into(),
        })).await.err().unwrap();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert!(error.message().contains("not loaded") && error.message().contains("memory budget exhausted"));
        let error = service.decide(Request::new(proto::DecideRequest {
            model: model.into(), state_json: "{}".into(), questions_json: "{}".into(),
        })).await.unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
    }
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 1, sampler: None, model: "unknown".into(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::NotFound);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn memory_pressure_preserves_default_routing_and_serves_fitting_models() {
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};
    use crate::memory::NativeMemoryBudget;
    use crate::registry::ModelsFile;

    let large = crate::gguf::tests::Fixture::new(8, 8).with_tokenizer().write();
    let small = crate::gguf::tests::Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(small.path()).unwrap()).unwrap();
    let mut config = ModelsFile::single_gguf(large.path().to_str().unwrap());
    config.models[0].name = "large".into();
    config.models.push(ModelsFile::single_gguf(small.path().to_str().unwrap()).models.remove(0));
    config.models[1].name = "small".into();
    let budget = NativeMemoryBudget::new(bytes + 8192);
    let backend = Arc::new(CpuBackend);
    let registry = Registry::load_available_with_budget(&*backend, &config, 64, Some(&budget), |_| {}).unwrap();
    let service = MdmaService::new(backend, registry, "cpu");
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert!(health.ready);
    assert_eq!(health.server_state, "degraded");
    assert_eq!(health.default_model, "large");
    assert!(!health.models[0].ready);
    assert!(health.models[1].ready);
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 0, sampler: None, model: String::new(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
    let stream = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 0, sampler: None, model: "small".into(),
    })).await.unwrap().into_inner();
    assert!(stream.collect::<Vec<_>>().await.is_empty());
}

#[test]
fn backend_buffer_exhaustion_is_resource_exhausted() {
    for domain in [crate::memory::MemoryDomain::Device, crate::memory::MemoryDomain::Host] {
        let status = super::native_status(crate::backend::Error::BufferBudget {
            domain, requested: 16, used: 100, limit: 100,
        });
        assert_eq!(status.code(), rama::http::grpc::Code::ResourceExhausted);
        assert!(status.message().contains(&format!("{domain} buffer budget")));
    }
}

#[test]
fn native_context_rejection_is_resource_exhausted_not_internal() {
    let status = super::native_status(crate::backend::Error::ContextLimit { prompt_tokens: 65, n_ctx: 64 });
    assert_eq!(status.code(), rama::http::grpc::Code::ResourceExhausted);
    assert!(status.message().contains("65"));
    assert!(status.message().contains("64"));
}

#[tokio::test]
async fn native_budget_exhaustion_is_resource_exhausted_before_streaming() {
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};
    use crate::memory::NativeMemoryBudget;
    use crate::registry::ModelsFile;

    let file = crate::gguf::tests::Fixture::new(1, 8).with_tokenizer().write();
    let gguf = GgufModel::open(file.path()).unwrap();
    let bytes = crate::model::memory::weight_bytes(&gguf).unwrap();
    let budget = NativeMemoryBudget::new(bytes);
    let backend = Arc::new(CpuBackend);
    let registry = Registry::load_with_budget(&*backend, &ModelsFile::single_gguf(file.path().to_str().unwrap()), 64, Some(&budget), |_| {}).unwrap();
    let service = MdmaService::new(backend, registry, "cpu");
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "abc".into(), max_tokens: 4, sampler: None, model: String::new(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert!(error.message().contains("native tensor memory budget"));
    assert_eq!(budget.used(), bytes);
    drop(service);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn shutdown_rejects_new_requests_but_drains_admitted_generation() {
    use proto::mdma_server::Mdma;
    use rama::graceful::Shutdown;
    use rama::http::grpc::{Code, Request};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let weak = shutdown.guard_weak();
    let backend = Arc::new(CpuBackend);
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let service = MdmaService::new(backend, tiny_registry(model, tokenizer), "cpu")
        .with_shutdown_guard(weak.clone());
    assert!(service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner().ready);
    let request = proto::SynthesizeRequest { prompt: "hello".into(), max_tokens: 40, sampler: None, model: String::new() };
    let stream = service.synthesize(Request::new(request.clone())).await.unwrap().into_inner();

    trigger.cancel();
    weak.cancelled().await;
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert!(!health.ready);
    assert_eq!(health.server_state, "draining");
    let error = service.synthesize(Request::new(request)).await.err().unwrap();
    assert_eq!(error.code(), Code::Unavailable);

    // Forty tokens cannot fit in the sixteen-slot output channel without a reader.
    let drain = shutdown.shutdown_with_limit(Duration::from_secs(2));
    tokio::pin!(drain);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut drain).await.is_err());
    let tokens: Vec<_> = stream.collect().await;
    assert_eq!(tokens.len(), 40);
    assert!(tokens.iter().all(Result::is_ok));
    drain.await.unwrap();
}

#[tokio::test]
async fn shutdown_waits_for_disconnected_generation_worker() {
    use proto::mdma_server::Mdma;
    use rama::graceful::Shutdown;
    use rama::http::grpc::Request;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let backend = Arc::new(CpuBackend);
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let service = MdmaService::new(backend, tiny_registry(model.clone(), tokenizer), "cpu")
        .with_shutdown_guard(shutdown.guard_weak());
    let stream = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 40, sampler: None, model: String::new(),
    })).await.unwrap().into_inner();
    drop(stream);
    drop(service);
    trigger.cancel();
    shutdown.shutdown_with_limit(Duration::from_secs(2)).await.unwrap();
    assert_eq!(Arc::strong_count(&model), 1, "worker must release its model before shutdown completes");
}

fn tiny_registry(model: Arc<Model<CpuBackend>>, tokenizer: Arc<Tokenizer>) -> Registry<CpuBackend> {
    Registry::single("tiny", Decoder { model, tokenizer, architecture: "tiny".into(), n_ctx: 64 })
}

fn tiny_service(status: crate::power::PowerStatus, limits: crate::power::DegradedLimits) -> MdmaService<CpuBackend> {
    tiny_service_ctx(status, limits, 64)
}

fn tiny_service_ctx(status: crate::power::PowerStatus, limits: crate::power::DegradedLimits, n_ctx: usize) -> MdmaService<CpuBackend> {
    let backend = Arc::new(CpuBackend);
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let (tx, rx) = tokio::sync::watch::channel(status);
    // The receiver keeps the last value after the sender is gone.
    drop(tx);
    let registry = Registry::single("tiny", Decoder { model, tokenizer, architecture: "tiny".into(), n_ctx });
    MdmaService::new(backend, registry, "cpu").with_power(rx, limits)
}

#[tokio::test]
async fn unknown_models_are_not_found_and_introspect_lists_the_registry() {
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};

    let service = tiny_service(crate::power::PowerStatus::default(), crate::power::DegradedLimits::default());
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert_eq!(health.default_model, "tiny");
    assert_eq!(health.models.len(), 1);
    assert_eq!((health.models[0].kind.as_str(), health.models[0].backend.as_str(), health.models[0].n_ctx), ("gguf", "cpu", 64));
    assert_eq!(health.model_architecture, "tiny");

    let request = proto::SynthesizeRequest { prompt: "hello".into(), max_tokens: 1, sampler: None, model: "nope".into() };
    let error = service.synthesize(Request::new(request)).await.err().unwrap();
    assert_eq!(error.code(), Code::NotFound);
    assert!(error.message().contains("tiny"), "{}", error.message());

    let decision = proto::DecideRequest { model: String::new(), state_json: "\"x\"".into(), questions_json: "{}".into() };
    let error = service.decide(Request::new(decision)).await.err().unwrap();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(error.message().contains("use Synthesize"));
}

fn on_battery(state: crate::power::ServerState, percent: u8) -> crate::power::PowerStatus {
    crate::power::PowerStatus {
        state,
        reading: Some(crate::power::PowerReading { on_battery: true, battery_percent: Some(percent), gpu: None }),
    }
}

#[test]
fn admission_propagates_tokenization_errors_and_preserves_lazy_rejection() {
    use crate::power::{DegradedLimits, ServerState};
    use rama::http::grpc::{Code, Status};

    let service = tiny_service(on_battery(ServerState::Degraded, 50), DegradedLimits::default());
    let error = service.admit(|| Err(Status::internal("tokenizer encoding failed")), 4, 64).unwrap_err();
    assert_eq!(error.code(), Code::Internal);
    assert_eq!(error.message(), "tokenizer encoding failed");

    for state in [ServerState::Serving, ServerState::Critical, ServerState::Draining] {
        let service = tiny_service(on_battery(state, 50), DegradedLimits::default());
        let result = service.admit(|| panic!("prompt counter must not run in {state:?}"), 4, 64);
        if state == ServerState::Serving {
            assert_eq!(result.unwrap(), (4, 64));
        } else {
            assert_eq!(result.unwrap_err().code(), Code::Unavailable);
        }
    }
}

#[tokio::test]
async fn gpu_capped_on_ac_reports_the_limit() {
    use crate::power::{DegradedLimits, GpuCap, PowerReading, PowerStatus, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::Request;

    let gpu = Some(GpuCap { limit_mw: 15_000, default_mw: 80_000 });
    let status = PowerStatus {
        state: ServerState::Degraded,
        reading: Some(PowerReading { on_battery: false, battery_percent: Some(100), gpu }),
    };
    let service = tiny_service(status, DegradedLimits::default());
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert_eq!(health.server_state, "degraded");
    assert!(!health.on_battery);
    assert_eq!((health.gpu_power_limit_mw, health.gpu_power_default_mw), (15_000, 80_000));
}

#[tokio::test]
async fn degraded_clamps_max_tokens_and_reports_power() {
    use crate::power::{DegradedLimits, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::Request;

    let service = tiny_service(on_battery(ServerState::Degraded, 63), DegradedLimits { max_tokens: 4, n_ctx: 64 });
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert!(health.ready, "degraded still serves");
    assert_eq!(health.server_state, "degraded");
    assert!(health.on_battery);
    assert_eq!(health.battery_percent, 63);

    let stream = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 40, sampler: None, model: String::new(),
    })).await.unwrap().into_inner();
    let tokens: Vec<_> = stream.collect().await;
    assert_eq!(tokens.len(), 4);
    assert!(tokens.iter().all(Result::is_ok));
}

#[tokio::test]
async fn degraded_rejects_prompts_over_the_reduced_context() {
    use crate::power::{DegradedLimits, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};

    let prompt = "hello hello hello hello";
    assert!(crate::tokenizer::tests::tiny_tokenizer().encode(prompt).len() > 2);
    let service = tiny_service(on_battery(ServerState::Degraded, 50), DegradedLimits { max_tokens: 4, n_ctx: 2 });
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: prompt.into(), max_tokens: 4, sampler: None, model: String::new(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
}

#[tokio::test]
async fn critical_refuses_new_generations() {
    use crate::power::{DegradedLimits, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};

    let service = tiny_service(on_battery(ServerState::Critical, 7), DegradedLimits::default());
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert!(!health.ready);
    assert_eq!(health.server_state, "critical");
    assert_eq!(health.battery_percent, 7);
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 4, sampler: None, model: String::new(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::Unavailable);
    assert!(error.message().contains("7%"), "{}", error.message());
}

#[tokio::test]
async fn degraded_budgets_prompt_and_output_together() {
    use crate::power::{DegradedLimits, PowerStatus, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::Request;

    let battery = on_battery(ServerState::Degraded, 50);
    for (status, requested, expected) in [
        (PowerStatus::default(), 40, 40),
        (battery, 40, 3),
        (battery, u32::MAX, 3),
        (battery, 3, 3),
        (battery, 2, 2),
        (battery, 0, 0),
    ] {
        let service = tiny_service(status, DegradedLimits { max_tokens: 4, n_ctx: 8 });
        let stream = service.synthesize(Request::new(proto::SynthesizeRequest {
            prompt: "hello".into(), max_tokens: requested, sampler: None, model: String::new(),
        })).await.unwrap().into_inner();
        let tokens: Vec<_> = stream.collect().await;
        assert!(tokens.iter().all(Result::is_ok), "{tokens:?}");
        assert_eq!(tokens.len(), expected, "{:?}, requested {requested}", status.state);
    }
}

#[tokio::test]
async fn degraded_budget_respects_server_and_model_context_caps() {
    use crate::power::{DegradedLimits, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::Request;

    // The tiny model has a 64-token context, regardless of the server setting.
    for (server_ctx, prompt_len) in [(8, 5), (128, 61)] {
        let service = tiny_service_ctx(
            on_battery(ServerState::Degraded, 50),
            DegradedLimits { max_tokens: 40, n_ctx: 128 },
            server_ctx,
        );
        let stream = service.synthesize(Request::new(proto::SynthesizeRequest {
            prompt: "a".repeat(prompt_len), max_tokens: 40, sampler: None, model: String::new(),
        })).await.unwrap().into_inner();
        let tokens: Vec<_> = stream.collect().await;
        assert!(tokens.iter().all(Result::is_ok), "{tokens:?}");
        assert_eq!(tokens.len(), 3);
    }
}

#[tokio::test]
async fn degraded_full_context_requires_zero_requested_output() {
    use crate::power::{DegradedLimits, ServerState};
    use proto::mdma_server::Mdma;
    use rama::http::grpc::{Code, Request};

    let service = tiny_service(
        on_battery(ServerState::Degraded, 50),
        DegradedLimits { max_tokens: 4, n_ctx: 5 },
    );
    let error = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 1, sampler: None, model: String::new(),
    })).await.err().unwrap();
    assert_eq!(error.code(), Code::ResourceExhausted);
    assert!(error.message().contains("no room for output"));

    let mut stream = service.synthesize(Request::new(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 0, sampler: None, model: String::new(),
    })).await.unwrap().into_inner();
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn without_a_watcher_introspect_reports_serving() {
    use proto::mdma_server::Mdma;
    use rama::http::grpc::Request;

    let backend = Arc::new(CpuBackend);
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let service = MdmaService::new(backend, tiny_registry(model, tokenizer), "cpu");
    let health = service.introspect(Request::new(proto::IntrospectRequest {})).await.unwrap().into_inner();
    assert!(health.ready);
    assert_eq!(health.server_state, "serving");
    assert!(!health.on_battery);
    assert_eq!(health.battery_percent, -1);
    assert_eq!((health.gpu_power_limit_mw, health.gpu_power_default_mw), (0, 0));
}

#[test]
fn missing_sampler_config_falls_back_to_default() {
    let cfg = sampler_config_from_proto(None);
    let default = crate::sampler::SamplerConfig::default();
    assert_eq!(cfg.temperature, default.temperature);
    assert_eq!(cfg.repetition_window, default.repetition_window);
}

#[test]
fn proto_fields_map_through_including_the_u32_to_usize_conversion() {
    let proto_cfg = proto::SamplerConfig {
        temperature: Some(0.5),
        top_p: Some(0.9),
        min_p: Some(0.1),
        repetition_penalty: Some(1.2),
        repetition_window: Some(32),
    };
    let cfg = sampler_config_from_proto(Some(proto_cfg));
    assert_eq!(cfg.temperature, 0.5);
    assert_eq!(cfg.top_p, 0.9);
    assert_eq!(cfg.min_p, 0.1);
    assert_eq!(cfg.repetition_penalty, 1.2);
    assert_eq!(cfg.repetition_window, 32usize);
}

/// The actual bug this guards against: a client that sends `Some(SamplerConfig{..})`
/// but only sets one field (e.g. from a client library that omits unset proto3
/// fields, or an over-eager `..Default::default()` on the client side) must not
/// have its *other* fields silently become zero - top_p=0.0 would collapse
/// nucleus sampling to near-greedy, not "use the default".
#[test]
fn partially_set_proto_config_falls_back_per_field_not_to_zero() {
    let proto_cfg = proto::SamplerConfig { temperature: Some(0.1), ..Default::default() };
    let cfg = sampler_config_from_proto(Some(proto_cfg));
    let default = crate::sampler::SamplerConfig::default();

    assert_eq!(cfg.temperature, 0.1);
    assert_eq!(cfg.top_p, default.top_p);
    assert_eq!(cfg.min_p, default.min_p);
    assert_eq!(cfg.repetition_penalty, default.repetition_penalty);
    assert_eq!(cfg.repetition_window, default.repetition_window);
    assert_ne!(cfg.top_p, 0.0, "top_p=0.0 would collapse nucleus sampling to near-greedy");
}

use crate::backend::test_support::fixture;
const EXPECTED_CONTINUATION: &[u32] = &[12247, 281, 12247, 505, 395];

struct TestCerts {
    ca_pem: String,
    client_pem: String,
    client_key_pem: String,
    ca_der: CertificateDer<'static>,
    server_cert: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    client_cert: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
}

fn generate_test_certs() -> TestCerts {
    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "mdma test CA");
    ca_params.distinguished_name = dn;
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign ca");
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let server_key = KeyPair::generate().expect("server key");
    let mut server_params = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).expect("server params");
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params.signed_by(&server_key, &issuer).expect("sign server cert");

    let client_key = KeyPair::generate().expect("client key");
    let mut client_params = CertificateParams::new(Vec::<String>::new()).expect("client params");
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params.signed_by(&client_key, &issuer).expect("sign client cert");

    TestCerts {
        ca_pem: ca_cert.pem(),
        client_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
        ca_der: ca_cert.der().clone(),
        server_cert: vec![server_cert.der().clone()],
        server_key: PrivateKeyDer::try_from(server_key.serialize_der()).expect("server key der"),
        client_cert: vec![client_cert.der().clone()],
        client_key: PrivateKeyDer::try_from(client_key.serialize_der()).expect("client key der"),
    }
}

#[tokio::test]
async fn shutdown_drains_real_mtls_transport() {
    use rama::graceful::Shutdown;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let certs = generate_test_certs();
    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let executor = Executor::graceful(shutdown.guard());
    let backend = Arc::new(CpuBackend);
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let service = MdmaService::new(backend, tiny_registry(model, tokenizer), "cpu")
        .with_shutdown_guard(shutdown.guard_weak());
    let server_tls = TlsServerConfig::new()
        .with_single_cert(ServerAuthData { private_key: certs.server_key, cert_chain: certs.server_cert, ocsp: None })
        .with_client_verify(ClientVerifyMode::ClientAuth(vec![certs.ca_der.clone()]))
        .with_alpn_http_auto();
    let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tcp_service = TlsAcceptorLayer::new(server_tls)
        .into_layer(HttpServer::auto(executor).service(MdmaServer::new(service)));
    let server = tokio::spawn(listener.serve(tcp_service));

    let client_tls = TlsClientConfig::new()
        .with_alpn_http_auto()
        .with_client_auth(ClientAuth::Single(ClientAuthData { private_key: certs.client_key, cert_chain: certs.client_cert }))
        .with_server_trust(TlsServerTrust::custom(TlsServerTrustAnchors::try_new(vec![certs.ca_der]).unwrap()));
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(client_tls)
        .with_default_http_connector::<rama::http::Body>(Executor::default())
        .without_connection_pool()
        .build_client::<rama::http::Body, rama::http::Body, _>();
    let client = proto::mdma_client::MdmaClient::new(connector, format!("https://localhost:{}", addr.port()).parse().unwrap());
    let response = client.synthesize(proto::SynthesizeRequest {
        prompt: "hello".into(), max_tokens: 40, sampler: None, model: String::new(),
    }).await.unwrap();
    let mut stream = response.into_inner();
    assert!(stream.next().await.unwrap().is_ok());
    trigger.cancel();
    let remaining = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>()).await.unwrap();
    assert_eq!(remaining.len(), 39);
    assert!(remaining.iter().all(Result::is_ok));
    shutdown.shutdown_with_limit(Duration::from_secs(2)).await.unwrap();
    server.await.unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err(), "listener must be closed after drain");
}

fn load_service() -> MdmaServer<MdmaService<CpuBackend>> {
    let backend = Arc::new(CpuBackend);
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Arc::new(Model::load(&*backend, &mut gguf).expect("load model"));
    let tokenizer = Arc::new(Tokenizer::from_gguf(&gguf).expect("build tokenizer"));
    let registry = Registry::single("minicheck", Decoder { model, tokenizer, architecture: "internlm2".into(), n_ctx: 4096 });
    let service = MdmaService::new(backend, registry, "cpu");
    MdmaServer::new(service)
}

#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf) and loopback networking"]
async fn synthesize_works_over_real_mtls() {
    let certs = generate_test_certs();
    let grpc_svc = load_service();

    let server_tls = TlsServerConfig::new()
        .with_single_cert(ServerAuthData {
            private_key: certs.server_key,
            cert_chain: certs.server_cert,
            ocsp: None,
        })
        .with_client_verify(ClientVerifyMode::ClientAuth(vec![certs.ca_der.clone()]))
        .with_alpn_http_auto();

    let executor = Executor::default();
    let listener = TcpListener::bind_address("127.0.0.1:0", executor.clone()).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let tcp_service = TlsAcceptorLayer::new(server_tls).into_layer(HttpServer::auto(executor.clone()).service(grpc_svc));
    tokio::spawn(async move {
        listener.serve(tcp_service).await;
    });

    let client_tls = TlsClientConfig::new()
        .with_alpn_http_auto()
        .with_client_auth(ClientAuth::Single(ClientAuthData { private_key: certs.client_key, cert_chain: certs.client_cert }))
        .with_server_trust(TlsServerTrust::custom(TlsServerTrustAnchors::try_new(vec![certs.ca_der]).expect("trust anchors")));

    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(client_tls)
        .with_default_http_connector::<rama::http::Body>(executor)
        .without_connection_pool()
        .build_client::<rama::http::Body, rama::http::Body, _>();

    let origin_uri: rama::http::grpc::codegen::net::uri::Uri =
        format!("https://localhost:{}", addr.port()).parse().expect("uri");
    let client = proto::mdma_client::MdmaClient::new(connector, origin_uri);

    let request = proto::SynthesizeRequest {
        prompt: "The capital of France is".to_string(),
        max_tokens: EXPECTED_CONTINUATION.len() as u32,
        sampler: Some(proto::SamplerConfig {
            temperature: Some(0.0),
            top_p: Some(1.0),
            min_p: Some(0.0),
            repetition_penalty: Some(1.0),
            repetition_window: Some(0),
        }),
        model: String::new(),
    };

    let response = client.synthesize(request).await.expect("synthesize rpc");
    let mut stream = response.into_inner();

    let mut ids = Vec::new();
    while let Some(item) = stream.next().await {
        ids.push(item.expect("stream item").token_id);
    }

    assert_eq!(ids, EXPECTED_CONTINUATION);
}
