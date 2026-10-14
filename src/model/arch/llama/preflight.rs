//! Metadata-only admission for separate-Q/K/V Llama and InternLM2 layouts.
use super::Llama;
use crate::backend::{Backend, Result};
use crate::gguf::GgufModel;
use crate::model::ModelConfig;
use crate::model::preflight::{Layout, unsupported};

#[cfg(test)]
mod tests;

const INTEGER_KEYS: &[&str] = &[
    "block_count",
    "embedding_length",
    "feed_forward_length",
    "attention.head_count",
    "context_length",
];
const FLOAT_KEYS: &[&str] = &["attention.layer_norm_rms_epsilon"];
/// Absent keys take llama.cpp's defaults (KV heads = heads, theta 10000).
const OPTIONAL_INTEGER_KEYS: &[&str] = &["attention.head_count_kv"];
const OPTIONAL_FLOAT_KEYS: &[&str] = &["rope.freq_base"];
const HEAD_KEYS: &[&str] = &[
    "attention.key_length",
    "attention.value_length",
    "rope.dimension_count",
];

impl<B: Backend> Llama<B> {
    pub(in crate::model) fn validate(
        backend: &B,
        gguf: &GgufModel,
        arch: &str,
    ) -> Result<ModelConfig> {
        let config = config(gguf, arch)?;
        if let Some(limit) = backend.max_head_dim()
            && config.head_dim > limit
        {
            return Err(unsupported(format!(
                "head_dim {} exceeds backend limit {limit}",
                config.head_dim
            )));
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
    for suffix in OPTIONAL_INTEGER_KEYS {
        let key = format!("{arch}.{suffix}");
        if gguf.has_metadata(&key) && gguf.metadata_u32(&key).is_none_or(|v| v == 0) {
            return Err(unsupported(format!("{key} must be a positive integer")));
        }
    }
    for suffix in FLOAT_KEYS.iter().chain(OPTIONAL_FLOAT_KEYS) {
        let key = format!("{arch}.{suffix}");
        let required = FLOAT_KEYS.contains(suffix);
        if (required || gguf.has_metadata(&key))
            && !gguf
                .metadata_f32(&key)
                .is_some_and(|v| v.is_finite() && v > 0.0)
        {
            return Err(unsupported(format!(
                "{key} must be a finite positive float"
            )));
        }
    }
    let c = ModelConfig::from_gguf(gguf, arch)
        .ok_or_else(|| unsupported("invalid geometry or hyperparameters"))?;
    if !c.n_embd.is_multiple_of(c.n_heads)
        || !c.n_heads.is_multiple_of(c.n_kv_heads)
        || c.head_dim == 0
        || !c.head_dim.is_multiple_of(2)
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
            suffix
                if INTEGER_KEYS.contains(&suffix)
                    || FLOAT_KEYS.contains(&suffix)
                    || OPTIONAL_INTEGER_KEYS.contains(&suffix)
                    || OPTIONAL_FLOAT_KEYS.contains(&suffix) => {}
            suffix if HEAD_KEYS.contains(&suffix) => {
                if gguf.metadata_u32(key).map(|v| v as usize) != Some(c.head_dim) {
                    return Err(unsupported(format!(
                        "{key} must equal the full head width {}",
                        c.head_dim
                    )));
                }
            }
            "attention.causal" => {
                if gguf.metadata_bool(key) != Some(true) {
                    return Err(unsupported(format!(
                        "{key} must be true; only causal decoding is supported"
                    )));
                }
            }
            "vocab_size" => {}
            _ => {
                return Err(unsupported(format!(
                    "unimplemented architecture metadata {key}"
                )));
            }
        }
    }
    Ok(c)
}

fn layout<B: Backend>(backend: &B, gguf: &GgufModel, c: &ModelConfig, arch: &str) -> Result<()> {
    let (mut tensors, _) = Layout::new(backend, gguf, arch, c, false)?;
    let kv_dim = c
        .n_kv_heads
        .checked_mul(c.head_dim)
        .ok_or_else(|| unsupported("KV width overflows usize"))?;
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
            tensors.tensor(&format!("blk.{i}.{suffix}.weight"), &shape, norm)?;
        }
    }
    tensors.finish("biases, extra norms and task heads are not implemented")
}
