use super::Qwen;
use crate::backend::{Backend, Operation, Result};
use crate::gguf::GgufModel;
use crate::memory::product;
use crate::model::ModelConfig;
use crate::model::preflight::{unsupported, validate_tensor};
use std::collections::HashSet;

const INTS: &[&str] = &[
    "block_count", "embedding_length", "feed_forward_length",
    "attention.head_count", "attention.head_count_kv", "context_length",
];
const FLOATS: &[&str] = &["rope.freq_base", "attention.layer_norm_rms_epsilon"];

pub(in crate::model) fn config(g: &GgufModel, arch: &str) -> Result<ModelConfig> {
    let u = |suffix: &str| {
        let key = format!("{arch}.{suffix}");
        g.metadata_u32(&key).filter(|v| *v > 0).map(|v| v as usize)
            .ok_or_else(|| unsupported(format!("{key} must be a positive integer")))
    };
    let f = |suffix: &str| {
        let key = format!("{arch}.{suffix}");
        g.metadata_f32(&key).filter(|v| v.is_finite() && *v > 0.0)
            .ok_or_else(|| unsupported(format!("{key} must be a finite positive float")))
    };
    let n_embd = u("embedding_length")?;
    let n_heads = u("attention.head_count")?;
    let n_kv_heads = u("attention.head_count_kv")?;
    let key = format!("{arch}.attention.key_length");
    let head_dim = if arch == "qwen3" || g.has_metadata(&key) { u("attention.key_length")? }
        else if n_embd.is_multiple_of(n_heads) { n_embd / n_heads }
        else { return Err(unsupported("Qwen head width must be specified independently of embedding width")); };
    if !n_heads.is_multiple_of(n_kv_heads) || head_dim == 0 || !head_dim.is_multiple_of(2) {
        return Err(unsupported("invalid Qwen attention geometry"));
    }
    for suffix in ["attention.value_length", "rope.dimension_count"] {
        let key = format!("{arch}.{suffix}");
        if g.has_metadata(&key) && g.metadata_u32(&key).map(|v| v as usize) != Some(head_dim) {
            return Err(unsupported(format!("{key} must equal head width {head_dim}")));
        }
    }
    let c = ModelConfig {
        n_layers: u("block_count")?, n_embd, n_ff: u("feed_forward_length")?,
        n_heads, n_kv_heads, head_dim, rope_theta: f("rope.freq_base")?,
        rms_eps: f("attention.layer_norm_rms_epsilon")?, n_ctx: u("context_length")?,
    };
    product(&[n_heads, head_dim])?;
    product(&[n_kv_heads, head_dim])?;
    let prefix = format!("{arch}.");
    for key in g.metadata_keys().filter(|k| k.starts_with(&prefix)) {
        match &key[prefix.len()..] {
            suffix if INTS.contains(&suffix) || FLOATS.contains(&suffix) => {}
            "attention.key_length" | "attention.value_length" | "rope.dimension_count" | "vocab_size" => {}
            "attention.causal" if g.metadata_bool(key) == Some(true) => {}
            "rope.dimension_sections" if arch == "qwen2vl" => {
                let sections = g.metadata_u32_array(key).ok_or_else(|| unsupported(format!("invalid {key}")))?;
                if !(3..=4).contains(&sections.len())
                    || sections.iter().map(|v| u64::from(*v)).sum::<u64>() != (head_dim / 2) as u64 {
                    return Err(unsupported(format!("invalid {key}")));
                }
            }
            _ => return Err(unsupported(format!("unimplemented architecture metadata {key}"))),
        }
    }
    Ok(c)
}

impl<B: Backend> Qwen<B> {
    pub(in crate::model) fn validate(b: &B, g: &GgufModel, arch: &str) -> Result<ModelConfig> {
        let c = config(g, arch)?;
        if let Some(limit) = b.max_head_dim() && c.head_dim > limit {
            return Err(unsupported(format!("head_dim {} exceeds backend limit {limit}", c.head_dim)));
        }
        let mut required = vec![Operation::RopeSplitHalf];
        if arch != "qwen3" { required.push(Operation::AddBias); }
        for op in required {
            if !b.supports(op) { return Err(unsupported(format!("Qwen requires backend operation {op:?}"))); }
        }
        layout(b, g, &c, arch)?;
        Ok(c)
    }
}

fn layout<B: Backend>(b: &B, g: &GgufModel, c: &ModelConfig, arch: &str) -> Result<()> {
    let mut remaining: HashSet<_> = g.tensor_names().collect();
    if c.n_layers > remaining.len() { return Err(unsupported("block_count exceeds tensor count")); }
    let embd = g.tensor_desc("token_embd.weight").ok_or_else(|| unsupported("missing tensor token_embd.weight"))?;
    let vocab = match embd.shape.as_slice() {
        &[vocab, width] if vocab > 0 && width == c.n_embd => vocab,
        _ => return Err(unsupported("invalid Qwen embedding shape")),
    };
    let key = format!("{arch}.vocab_size");
    if g.has_metadata(&key) && g.metadata_u32(&key).map(|v| v as usize) != Some(vocab) {
        return Err(unsupported(format!("{key} must match token_embd.weight")));
    }
    let mut tensor = |name: &str, shape: &[usize], norm: bool| -> Result<()> {
        let desc = g.tensor_desc(name).ok_or_else(|| unsupported(format!("missing tensor {name}")))?;
        validate_tensor(b, name, desc, shape, norm)?;
        remaining.remove(name);
        Ok(())
    };
    tensor("token_embd.weight", &[vocab, c.n_embd], false)?;
    tensor("output_norm.weight", &[c.n_embd], true)?;
    if g.tensor_desc("output.weight").is_some() { tensor("output.weight", &[vocab, c.n_embd], false)?; }
    let q = product(&[c.n_heads, c.head_dim])?;
    let kv = product(&[c.n_kv_heads, c.head_dim])?;
    for i in 0..c.n_layers {
        for (suffix, shape, norm) in [
            ("attn_norm", vec![c.n_embd], true),
            ("attn_q", vec![q, c.n_embd], false),
            ("attn_k", vec![kv, c.n_embd], false),
            ("attn_v", vec![kv, c.n_embd], false),
            ("attn_output", vec![c.n_embd, q], false),
            ("ffn_norm", vec![c.n_embd], true),
            ("ffn_gate", vec![c.n_ff, c.n_embd], false),
            ("ffn_up", vec![c.n_ff, c.n_embd], false),
            ("ffn_down", vec![c.n_embd, c.n_ff], false),
        ] { tensor(&format!("blk.{i}.{suffix}.weight"), &shape, norm)?; }
        if arch == "qwen3" {
            for suffix in ["attn_q_norm", "attn_k_norm"] {
                tensor(&format!("blk.{i}.{suffix}.weight"), &[c.head_dim], true)?;
            }
        } else {
            for (suffix, width) in [("attn_q", q), ("attn_k", kv), ("attn_v", kv)] {
                tensor(&format!("blk.{i}.{suffix}.bias"), &[width], true)?;
            }
        }
    }
    if let Some(name) = remaining.into_iter().min() {
        return Err(unsupported(format!("unexpected tensor {name}; Qwen task heads and adapters are not text decoders")));
    }
    Ok(())
}
