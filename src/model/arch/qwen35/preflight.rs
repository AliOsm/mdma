use super::{ARCH, Config, Qwen35};
use crate::backend::{Backend, Operation, Result};
use crate::gguf::GgufModel;
use crate::memory::product;
use crate::model::ModelConfig;
use crate::model::planning::layer_index;
use crate::model::preflight::{Layout, unsupported};

impl<B: Backend> Qwen35<B> {
    pub(in crate::model) fn validate(b: &B, g: &GgufModel) -> Result<(ModelConfig, Config)> {
        let (c, config) = Config::read(g)?;
        if let Some(limit) = b.max_head_dim()
            && c.head_dim.max(config.delta.head_dim) > limit
        {
            return Err(unsupported(format!(
                "head width exceeds backend limit {limit}"
            )));
        }
        for op in [
            Operation::RopePartialSplitHalf,
            Operation::SigmoidMul,
            Operation::GatedDeltaNet,
        ] {
            if !b.supports(op) {
                return Err(unsupported(format!(
                    "Qwen3.5 requires backend operation {op:?}"
                )));
            }
        }
        layout(b, g, &c, &config)?;
        Ok((c, config))
    }
}

fn layout<B: Backend>(b: &B, g: &GgufModel, c: &ModelConfig, config: &Config) -> Result<()> {
    let (mut tensors, _) = Layout::new(b, g, ARCH, c, true)?;
    let q = product(&[c.n_heads, c.head_dim])?;
    let kv = product(&[c.n_kv_heads, c.head_dim])?;
    let (conv, value) = (config.conv_channels(), config.value_width());
    let heads = config.delta.n_v_heads;
    for (i, &recurrent) in config.recurrent.iter().enumerate() {
        let mut shapes = vec![
            ("attn_norm.weight", vec![c.n_embd], true),
            ("post_attention_norm.weight", vec![c.n_embd], true),
            ("ffn_gate.weight", vec![c.n_ff, c.n_embd], false),
            ("ffn_up.weight", vec![c.n_ff, c.n_embd], false),
            ("ffn_down.weight", vec![c.n_embd, c.n_ff], false),
        ];
        if recurrent {
            shapes.extend([
                ("attn_qkv.weight", vec![conv, c.n_embd], false),
                ("attn_gate.weight", vec![value, c.n_embd], false),
                ("ssm_alpha.weight", vec![heads, c.n_embd], false),
                ("ssm_beta.weight", vec![heads, c.n_embd], false),
                ("ssm_conv1d.weight", vec![conv, config.conv_taps], true),
                ("ssm_dt.bias", vec![heads], true),
                ("ssm_a", vec![heads], true),
                ("ssm_norm.weight", vec![config.delta.head_dim], true),
                ("ssm_out.weight", vec![c.n_embd, value], false),
            ]);
        } else {
            shapes.extend([
                ("attn_q.weight", vec![2 * q, c.n_embd], false),
                ("attn_k.weight", vec![kv, c.n_embd], false),
                ("attn_v.weight", vec![kv, c.n_embd], false),
                ("attn_output.weight", vec![c.n_embd, q], false),
                ("attn_q_norm.weight", vec![c.head_dim], true),
                ("attn_k_norm.weight", vec![c.head_dim], true),
            ]);
        }
        for (suffix, shape, norm) in shapes {
            tensors.tensor(&format!("blk.{i}.{suffix}"), &shape, norm)?;
        }
    }
    // Multi-token prediction blocks follow the trunk; they are never run.
    tensors.skip(|name| {
        layer_index(name).is_some_and(|layer| (c.n_layers..config.block_count).contains(&layer))
    });
    tensors.finish("Qwen3.5 task heads and adapters are not text decoders")
}
