//! Shared checkpoint-gate steps for decoder tests.
use super::Model;
use crate::backend::Backend;
use crate::{sampler::greedy, tokenizer::Tokenizer};

/// Prefills `prompt` at position 0 and greedy-decodes, asserting each token
/// against `expected`, which may end in EOS. Returns the prompt ids and the
/// prefill logits.
pub(crate) fn assert_greedy<B: Backend>(
    b: &B,
    model: &Model<B>,
    tok: &Tokenizer,
    prompt: &str,
    expected: &[u32],
) -> (Vec<u32>, Vec<f32>) {
    let ids = tok.try_encode(prompt).unwrap();
    let mut kv = model.new_kv_cache(b, ids.len() + expected.len()).unwrap();
    let mut ws = model.alloc_workspace(b, ids.len()).unwrap();
    let prefill = model.prefill(b, &mut kv, &mut ws, &ids, 0).unwrap();
    drop(ws);
    let mut decode = model.alloc_workspace(b, 1).unwrap();
    let mut logits = prefill.clone();
    for (i, &target) in expected.iter().enumerate() {
        assert!(logits.iter().all(|v| v.is_finite()));
        let id = greedy(&logits);
        assert_eq!(id, target, "{prompt:?}: output position {i}");
        if tok.is_eos(id) {
            assert_eq!(i + 1, expected.len(), "unexpected early EOS");
            break;
        }
        if i + 1 < expected.len() {
            logits = model
                .forward(b, &mut kv, &mut decode, id, (ids.len() + i) as u32)
                .unwrap();
        }
    }
    (ids, prefill)
}
