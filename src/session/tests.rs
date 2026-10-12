use super::{Session, SessionError};
use crate::backend::cpu::CpuBackend;
use crate::backend::test_support::fixture;
use crate::gguf::GgufModel;
use crate::model::Model;
use crate::sampler::SamplerConfig;
use crate::tokenizer::Tokenizer;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "execution/tests.rs"]
mod execution;

const EXPECTED_CONTINUATION: &[u32] = &[12247, 281, 12247, 505, 395];

fn load_session() -> Session<CpuBackend> {
    let backend = Arc::new(CpuBackend);
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Arc::new(Model::load(&*backend, &mut gguf).expect("load model"));
    let tokenizer = Arc::new(Tokenizer::from_gguf(&gguf).expect("build tokenizer"));
    let greedy = SamplerConfig {
        temperature: 0.0,
        top_p: 1.0,
        min_p: 0.0,
        repetition_penalty: 1.0,
        repetition_window: 0,
    };
    let n_ctx = model.config().n_ctx;
    Session::new(backend, model, tokenizer, greedy, n_ctx)
}

#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
async fn generate_streams_the_expected_greedy_continuation() {
    let session = load_session();
    let stream = session
        .generate(
            "The capital of France is",
            EXPECTED_CONTINUATION.len(),
            CancellationToken::new(),
        )
        .expect("generate");

    let ids: Vec<u32> = stream.map(|t| t.expect("token").token_id).collect().await;
    assert_eq!(ids, EXPECTED_CONTINUATION);
}

#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
async fn generate_stops_immediately_on_a_precancelled_token() {
    let session = load_session();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let stream = session
        .generate("The capital of France is", 5, cancel)
        .expect("generate");
    let items: Vec<_> = stream.collect().await;
    assert!(items.is_empty());
}

#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
async fn dropping_stream_before_any_token_cancels_during_prefill() {
    let session = load_session();
    let cancel = CancellationToken::new();
    let stream = session
        .generate("The capital of France is", 5, cancel.clone())
        .expect("generate");

    drop(stream);

    tokio::time::timeout(std::time::Duration::from_secs(5), cancel.cancelled())
        .await
        .expect("cancel token should fire promptly on stream drop, even with zero tokens consumed");
}

#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
async fn dropping_stream_after_one_token_still_cancels() {
    let session = load_session();
    let cancel = CancellationToken::new();
    let mut stream = session
        .generate("The capital of France is", 5, cancel.clone())
        .expect("generate");

    let _first = stream
        .next()
        .await
        .expect("at least one token before EOS/max_tokens");
    drop(stream);

    tokio::time::timeout(std::time::Duration::from_secs(5), cancel.cancelled())
        .await
        .expect("cancel token should fire promptly on stream drop");
}

#[test]
fn invalid_transition_error_supports_equality() {
    let err = SessionError::InvalidTransition;
    assert_eq!(err, SessionError::InvalidTransition);
}

#[cfg(feature = "rocm")]
#[tokio::test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
async fn gpu_cached_session_handles_repetition_concurrency_cancellation_and_eviction() {
    use crate::backend::rocm::HipBackend;
    let _guard = crate::backend::rocm::tests::GPU_MODEL_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let backend = Arc::new(HipBackend::new().unwrap());
    let mut gguf = GgufModel::open(fixture()).unwrap();
    let model = Arc::new(Model::load(&*backend, &mut gguf).unwrap());
    let tokenizer = Arc::new(Tokenizer::from_gguf(&gguf).unwrap());
    let config = SamplerConfig {
        temperature: 0.0,
        top_p: 1.0,
        min_p: 0.0,
        repetition_penalty: 1.0,
        repetition_window: 0,
    };
    let session = Session::new(backend, model, tokenizer, config, 64).with_prefix_reuse();
    let prompt = "The capital of France is";
    for _ in 0..2 {
        let stream = session
            .generate(prompt, 5, CancellationToken::new())
            .unwrap();
        let ids: Vec<u32> = stream.map(|t| t.unwrap().token_id).collect().await;
        assert_eq!(ids, EXPECTED_CONTINUATION);
    }
    let a = session
        .generate(prompt, 5, CancellationToken::new())
        .unwrap();
    let b = session
        .generate(prompt, 5, CancellationToken::new())
        .unwrap();
    let (a, b): (Vec<_>, Vec<_>) = tokio::join!(a.collect(), b.collect());
    for tokens in [a, b] {
        assert_eq!(
            tokens
                .into_iter()
                .map(|t| t.unwrap().token_id)
                .collect::<Vec<_>>(),
            EXPECTED_CONTINUATION
        );
    }
    let cancel = CancellationToken::new();
    cancel.cancel();
    let empty: Vec<_> = session.generate(prompt, 5, cancel).unwrap().collect().await;
    assert!(empty.is_empty());
    let cancel = CancellationToken::new();
    let mut stream = session.generate(prompt, 30, cancel.clone()).unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    session.clear_prefix_cache();
    drop(stream);
    assert!(cancel.is_cancelled());
    let ids: Vec<u32> = session
        .generate(prompt, 5, CancellationToken::new())
        .unwrap()
        .map(|t| t.unwrap().token_id)
        .collect()
        .await;
    assert_eq!(ids, EXPECTED_CONTINUATION);
    session.clear_prefix_cache();
}
