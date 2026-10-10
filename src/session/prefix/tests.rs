use super::*;
use crate::backend::cpu::CpuBackend;
use crate::model::tests::tiny_model;

fn acquire(backend: &CpuBackend, model: &Model<CpuBackend>, n_ctx: usize, slot: Option<SharedPrefix<CpuBackend>>) -> Result<PromptLease<CpuBackend>> {
    let mut lease = PromptLease::reserve(slot);
    lease.allocate(backend, model, n_ctx)?;
    Ok(lease)
}

#[test]
fn exact_extended_branched_shortened_and_unrelated_prompts_match_fresh() {
    let backend = CpuBackend;
    let model = tiny_model(&backend);
    let mut cached = PromptState::new(model.new_kv_cache(&backend, 64).unwrap());
    let cases: &[(&[u32], usize)] = &[
        (&[3, 5, 7, 11, 13, 17, 19], 0),
        (&[3, 5, 7, 11, 13, 17, 19], 7),
        (&[3, 5, 7, 11, 13, 17, 19, 23], 7),
        (&[3, 5, 7, 11, 13, 29, 31], 5),
        (&[3, 5], 1),
        (&[47, 31, 17], 0),
    ];
    for &(tokens, expected_reuse) in cases {
        let (got, reused) = cached.prefill(&backend, &model, tokens).unwrap();
        let mut kv = model.new_kv_cache(&backend, 64).unwrap();
        let mut ws = model.alloc_workspace(&backend, tokens.len()).unwrap();
        let want = model.prefill(&backend, &mut kv, &mut ws, tokens, 0).unwrap();
        assert_eq!(reused, expected_reuse);
        assert_eq!(got, want);
        // Appended generation must not contaminate later prompt hits.
        let mut decode_ws = model.alloc_workspace(&backend, 1).unwrap();
        model.forward(&backend, &mut cached.kv, &mut decode_ws, 97, tokens.len() as u32).unwrap();
        let (hit, reused) = cached.prefill(&backend, &model, tokens).unwrap();
        assert_eq!(hit, want);
        assert_eq!(reused, tokens.len());
        assert_eq!(cached.kv.len(), tokens.len());
    }
    assert!(cached.prefill(&backend, &model, &[]).is_err());
    assert!(cached.prefill(&backend, &model, &[1; 65]).is_err());
}

#[test]
fn leases_are_exclusive_and_clear_prevents_inflight_repopulation() {
    let backend = CpuBackend;
    let model = tiny_model(&backend);
    let slot = Arc::new(Mutex::new(PrefixSlot::new()));
    let mut first = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    first.state.as_mut().unwrap().prefill(&backend, &model, &[1, 3, 5]).unwrap();
    drop(first);
    assert!(slot.lock().unwrap().state.is_some());
    let first = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    let mut second = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    assert_eq!(first.state.as_ref().unwrap().tokens, [1, 3, 5]);
    assert!(second.state.as_ref().unwrap().tokens.is_empty());
    second.state.as_mut().unwrap().prefill(&backend, &model, &[7, 11]).unwrap();
    assert_eq!(first.state.as_ref().unwrap().tokens, [1, 3, 5]);
    slot.lock().unwrap().clear();
    drop(first);
    drop(second);
    assert!(slot.lock().unwrap().state.is_none());
}

#[test]
fn failed_prefill_is_not_cached_and_idle_storage_is_bounded() {
    let backend = CpuBackend;
    let model = tiny_model(&backend);
    let slot = Arc::new(Mutex::new(PrefixSlot::new()));
    let mut a = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    let mut b = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    a.state.as_mut().unwrap().prefill(&backend, &model, &[1, 3]).unwrap();
    b.state.as_mut().unwrap().prefill(&backend, &model, &[5, 7]).unwrap();
    drop(a);
    drop(b);
    assert_eq!(slot.lock().unwrap().state.as_ref().unwrap().tokens, [5, 7]);
    let mut bad = acquire(&backend, &model, 64, Some(slot.clone())).unwrap();
    assert!(bad.state.as_mut().unwrap().prefill(&backend, &model, &[5, u32::MAX]).is_err());
    drop(bad);
    assert!(slot.lock().unwrap().state.is_none());
}

#[test]
fn request_sized_leases_reuse_larger_caches_and_replace_smaller_ones() {
    let backend = CpuBackend;
    let model = tiny_model(&backend);
    let slot = Arc::new(Mutex::new(PrefixSlot::new()));
    let mut small = acquire(&backend, &model, 4, Some(slot.clone())).unwrap();
    small.state.as_mut().unwrap().prefill(&backend, &model, &[1, 3, 5]).unwrap();
    drop(small);
    let mut hit = acquire(&backend, &model, 3, Some(slot.clone())).unwrap();
    let state = hit.state.as_mut().unwrap();
    assert_eq!(state.kv.capacity(), 4);
    assert_eq!(state.prefill(&backend, &model, &[1, 3, 5]).unwrap().1, 3);
    drop(hit);
    let mut larger = acquire(&backend, &model, 8, Some(slot)).unwrap();
    let state = larger.state.as_mut().unwrap();
    assert_eq!(state.kv.capacity(), 8);
    assert!(state.tokens.is_empty());
    let tokens = &[1, 3, 5, 7, 9];
    let got = state.prefill(&backend, &model, tokens).unwrap();
    let mut fresh = PromptState::new(model.new_kv_cache(&backend, 8).unwrap());
    assert_eq!(got, fresh.prefill(&backend, &model, tokens).unwrap());
}

#[test]
fn clearing_a_reserved_lease_prevents_repopulation_before_allocation() {
    let backend = CpuBackend;
    let model = tiny_model(&backend);
    let slot = Arc::new(Mutex::new(PrefixSlot::new()));
    let mut queued = PromptLease::reserve(Some(slot.clone()));
    assert!(queued.state.is_none());
    slot.lock().unwrap().clear();
    queued.allocate(&backend, &model, 4).unwrap();
    queued.state.as_mut().unwrap().prefill(&backend, &model, &[1, 3, 5]).unwrap();
    drop(queued);
    assert!(slot.lock().unwrap().state.is_none());
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn gpu_reuse_matches_explicit_chunked_prefill_after_graph_decode() {
    use crate::backend::rocm::HipBackend;
    use crate::gguf::GgufModel;
    let _guard = crate::backend::rocm::tests::GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = HipBackend::new().unwrap();
    let model = Model::load(&backend, &mut GgufModel::open(crate::backend::test_support::fixture()).unwrap()).unwrap();
    let mut cached = PromptState::new(model.new_kv_cache(&backend, 64).unwrap());
    let mut reference = model.new_kv_cache(&backend, 64).unwrap();
    let cases: Vec<(Vec<u32>, usize)> = vec![
        ((1..=17).collect(), 0), ((1..=17).collect(), 17),
        ((1..=20).collect(), 17), (vec![1, 2, 3, 4, 5, 51, 52], 5),
        (vec![1, 2, 3, 4], 3), (vec![47, 59, 61], 0),
    ];
    let mut previous = Vec::new();
    for (tokens, reuse) in cases {
        let (got, reused) = cached.prefill(&backend, &model, &tokens).unwrap();
        assert_eq!(reused, reuse);
        let want = if reuse == tokens.len() {
            previous.clone()
        } else {
            reference.set_len(reuse);
            let mut pw = model.alloc_workspace(&backend, tokens.len() - reuse).unwrap();
            model.prefill(&backend, &mut reference, &mut pw, &tokens[reuse..], reuse as u32).unwrap()
        };
        assert_eq!(got, want);
        previous = want;
        let mut ws = model.alloc_workspace(&backend, 1).unwrap();
        let mut decoder = backend.decoder(&model, &mut cached.kv, &mut ws).unwrap();
        assert!(decoder.is_graph());
        decoder.forward(97, tokens.len() as u32).unwrap();
    }
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_prefix_reuse() {
    use crate::backend::rocm::HipBackend;
    use crate::gguf::GgufModel;
    use std::time::Instant;
    let _guard = crate::backend::rocm::tests::GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = HipBackend::new().unwrap();
    let model = Model::load(&backend, &mut GgufModel::open(crate::backend::test_support::fixture()).unwrap()).unwrap();
    let base: Vec<u32> = (1..=128).collect();
    let a: Vec<u32> = base.iter().copied().chain(129..=144).collect();
    let b: Vec<u32> = base.iter().copied().chain(145..=160).collect();
    let mut cached = PromptState::new(model.new_kv_cache(&backend, 256).unwrap());
    cached.prefill(&backend, &model, &a).unwrap();
    for trial in 0..5 {
        for mode in if trial % 2 == 0 { ["cold", "suffix", "exact"] } else { ["exact", "suffix", "cold"] } {
            let tokens = if cached.tokens == a { &b } else { &a };
            let mut fresh = PromptState::new(model.new_kv_cache(&backend, 256).unwrap());
            let start = Instant::now();
            let (logits, reused) = match mode {
                "cold" => fresh.prefill(&backend, &model, tokens).unwrap(),
                "suffix" => cached.prefill(&backend, &model, tokens).unwrap(),
                _ => {
                    let current = cached.tokens.clone();
                    cached.prefill(&backend, &model, &current).unwrap()
                }
            };
            let elapsed = start.elapsed();
            assert!(logits.iter().all(|x| x.is_finite()));
            eprintln!("PREFIX mode={mode} prompt=144 reused={reused} trial={trial} elapsed_ns={}", elapsed.as_nanos());
        }
    }
}

impl<B: Backend> PromptState<B> {
    pub fn prefill(&mut self, backend: &B, model: &Model<B>, tokens: &[u32]) -> Result<(Vec<f32>, usize)> {
        self.prefill_reserved(backend, model, tokens, None)
    }
}
