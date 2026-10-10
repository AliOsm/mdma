use super::{Decoder, Entry, Error, Registry, Result, Unavailable};
use crate::backend::{self, Backend};
use crate::sampler::greedy;
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_WARMUP_ROUNDS: u32 = 8;
const PREFILL_TOKENS: usize = 16;
const DECODE_STEPS: usize = 4;

impl<B: Backend> Registry<B> {
    fn warmup_decoders(&self) -> Vec<(usize, usize)> {
        let mut unique: Vec<(usize, usize)> = Vec::new();
        let mut identities = HashMap::new();
        for (index, loaded) in self.models.iter().enumerate() {
            if let Entry::Decoder(decoder) = &loaded.entry {
                let position = *identities.entry(Arc::as_ptr(&decoder.model)).or_insert_with(|| {
                    unique.push((index, decoder.n_ctx));
                    unique.len() - 1
                });
                unique[position].1 = unique[position].1.max(decoder.n_ctx);
            }
        }
        unique
    }

    /// Daemon warm-up: release all aliases of a decoder if its temporary buffers
    /// exhaust memory, then continue with the remaining models.
    pub fn warmup_available(&mut self, backend: &B, rounds: u32, mut cancelled: impl FnMut() -> bool) -> Result<bool> {
        if rounds > MAX_WARMUP_ROUNDS {
            return Err(Error::Config(format!("warm-up rounds must be between 0 and {MAX_WARMUP_ROUNDS}")));
        }
        if rounds == 0 {
            return Ok(!cancelled());
        }
        for (index, n_ctx) in self.warmup_decoders() {
            if cancelled() {
                return Ok(false);
            }
            let loaded = &self.models[index];
            let Entry::Decoder(decoder) = &loaded.entry else { continue; };
            let identity = Arc::as_ptr(&decoder.model);
            match warm_decoder(backend, decoder, n_ctx, rounds, &mut cancelled) {
                Ok(false) => return Ok(false),
                Ok(true) => {}
                Err(error) if error.is_memory_exhausted() => {
                    let reason = format!("warm-up: {error}");
                    for loaded in &mut self.models {
                        if let Entry::Decoder(decoder) = &loaded.entry
                            && Arc::as_ptr(&decoder.model) == identity
                        {
                            loaded.entry = Entry::Unavailable(Unavailable {
                                architecture: decoder.architecture.clone(), n_ctx: decoder.n_ctx, reason: reason.clone(),
                            });
                        }
                    }
                }
                Err(source) => return Err(Error::Load { name: loaded.name.clone(), source: source.into() }),
            }
        }
        Ok(!cancelled())
    }

    /// Exercise each unique native decoder before admitting requests.
    ///
    /// Runs at most 16 synthetic prefill tokens and four decode steps per round,
    /// bounded by the largest context policy among aliases. Logits readback joins
    /// device execution. All KV, workspace and graph state is temporary; weights
    /// stay resident. Existing tensor/buffer budgets apply.
    ///
    /// Zero rounds disables warm-up; values above `MAX_WARMUP_ROUNDS` are rejected.
    /// Laya and upstream entries are not exercised. `Ok(false)` means cancellation
    /// was observed between operations: the caller must not report readiness.
    pub fn warmup(&self, backend: &B, rounds: u32, mut cancelled: impl FnMut() -> bool) -> Result<bool> {
        if rounds > MAX_WARMUP_ROUNDS {
            return Err(Error::Config(format!("warm-up rounds must be between 0 and {MAX_WARMUP_ROUNDS}")));
        }
        if rounds == 0 {
            return Ok(!cancelled());
        }
        for (index, n_ctx) in self.warmup_decoders() {
            if cancelled() {
                return Ok(false);
            }
            let loaded = &self.models[index];
            let Entry::Decoder(decoder) = &loaded.entry else { continue; };
            let complete = warm_decoder(backend, decoder, n_ctx, rounds, &mut cancelled)
                .map_err(|source| Error::Load { name: loaded.name.clone(), source: source.into() })?;
            if !complete {
                return Ok(false);
            }
        }
        Ok(!cancelled())
    }
}

fn warm_decoder<B: Backend>(
    backend: &B, loaded: &Decoder<B>, n_ctx: usize, rounds: u32,
    cancelled: &mut impl FnMut() -> bool,
) -> backend::Result<bool> {
    let model = &loaded.model;
    let capacity = n_ctx.min(model.config().n_ctx).min(PREFILL_TOKENS + DECODE_STEPS);
    if capacity == 0 {
        return Err(backend::Error::ShapeMismatch("warm-up requires a nonzero decoder context".into()));
    }
    let steps = DECODE_STEPS.min(capacity - 1);
    let tokens = vec![0; capacity - steps]; // Token zero is valid in every nonempty vocabulary.
    let mut kv = model.new_kv_cache(backend, capacity)?;
    let mut prefill_ws = model.alloc_workspace(backend, tokens.len())?;
    let mut decode_ws = if steps > 0 { Some(model.alloc_workspace(backend, 1)?) } else { None };
    for _ in 0..rounds {
        if cancelled() {
            return Ok(false);
        }
        kv.set_len(0);
        let mut token = next_token(&model.prefill(backend, &mut kv, &mut prefill_ws, &tokens, 0)?)?;
        if steps > 0 {
            if cancelled() {
                return Ok(false);
            }
            // Use the same eager/graph factory as requests. Graphs borrow these
            // temporary buffers and are dropped before the next prefill.
            let mut decoder = backend.decoder(model, &mut kv, decode_ws.as_mut().expect("decode workspace exists when steps > 0"))?;
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
        return Err(backend::Error::Backend("warm-up produced empty or nonfinite logits".into()));
    }
    Ok(greedy(logits))
}

#[cfg(test)]
mod tests;
