use super::Glm4;
use crate::backend::{Backend, Operation, Result};
use crate::gguf::GgufModel;
use crate::memory::product;
use crate::model::ModelConfig;
use crate::model::preflight::{Layout, positive, positive_f32, unsupported};

pub(super) fn rotary_dim(g: &GgufModel, c: &ModelConfig) -> Result<usize> {
    let rot = g
        .metadata_u32("glm4.rope.dimension_count")
        .map(|v| v as usize)
        .ok_or_else(|| unsupported("glm4.rope.dimension_count must be an integer"))?;
    if rot == 0 || !rot.is_multiple_of(2) || rot > c.head_dim {
        return Err(unsupported(
            "glm4.rope.dimension_count must be a positive even rotary prefix within head width",
        ));
    }
    Ok(rot)
}

impl<B: Backend> Glm4<B> {
    pub(in crate::model) fn validate(b: &B, g: &GgufModel) -> Result<ModelConfig> {
        let u = |suffix: &str| positive(g, "glm4", suffix);
        let f = |suffix: &str| positive_f32(g, "glm4", suffix);
        let n_embd = u("embedding_length")?;
        let n_heads = u("attention.head_count")?;
        let head_dim = if g.has_metadata("glm4.attention.key_length") {
            u("attention.key_length")?
        } else if n_embd.is_multiple_of(n_heads) {
            n_embd / n_heads
        } else {
            return Err(unsupported("GLM head width must be specified"));
        };
        let c = ModelConfig {
            n_layers: u("block_count")?,
            n_embd,
            n_ff: u("feed_forward_length")?,
            n_heads,
            n_kv_heads: u("attention.head_count_kv")?,
            head_dim,
            rope_theta: f("rope.freq_base")?,
            rms_eps: f("attention.layer_norm_rms_epsilon")?,
            n_ctx: u("context_length")?,
        };
        if head_dim == 0 || !head_dim.is_multiple_of(2) || !n_heads.is_multiple_of(c.n_kv_heads) {
            return Err(unsupported("invalid GLM attention geometry"));
        }
        if g.has_metadata("glm4.attention.value_length")
            && g.metadata_u32("glm4.attention.value_length")
                .map(|v| v as usize)
                != Some(head_dim)
        {
            return Err(unsupported(
                "glm4.attention.value_length must equal head width",
            ));
        }
        rotary_dim(g, &c)?;
        for key in g.metadata_keys().filter(|k| k.starts_with("glm4.")) {
            match &key["glm4.".len()..] {
                "block_count"
                | "embedding_length"
                | "feed_forward_length"
                | "context_length"
                | "attention.head_count"
                | "attention.head_count_kv"
                | "attention.key_length"
                | "attention.value_length"
                | "rope.dimension_count"
                | "rope.freq_base"
                | "attention.layer_norm_rms_epsilon"
                | "vocab_size" => {}
                "attention.causal" if g.metadata_bool(key) == Some(true) => {}
                _ => {
                    return Err(unsupported(format!(
                        "unimplemented architecture metadata {key}"
                    )));
                }
            }
        }
        if !b.supports(Operation::RopePartialInterleaved) {
            return Err(unsupported(
                "GLM requires backend operation RopePartialInterleaved",
            ));
        }
        if let Some(limit) = b.max_head_dim()
            && head_dim > limit
        {
            return Err(unsupported(format!(
                "GLM head_dim {head_dim} exceeds backend limit {limit}"
            )));
        }
        layout(b, g, &c)?;
        Ok(c)
    }
}

fn layout<B: Backend>(b: &B, g: &GgufModel, c: &ModelConfig) -> Result<()> {
    let (mut tensors, _) = Layout::new(b, g, "glm4", c, true)?;
    let q = product(&[c.n_heads, c.head_dim])?;
    let kv = product(&[c.n_kv_heads, c.head_dim])?;
    let packed = product(&[2, c.n_ff])?;
    for i in 0..c.n_layers {
        for (suffix, shape, norm) in [
            ("attn_norm", vec![c.n_embd], true),
            ("attn_q", vec![q, c.n_embd], false),
            ("attn_k", vec![kv, c.n_embd], false),
            ("attn_v", vec![kv, c.n_embd], false),
            ("attn_output", vec![c.n_embd, q], false),
            ("post_attention_norm", vec![c.n_embd], true),
            ("ffn_norm", vec![c.n_embd], true),
            ("ffn_up", vec![packed, c.n_embd], false),
            ("ffn_down", vec![c.n_embd, c.n_ff], false),
            ("post_ffw_norm", vec![c.n_embd], true),
        ] {
            tensors.tensor(&format!("blk.{i}.{suffix}.weight"), &shape, norm)?;
        }
    }
    tensors.finish("GLM MTP, MoE, adapters and task heads are not implemented")
}
