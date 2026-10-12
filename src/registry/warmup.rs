//! Each load exercises its decoder before the first request.
use crate::backend::{self, Backend};
use crate::model::Model;
use crate::sampler::greedy;

pub const MAX_WARMUP_ROUNDS: u32 = 8;
const PREFILL_TOKENS: usize = 16;
const DECODE_STEPS: usize = 4;

/// Up to 16 synthetic prefill tokens and four decode steps per round, within
/// `n_ctx`. Logits readback joins device execution; all KV, workspace and graph
/// state is temporary. Zero rounds does nothing. `Ok(false)` means cancelled.
pub(super) fn warm_decoder<B: Backend>(
    backend: &B,
    model: &Model<B>,
    n_ctx: usize,
    rounds: u32,
    cancelled: &mut impl FnMut() -> bool,
) -> backend::Result<bool> {
    if rounds == 0 || cancelled() {
        return Ok(!cancelled());
    }
    let capacity = n_ctx
        .min(model.config().n_ctx)
        .min(PREFILL_TOKENS + DECODE_STEPS);
    if capacity == 0 {
        return Err(backend::Error::ShapeMismatch(
            "warm-up requires a nonzero decoder context".into(),
        ));
    }
    let steps = DECODE_STEPS.min(capacity - 1);
    let tokens = vec![0; capacity - steps]; // Token zero is valid in every nonempty vocabulary.
    let mut kv = model.new_kv_cache(backend, capacity)?;
    let mut prefill_ws = model.alloc_workspace(backend, tokens.len())?;
    let mut decode_ws = if steps > 0 {
        Some(model.alloc_workspace(backend, 1)?)
    } else {
        None
    };
    for _ in 0..rounds {
        if cancelled() {
            return Ok(false);
        }
        kv.set_len(0);
        let mut token =
            next_token(&model.prefill(backend, &mut kv, &mut prefill_ws, &tokens, 0)?)?;
        if steps > 0 {
            if cancelled() {
                return Ok(false);
            }
            // Use the same eager/graph factory as requests. Graphs borrow these
            // temporary buffers and are dropped before the next prefill.
            let mut decoder = backend.decoder(
                model,
                &mut kv,
                decode_ws
                    .as_mut()
                    .expect("decode workspace exists when steps > 0"),
            )?;
            for step in 0..steps {
                if cancelled() {
                    return Ok(false);
                }
                let logits = decoder.forward(token, (tokens.len() + step) as u32)?;
                token = next_token(&logits)?;
            }
        }
    }
    Ok(!cancelled())
}

fn next_token(logits: &[f32]) -> backend::Result<u32> {
    if logits.is_empty() || logits.iter().any(|value| !value.is_finite()) {
        return Err(backend::Error::Backend(
            "warm-up produced empty or nonfinite logits".into(),
        ));
    }
    Ok(greedy(logits))
}

#[cfg(test)]
mod tests;
