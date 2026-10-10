use super::*;
use crate::model::preflight::validate_tensor;

impl<B: Backend> Gemma<B> {
    pub(in crate::model) fn validate(b: &B, g: &GgufModel) -> Result<(ModelConfig, Config)> {
        for operation in [Operation::Scale, Operation::GeluTanhMul, Operation::Softcap,
            Operation::RopeSplitHalf, Operation::WindowedAttention] {
            if !b.supports(operation) {
                return Err(Error::NotImplemented("Gemma 4 requires scale, GELU gating, softcap, split-half RoPE, and windowed attention"));
            }
        }
        let (c, config) = Config::read(g)?;
        let vocab = g.tensor_desc("token_embd.weight").and_then(|t| t.shape.first()).copied()
            .filter(|&n| n > 0).ok_or_else(|| invalid("missing Gemma vocabulary"))?;
        let tensor = |name: &str, shape: &[usize], norm: bool| -> Result<()> {
            let desc = g.tensor_desc(name).ok_or_else(|| invalid(format!("missing tensor {name}")))?;
            validate_tensor(b, name, desc, shape, norm)
        };
        tensor("token_embd.weight", &[vocab, c.n_embd], false)?;
        tensor("output_norm.weight", &[c.n_embd], true)?;
        if g.tensor_desc("output.weight").is_some() {
            tensor("output.weight", &[vocab, c.n_embd], false)?;
        }
        let total = product(&[c.n_layers, config.per_layer])?;
        tensor("per_layer_token_embd.weight", &[vocab, total], false)?;
        tensor("per_layer_model_proj.weight", &[total, c.n_embd], false)?;
        tensor("per_layer_proj_norm.weight", &[config.per_layer], true)?;
        tensor("rope_freqs.weight", &[config.global_dim / 2], true)?;
        for i in 0..c.n_layers {
            let name = |s: &str| format!("blk.{i}.{s}.weight");
            let d = config.dim(i);
            if b.max_head_dim().is_some_and(|limit| d > limit) {
                return Err(invalid("Gemma head width exceeds backend limit"));
            }
            let q = product(&[c.n_heads, d])?;
            let kv = product(&[c.n_kv_heads, d])?;
            for suffix in ["attn_norm", "post_attention_norm", "ffn_norm", "post_ffw_norm", "post_norm"] {
                tensor(&name(suffix), &[c.n_embd], true)?;
            }
            tensor(&name("attn_q"), &[q, c.n_embd], false)?;
            tensor(&name("attn_q_norm"), &[d], true)?;
            if i < config.producers {
                tensor(&name("attn_k"), &[kv, c.n_embd], false)?;
                tensor(&name("attn_k_norm"), &[d], true)?;
                if g.tensor_desc(&name("attn_v")).is_some() {
                    tensor(&name("attn_v"), &[kv, c.n_embd], false)?;
                }
            }
            tensor(&name("attn_output"), &[c.n_embd, q], false)?;
            tensor(&name("ffn_gate"), &[config.ffn[i], c.n_embd], false)?;
            tensor(&name("ffn_up"), &[config.ffn[i], c.n_embd], false)?;
            tensor(&name("ffn_down"), &[c.n_embd, config.ffn[i]], false)?;
            tensor(&name("inp_gate"), &[config.per_layer, c.n_embd], false)?;
            tensor(&name("proj"), &[c.n_embd, config.per_layer], false)?;
            if g.tensor_desc(&name("layer_output_scale")).is_some() {
                tensor(&name("layer_output_scale"), &[1], true)?;
            }
        }
        config.workspace_bytes(&c)?;
        config.generated_weight_bytes(&c)?;
        Ok((c, config))
    }
}
