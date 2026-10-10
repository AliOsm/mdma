//! Deterministic local generation, with optional CPU/GPU layer placement.
use crate::backend::{Backend, Error, Result};
use crate::{gguf::GgufModel, model::{Model, hybrid::HybridModel}, sampler::greedy, tokenizer::Tokenizer};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Generated {
    pub prompt_tokens: Vec<u32>,
    pub tokens: Vec<u32>,
    pub text: String,
    pub gpu_layers: Option<usize>,
    pub cpu_layers: Option<usize>,
}

/// Validates context before uploading weights. `Some(n)` selects eager hybrid
/// execution; `None` retains the backend's ordinary decoder/graph selection.
pub fn generate<B: Backend>(b: &B, g: &mut GgufModel, prompt: &str, max: usize,
    gpu_layers: Option<usize>) -> Result<Generated> {
    generate_impl(b, g, prompt, max, gpu_layers, false)
}

/// Local planning and generation use the same source layout. Does not attach
/// an environment-selected prepared cache; serving's cache policy is unchanged.
pub fn generate_raw<B: Backend>(b: &B, g: &mut GgufModel, prompt: &str, max: usize,
    gpu_layers: Option<usize>) -> Result<Generated> {
    generate_impl(b, g, prompt, max, gpu_layers, true)
}

fn generate_impl<B: Backend>(b: &B, g: &mut GgufModel, prompt: &str, max: usize,
    gpu_layers: Option<usize>, raw: bool) -> Result<Generated> {
    let c = if let Some(count) = gpu_layers {
        crate::model::hybrid::validate(b, g, count)?
    } else { Model::validate(b, g)? };
    let tokenizer = Tokenizer::from_gguf(g).ok_or_else(|| Error::UnsupportedModel("unsupported or invalid tokenizer".into()))?;
    let ids = tokenizer.try_encode(prompt)?;
    let capacity = ids.len().checked_add(max).ok_or(Error::SizeOverflow)?;
    if ids.is_empty() || max == 0 || capacity > c.n_ctx || capacity > u32::MAX as usize {
        return Err(Error::ContextLimit { prompt_tokens: ids.len(), n_ctx: c.n_ctx });
    }
    let tokens = if let Some(count) = gpu_layers {
        let model = HybridModel::load(b, g, count)?;
        let mut kv = model.new_kv_cache(b, capacity)?;
        let mut prefill = model.alloc_workspace(b, ids.len())?;
        let logits = model.prefill(b, &mut kv, &mut prefill, &ids, 0)?;
        drop(prefill);
        let mut ws = model.alloc_workspace(b, 1)?;
        sample(&tokenizer, logits, ids.len(), max, |token, pos| model.forward(b, &mut kv, &mut ws, token, pos))?
    } else {
        let model = if raw { Model::load_reserved(b, g, None)? } else { Model::load(b, g)? };
        let mut kv = model.new_kv_cache(b, capacity)?;
        let mut prefill = model.alloc_workspace(b, ids.len())?;
        let logits = model.prefill(b, &mut kv, &mut prefill, &ids, 0)?;
        drop(prefill);
        let mut ws = model.alloc_workspace(b, 1)?;
        let mut decoder = b.decoder(&model, &mut kv, &mut ws)?;
        sample(&tokenizer, logits, ids.len(), max, |token, pos| decoder.forward(token, pos))?
    };
    Ok(Generated { text: tokenizer.decode(&tokens), prompt_tokens: ids, tokens,
        cpu_layers: gpu_layers.map(|n| c.n_layers - n), gpu_layers })
}

fn sample(tokenizer: &Tokenizer, mut logits: Vec<f32>, prompt: usize, max: usize,
    mut forward: impl FnMut(u32, u32) -> Result<Vec<f32>>) -> Result<Vec<u32>> {
    let mut tokens = Vec::new();
    for i in 0..max {
        if logits.is_empty() || logits.iter().any(|x| !x.is_finite()) {
            return Err(Error::Backend("empty or nonfinite logits".into()));
        }
        let token = greedy(&logits);
        if tokenizer.is_eos(token) { break; }
        tokens.push(token);
        if i + 1 < max { logits = forward(token, (prompt + i) as u32)?; }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests;
