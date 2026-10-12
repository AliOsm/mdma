use super::Model;
use crate::backend::cpu::CpuBackend;
use crate::gguf::GgufModel;
use crate::sampler::greedy;

pub(crate) fn tiny_model<B: crate::backend::Backend>(backend: &B) -> Model<B> {
    super::arch::llama::tests::tiny_model(backend)
}

use crate::backend::test_support::fixture;

// "The capital of France is", including BOS.
const PROMPT: &[u32] = &[1, 918, 6872, 446, 9760, 505];
const EXPECTED: &[u32] = &[12247, 281, 12247, 505, 395];

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn matches_llama_cpp_greedy_continuation() {
    let backend = CpuBackend;
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let mut kv = model
        .new_kv_cache(&backend, model.config().n_ctx)
        .expect("new kv cache");
    let mut ws = model.alloc_workspace(&backend, 1).expect("alloc workspace");

    let mut pos: u32 = 0;
    let mut logits = Vec::new();
    for &tok in PROMPT {
        logits = model
            .forward(&backend, &mut kv, &mut ws, tok, pos)
            .expect("forward");
        pos += 1;
    }

    let mut generated = Vec::new();
    let mut next = greedy(&logits);
    for _ in 0..EXPECTED.len() {
        generated.push(next);
        logits = model
            .forward(&backend, &mut kv, &mut ws, next, pos)
            .expect("forward");
        pos += 1;
        next = greedy(&logits);
    }

    assert_eq!(generated, EXPECTED);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF (Bespoke-MiniCheck-7B-Q4_K_M.gguf)"]
fn prefill_matches_llama_cpp_greedy_continuation() {
    let backend = CpuBackend;
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let mut kv = model
        .new_kv_cache(&backend, model.config().n_ctx)
        .expect("new kv cache");
    let mut prefill_ws = model
        .alloc_workspace(&backend, PROMPT.len())
        .expect("alloc prefill workspace");
    let mut ws = model
        .alloc_workspace(&backend, 1)
        .expect("alloc decode workspace");

    let mut logits = model
        .prefill(&backend, &mut kv, &mut prefill_ws, PROMPT, 0)
        .expect("prefill");
    let mut generated = Vec::new();
    let mut next = greedy(&logits);
    for pos in (PROMPT.len() as u32..).take(EXPECTED.len()) {
        generated.push(next);
        logits = model
            .forward(&backend, &mut kv, &mut ws, next, pos)
            .expect("forward");
        next = greedy(&logits);
    }

    assert_eq!(generated, EXPECTED);
}
