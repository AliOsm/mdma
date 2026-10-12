use super::*;
use crate::backend::Backend;
use crate::backend::test_support::fixture;
use crate::backend::test_support::graph::{concurrent_prefill_replay, replay_matches_eager};
use crate::gguf::GgufModel;

const PROMPT: &[u32] = &[1, 918, 6872, 446, 9760, 505];
const EXPECTED: &[u32] = &[12247, 281, 12247, 505, 395];

// The ROCm direct-capture path creates its own backend; do not substitute
// the production factory, which is allowed to fall back to eager.
fn capture<'a>(
    backend: &'a HipBackend,
    model: &'a Model<HipBackend>,
    kv: &'a mut KvCache<HipBackend>,
    ws: &'a mut Workspace<HipBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    let state = GraphState::capture(backend, model, kv, ws)?;
    Ok(Box::new(HipDecoder {
        state,
        model,
        kv,
        ws,
    }))
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn graph_matches_eager_with_changing_tokens_positions_and_sessions() {
    let _guard = crate::backend::rocm::tests::GPU_MODEL_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let backend = HipBackend::new().unwrap();
    let model = Model::load(&backend, &mut GgufModel::open(fixture()).unwrap()).unwrap();
    replay_matches_eager(&backend, &model, capture, PROMPT, EXPECTED, 70, 128);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn concurrent_prefill_capture_and_replay_keep_requests_isolated() {
    let _guard = crate::backend::rocm::tests::GPU_MODEL_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let backend = HipBackend::new().unwrap();
    let model = Model::load(&backend, &mut GgufModel::open(fixture()).unwrap()).unwrap();
    concurrent_prefill_replay(&backend, &model, HipBackend::decoder, 64, PROMPT, EXPECTED);
}
