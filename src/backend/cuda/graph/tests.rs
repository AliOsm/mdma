use super::*;
use crate::backend::cuda::tests::GPU_MODEL_LOCK;
use crate::backend::test_support::fixture;
use crate::backend::test_support::graph::{concurrent_prefill_replay, replay_matches_eager};
use crate::gguf::GgufModel;

const PROMPT: &[u32] = &[1, 918, 6872, 446, 9760, 505];
const EXPECTED: &[u32] = &[12247, 281, 12247, 505, 395];

// Deliberately bypass fallback and require actual capture for the exact gate.
fn capture<'a>(
    backend: &'a CudaBackend,
    model: &'a Model<CudaBackend>,
    kv: &'a mut KvCache<CudaBackend>,
    ws: &'a mut Workspace<CudaBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    let state = GraphState::capture(backend, model, kv, ws)?;
    Ok(Box::new(CudaDecoder { state, model, kv, ws }))
}

/// A fixed chunk makes eager and replay split attention at the same key
/// boundaries; replay's extra all-masked chunks add exact zeros, so the
/// logits must match bit for bit.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and CUDA hardware"]
fn graph_matches_eager_with_changing_tokens_positions_and_sessions() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = CudaBackend::new().unwrap().with_attn_chunk(37);
    let model = Model::load(&backend, &mut GgufModel::open(fixture()).unwrap()).unwrap();
    replay_matches_eager(&backend, &model, capture, PROMPT, EXPECTED, 70, 128);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and CUDA hardware"]
fn concurrent_prefill_capture_and_replay_keep_requests_isolated() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = CudaBackend::new().unwrap();
    let model = Model::load(&backend, &mut GgufModel::open(fixture()).unwrap()).unwrap();
    concurrent_prefill_replay(&backend, &model, CudaBackend::decoder, 4096, PROMPT, EXPECTED);
}
