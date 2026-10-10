//! Metadata-only admission for separate-Q/K/V Llama and InternLM2 layouts.
use super::Llama;
use crate::model::ModelConfig;
use crate::model::preflight::{unsupported, validate_tensor};
use crate::backend::{Backend, Result};
use crate::gguf::GgufModel;
use std::collections::HashSet;

#[cfg(test)]
mod tests;

const INTEGER_KEYS: &[&str] = &[
    "block_count",
    "embedding_length",
    "feed_forward_length",
    "attention.head_count",
    "attention.head_count_kv",
    "context_length",
];
const FLOAT_KEYS: &[&str] = &["rope.freq_base", "attention.layer_norm_rms_epsilon"];
const HEAD_KEYS: &[&str] = &["attention.key_length", "attention.value_length", "rope.dimension_count"];

impl<B: Backend> Llama<B> {
    pub(in crate::model) fn validate(backend: &B, gguf: &GgufModel, arch: &str) -> Result<ModelConfig> {
        let config = config(gguf, arch)?;
        if let Some(limit) = backend.max_head_dim()
            && config.head_dim > limit
        {
            return Err(unsupported(format!("head_dim {} exceeds backend limit {limit}", config.head_dim)));
        }
        layout(backend, gguf, &config, arch)?;
        Ok(config)
    }
}

fn config(gguf: &GgufModel, arch: &str) -> Result<ModelConfig> {
    for suffix in INTEGER_KEYS {
        let key = format!("{arch}.{suffix}");
        if gguf.metadata_u32(&key).is_none_or(|v| v == 0) {
            return Err(unsupported(format!("{key} must be a positive integer")));
        }
    }
    for suffix in FLOAT_KEYS {
        let key = format!("{arch}.{suffix}");
        if !gguf.metadata_f32(&key).is_some_and(|v| v.is_finite() && v > 0.0) {
            return Err(unsupported(format!("{key} must be a finite positive float")));
        }
    }
    let c = ModelConfig::from_gguf(gguf, arch)
        .ok_or_else(|| unsupported("invalid geometry or hyperparameters"))?;
    if !c.n_embd.is_multiple_of(c.n_heads) || !c.n_heads.is_multiple_of(c.n_kv_heads)
        || c.head_dim == 0 || !c.head_dim.is_multiple_of(2)
    {
        return Err(unsupported(format!(
            "invalid geometry: embedding {}, heads {}, KV heads {}, head_dim {}",
            c.n_embd, c.n_heads, c.n_kv_heads, c.head_dim
        )));
    }
    let prefix = format!("{arch}.");
    for key in gguf.metadata_keys().filter(|k| k.starts_with(&prefix)) {
        let suffix = &key[prefix.len()..];
        match suffix {
            suffix if INTEGER_KEYS.contains(&suffix) || FLOAT_KEYS.contains(&suffix) => {}
            suffix if HEAD_KEYS.contains(&suffix) => {
                if gguf.metadata_u32(key).map(|v| v as usize) != Some(c.head_dim) {
                    return Err(unsupported(format!("{key} must equal the full head width {}", c.head_dim)));
                }
            }
            "attention.causal" => {
                if gguf.metadata_bool(key) != Some(true) {
                    return Err(unsupported(format!("{key} must be true; only causal decoding is supported")));
                }
            }
            "vocab_size" => {}
            _ => return Err(unsupported(format!("unimplemented architecture metadata {key}"))),
        }
    }
    Ok(c)
}

fn layout<B: Backend>(backend: &B, gguf: &GgufModel, c: &ModelConfig, arch: &str) -> Result<()> {
    let mut remaining: HashSet<_> = gguf.tensor_names().collect();
    // Bound descriptor work even if a corrupt header claims billions of layers.
    if c.n_layers > remaining.len() {
        return Err(unsupported("block_count exceeds tensor count"));
    }
    let embd = gguf.tensor_desc("token_embd.weight")
        .ok_or_else(|| unsupported("missing tensor token_embd.weight"))?;
    let vocab = match embd.shape.as_slice() {
        &[vocab, width] if vocab > 0 && width == c.n_embd => vocab,
        _ => return Err(unsupported(format!("token_embd.weight: expected [vocab > 0, {}], got {:?}", c.n_embd, embd.shape))),
    };
    let vocab_key = format!("{arch}.vocab_size");
    if gguf.has_metadata(&vocab_key)
        && gguf.metadata_u32(&vocab_key).map(|v| v as usize) != Some(vocab)
    {
        return Err(unsupported(format!("{vocab_key} must match token_embd.weight")));
    }
    let mut tensor = |name: &str, shape: &[usize], norm: bool| -> Result<()> {
        let desc = gguf.tensor_desc(name).ok_or_else(|| unsupported(format!("missing tensor {name}")))?;
        validate_tensor(backend, name, desc, shape, norm)?;
        remaining.remove(name);
        Ok(())
    };
    tensor("token_embd.weight", &[vocab, c.n_embd], false)?;
    tensor("output_norm.weight", &[c.n_embd], true)?;
    tensor("output.weight", &[vocab, c.n_embd], false)?;
    let kv_dim = c.n_kv_heads.checked_mul(c.head_dim).ok_or_else(|| unsupported("KV width overflows usize"))?;
    for i in 0..c.n_layers {
        for (suffix, shape, norm) in [
            ("attn_norm", vec![c.n_embd], true),
            ("attn_q", vec![c.n_embd, c.n_embd], false),
            ("attn_k", vec![kv_dim, c.n_embd], false),
            ("attn_v", vec![kv_dim, c.n_embd], false),
            ("attn_output", vec![c.n_embd, c.n_embd], false),
            ("ffn_norm", vec![c.n_embd], true),
            ("ffn_gate", vec![c.n_ff, c.n_embd], false),
            ("ffn_up", vec![c.n_ff, c.n_embd], false),
            ("ffn_down", vec![c.n_embd, c.n_ff], false),
        ] {
            tensor(&format!("blk.{i}.{suffix}.weight"), &shape, norm)?;
        }
    }
    if let Some(name) = remaining.into_iter().min() {
        return Err(unsupported(format!("unexpected tensor {name}; biases, extra norms and task heads are not implemented")));
    }
    Ok(())
}
