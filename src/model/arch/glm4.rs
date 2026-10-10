//! Dense GLM-4 text decoder: partial interleaved RoPE and post-sublayer norms.
use super::Decoder;
use crate::backend::{AttnSpec, Backend, Pool, QWeight, Result, RopeSpec};
use crate::gguf::GgufModel;
use crate::kv_cache::KvCache;
use crate::memory::product;
use crate::model::{Model, ModelConfig, load_norm, load_weight};

mod preflight;
#[cfg(test)]
mod tests;

struct Layer<B: Backend> {
    attn_norm: B::Buf,
    q: QWeight<B::Buf>,
    k: QWeight<B::Buf>,
    v: QWeight<B::Buf>,
    out: QWeight<B::Buf>,
    attn_post_norm: B::Buf,
    ffn_norm: B::Buf,
    up: QWeight<B::Buf>,
    down: QWeight<B::Buf>,
    ffn_post_norm: B::Buf,
}

pub(in crate::model) struct Glm4<B: Backend> {
    layers: Vec<Layer<B>>,
    rotary_dim: usize,
}

pub(in crate::model) struct Workspace<B: Backend> {
    packed_ffn: B::Buf,
}

impl<B: Backend> Glm4<B> {
    pub fn load(b: &B, g: &mut GgufModel, c: ModelConfig) -> Result<Model<B>> {
        let rotary_dim = preflight::rotary_dim(g, &c)?;
        let token_embd = load_weight(b, g, "token_embd.weight")?;
        let output_norm = load_norm(b, g, "output_norm.weight")?;
        let output = if g.tensor_desc("output.weight").is_some() {
            Some(load_weight(b, g, "output.weight")?)
        } else { None };
        let mut layers = Vec::with_capacity(c.n_layers);
        for i in 0..c.n_layers {
            let name = |s: &str| format!("blk.{i}.{s}.weight");
            layers.push(Layer {
                attn_norm: load_norm(b, g, &name("attn_norm"))?,
                q: load_weight(b, g, &name("attn_q"))?,
                k: load_weight(b, g, &name("attn_k"))?,
                v: load_weight(b, g, &name("attn_v"))?,
                out: load_weight(b, g, &name("attn_output"))?,
                attn_post_norm: load_norm(b, g, &name("post_attention_norm"))?,
                ffn_norm: load_norm(b, g, &name("ffn_norm"))?,
                up: load_weight(b, g, &name("ffn_up"))?,
                down: load_weight(b, g, &name("ffn_down"))?,
                ffn_post_norm: load_norm(b, g, &name("post_ffw_norm"))?,
            });
        }
        Ok(Model {
            config: c, token_embd, output_norm, output,
            architecture: Decoder::Glm4(Self { layers, rotary_dim }), memory: None,
        })
    }

    pub fn workspace_bytes(c: &ModelConfig) -> Result<usize> {
        product(&[2, c.n_ff, size_of::<f32>()])
    }

    #[expect(clippy::unused_self, reason = "architectures share one workspace signature")]
    pub fn workspace(&self, b: &B, c: &ModelConfig) -> Result<Workspace<B>> {
        Ok(Workspace { packed_ffn: b.alloc(Self::workspace_bytes(c)?, Pool::Scratch)? })
    }

    pub fn run(&self, b: &B, model: &Model<B>, kv: &mut KvCache<B>,
        ws: &mut crate::model::Workspace<B>, token: u32, pos: u32) -> Result<()> {
        let c = &model.config;
        let extra = ws.glm.as_mut().expect("GLM workspace was allocated");
        let rope = RopeSpec { head_dim: c.head_dim, theta: c.rope_theta };
        let attn = AttnSpec { head_dim: c.head_dim, n_q_heads: c.n_heads, n_kv_heads: c.n_kv_heads };
        b.embed(&model.token_embd, &[token], &mut ws.x)?;
        for (i, l) in self.layers.iter().enumerate() {
            b.rmsnorm(&l.attn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            b.qmatmul_qkv(&l.q, &l.k, &l.v, &ws.normed, &mut ws.q, &mut ws.k, &mut ws.v)?;
            b.rope_partial_interleaved(&mut ws.q, pos, &rope, self.rotary_dim)?;
            b.rope_partial_interleaved(&mut ws.k, pos, &rope, self.rotary_dim)?;
            kv.write(b, i, pos as usize, &ws.k, &ws.v, 1)?;
            kv.set_len(pos as usize + 1);
            b.attn(&ws.q, &kv.view(i), &attn, &mut ws.attn_out)?;
            b.qmatmul(&l.out, &ws.attn_out, &mut ws.proj)?;
            b.rmsnorm(&l.attn_post_norm, &ws.proj, &mut ws.normed, c.rms_eps)?;
            b.add(&ws.x, &ws.normed, &mut ws.proj)?;
            std::mem::swap(&mut ws.x, &mut ws.proj);
            b.rmsnorm(&l.ffn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            b.qmatmul(&l.up, &ws.normed, &mut extra.packed_ffn)?;
            let bytes = product(&[c.n_ff, size_of::<f32>()])?;
            b.copy(&extra.packed_ffn, 0, &mut ws.gate, 0, bytes)?;
            b.copy(&extra.packed_ffn, bytes, &mut ws.up, 0, bytes)?;
            b.silu_mul(&ws.gate, &ws.up, &mut ws.act)?;
            b.qmatmul(&l.down, &ws.act, &mut ws.proj)?;
            b.rmsnorm(&l.ffn_post_norm, &ws.proj, &mut ws.normed, c.rms_eps)?;
            b.add(&ws.x, &ws.normed, &mut ws.x_alt)?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
        }
        b.rmsnorm(&model.output_norm, &ws.x, &mut ws.normed, c.rms_eps)
    }
}
