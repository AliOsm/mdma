//! Shared greedy-generation assertions and benchmark setup.
use crate::backend::Backend;
use crate::kv_cache::KvCache;
use crate::model::{Model, Workspace};
use crate::sampler::greedy;

#[allow(dead_code)]
pub(crate) fn fixture_greedy_continuation<B: Backend>(backend: &B) {
    let mut gguf = crate::gguf::GgufModel::open(super::fixture()).expect("open gguf");
    let model = Model::load(backend, &mut gguf).expect("load model");
    assert_sequential_continuation(
        backend,
        &model,
        64,
        &[1, 918, 6872, 446, 9760, 505],
        &[12247, 281, 12247, 505, 395],
    );
}

fn assert_sequential_continuation<B: Backend>(
    backend: &B,
    model: &Model<B>,
    capacity: usize,
    prompt: &[u32],
    expected: &[u32],
) {
    let mut kv = model.new_kv_cache(backend, capacity).expect("kv");
    let mut ws = model.alloc_workspace(backend, 1).expect("workspace");
    let mut pos = 0u32;
    let mut logits = Vec::new();
    for &tok in prompt {
        logits = model
            .forward(backend, &mut kv, &mut ws, tok, pos)
            .expect("forward");
        pos += 1;
    }
    let generated = greedy_continuation(
        backend,
        model,
        &mut kv,
        &mut ws,
        logits,
        pos,
        expected.len(),
    );
    assert_eq!(generated, expected);
}

/// Deterministic benchmark prefix, entirely outside the timed decode region.
pub(crate) fn benchmark_prefix<B: Backend>(
    backend: &B,
    model: &Model<B>,
    kv: &mut KvCache<B>,
    prefix: u32,
) {
    if prefix > 0 {
        let tokens: Vec<u32> = (0..prefix).map(|i| i % 128 + 1).collect();
        let mut ws = model
            .alloc_workspace(backend, tokens.len())
            .expect("prefill workspace");
        model
            .prefill(backend, kv, &mut ws, &tokens, 0)
            .expect("prefix prefill");
    }
}

pub(crate) fn greedy_continuation<B: Backend>(
    backend: &B,
    model: &Model<B>,
    kv: &mut KvCache<B>,
    ws: &mut Workspace<B>,
    mut logits: Vec<f32>,
    mut pos: u32,
    steps: usize,
) -> Vec<u32> {
    let mut generated = Vec::new();
    let mut next = greedy(&logits);
    for _ in 0..steps {
        generated.push(next);
        // Append every emitted token to KV, including the final token.
        logits = model.forward(backend, kv, ws, next, pos).expect("forward");
        pos += 1;
        next = greedy(&logits);
    }
    generated
}

#[test]
fn continuation_preserves_tokens_positions_and_final_cache_write() {
    let backend = crate::backend::cpu::CpuBackend;
    let model = crate::model::tests::tiny_model(&backend);
    for steps in [0, 1, 4] {
        let mut actual_kv = model.new_kv_cache(&backend, 16).unwrap();
        let mut expected_kv = model.new_kv_cache(&backend, 16).unwrap();
        let mut actual_ws = model.alloc_workspace(&backend, 1).unwrap();
        let mut expected_ws = model.alloc_workspace(&backend, 1).unwrap();
        let mut logits = Vec::new();
        for (pos, token) in [1, 7, 13].into_iter().enumerate() {
            logits = model
                .forward(&backend, &mut actual_kv, &mut actual_ws, token, pos as u32)
                .unwrap();
            model
                .forward(
                    &backend,
                    &mut expected_kv,
                    &mut expected_ws,
                    token,
                    pos as u32,
                )
                .unwrap();
        }
        let actual = greedy_continuation(
            &backend,
            &model,
            &mut actual_kv,
            &mut actual_ws,
            logits.clone(),
            3,
            steps,
        );
        let mut expected = Vec::new();
        for pos in 3..3 + steps {
            let token = greedy(&logits);
            expected.push(token);
            logits = model
                .forward(
                    &backend,
                    &mut expected_kv,
                    &mut expected_ws,
                    token,
                    pos as u32,
                )
                .unwrap();
        }
        assert_eq!(actual, expected);
        assert_sequential_continuation(&backend, &model, 16, &[1, 7, 13], &expected);
        assert_eq!(actual_kv.len(), 3 + steps);
        let pos = (3 + steps) as u32;
        assert_eq!(
            model
                .forward(&backend, &mut actual_kv, &mut actual_ws, 19, pos)
                .unwrap(),
            model
                .forward(&backend, &mut expected_kv, &mut expected_ws, 19, pos)
                .unwrap(),
        );
    }
}

#[test]
fn benchmark_prefix_matches_explicit_prefill() {
    let backend = crate::backend::cpu::CpuBackend;
    let model = crate::model::tests::tiny_model(&backend);
    for prefix in [0, 1, 7] {
        let mut actual = model.new_kv_cache(&backend, 16).unwrap();
        let mut expected = model.new_kv_cache(&backend, 16).unwrap();
        benchmark_prefix(&backend, &model, &mut actual, prefix);
        if prefix > 0 {
            let tokens: Vec<u32> = (1..=prefix).collect();
            let mut ws = model.alloc_workspace(&backend, tokens.len()).unwrap();
            model
                .prefill(&backend, &mut expected, &mut ws, &tokens, 0)
                .unwrap();
        }
        assert_eq!(actual.len(), prefix as usize);
        let mut ws = model.alloc_workspace(&backend, 1).unwrap();
        assert_eq!(
            model
                .forward(&backend, &mut actual, &mut ws, 19, prefix)
                .unwrap(),
            model
                .forward(&backend, &mut expected, &mut ws, 19, prefix)
                .unwrap(),
        );
    }
}
