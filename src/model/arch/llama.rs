//! Llama GGUF layout, grouped-query attention, and SwiGLU decoder layers.
use super::Decoder;
use crate::backend::{AttnSpec, Backend, QWeight, Result, RopeSpec};
use crate::gguf::GgufModel;
use crate::kv_cache::KvCache;
use crate::model::{Model, ModelConfig, Workspace, load_norm, load_weight};

mod preflight;
#[cfg(test)]
pub(in crate::model) mod tests;

pub(in crate::model) const ARCH: &str = "internlm2";
/// llama.cpp's `rope.freq_base` when the key is absent.
pub(in crate::model) const DEFAULT_ROPE_THETA: f32 = 10_000.0;

pub(in crate::model) fn config_from_gguf(gguf: &GgufModel, arch: &str) -> Option<ModelConfig> {
    let n_embd = gguf.metadata_u32(&format!("{arch}.embedding_length"))? as usize;
    let n_heads = gguf.metadata_u32(&format!("{arch}.attention.head_count"))? as usize;
    if n_heads == 0 || n_embd == 0 || !n_embd.is_multiple_of(n_heads) {
        return None;
    }
    Some(ModelConfig {
        n_layers: gguf.metadata_u32(&format!("{arch}.block_count"))? as usize,
        n_embd,
        n_ff: gguf.metadata_u32(&format!("{arch}.feed_forward_length"))? as usize,
        n_heads,
        // llama.cpp's defaults: llama2.c conversions omit both keys.
        n_kv_heads: gguf
            .metadata_u32(&format!("{arch}.attention.head_count_kv"))
            .map_or(n_heads, |n| n as usize),
        head_dim: n_embd.checked_div(n_heads)?,
        rope_theta: gguf
            .metadata_f32(&format!("{arch}.rope.freq_base"))
            .unwrap_or(DEFAULT_ROPE_THETA),
        rms_eps: gguf.metadata_f32(&format!("{arch}.attention.layer_norm_rms_epsilon"))?,
        n_ctx: gguf.metadata_u32(&format!("{arch}.context_length"))? as usize,
    })
}

pub(in crate::model) struct Layer<B: Backend> {
    attn_norm: B::Buf,
    wq: QWeight<B::Buf>,
    wk: QWeight<B::Buf>,
    wv: QWeight<B::Buf>,
    wo: QWeight<B::Buf>,
    ffn_norm: B::Buf,
    w_gate: QWeight<B::Buf>,
    w_up: QWeight<B::Buf>,
    w_down: QWeight<B::Buf>,
}

/// One layer's matmul weights, for backends that place them before decoding.
#[cfg_attr(
    not(all(feature = "coral", any(target_os = "freebsd", target_os = "linux"))),
    expect(dead_code, reason = "read by the Coral backend")
)]
pub(crate) struct Matmuls<'a, Buf> {
    pub wq: &'a QWeight<Buf>,
    pub wk: &'a QWeight<Buf>,
    pub wv: &'a QWeight<Buf>,
    pub wo: &'a QWeight<Buf>,
    pub w_gate: &'a QWeight<Buf>,
    pub w_up: &'a QWeight<Buf>,
    pub w_down: &'a QWeight<Buf>,
}

impl<B: Backend> Layer<B> {
    #[cfg_attr(
        not(all(feature = "coral", any(target_os = "freebsd", target_os = "linux"))),
        expect(dead_code, reason = "read by the Coral backend")
    )]
    /// (attention norm, FFN norm) weights.
    pub(in crate::model) fn norms(&self) -> (&B::Buf, &B::Buf) {
        (&self.attn_norm, &self.ffn_norm)
    }

    pub(in crate::model) fn matmuls(&self) -> Matmuls<'_, B::Buf> {
        Matmuls {
            wq: &self.wq,
            wk: &self.wk,
            wv: &self.wv,
            wo: &self.wo,
            w_gate: &self.w_gate,
            w_up: &self.w_up,
            w_down: &self.w_down,
        }
    }
}

pub(in crate::model) struct Llama<B: Backend> {
    pub(in crate::model) layers: Vec<Layer<B>>,
}

impl<B: Backend> Llama<B> {
    /// Called only after the shared loader completes descriptor preflight.
    pub fn load(backend: &B, gguf: &mut GgufModel, config: ModelConfig) -> Result<Model<B>> {
        let token_embd = load_weight(backend, gguf, "token_embd.weight")?;
        let layers = Self::load_layers(backend, gguf, 0..config.n_layers)?;
        let output_norm = load_norm(backend, gguf, "output_norm.weight")?;
        let output = load_weight(backend, gguf, "output.weight")?;
        Ok(Model {
            config,
            token_embd,
            output_norm,
            output: Some(output),
            architecture: Decoder::Llama(Self { layers }),
            bos: None,
            memory: None,
        })
    }

    pub(in crate::model) fn load_layers(
        backend: &B,
        gguf: &mut GgufModel,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<Layer<B>>> {
        let mut layers = Vec::with_capacity(range.len());
        for i in range {
            layers.push(Layer {
                attn_norm: load_norm(backend, gguf, &format!("blk.{i}.attn_norm.weight"))?,
                wq: load_weight(backend, gguf, &format!("blk.{i}.attn_q.weight"))?,
                wk: load_weight(backend, gguf, &format!("blk.{i}.attn_k.weight"))?,
                wv: load_weight(backend, gguf, &format!("blk.{i}.attn_v.weight"))?,
                wo: load_weight(backend, gguf, &format!("blk.{i}.attn_output.weight"))?,
                ffn_norm: load_norm(backend, gguf, &format!("blk.{i}.ffn_norm.weight"))?,
                w_gate: load_weight(backend, gguf, &format!("blk.{i}.ffn_gate.weight"))?,
                w_up: load_weight(backend, gguf, &format!("blk.{i}.ffn_up.weight"))?,
                w_down: load_weight(backend, gguf, &format!("blk.{i}.ffn_down.weight"))?,
            });
        }
        Ok(layers)
    }

    /// Leaves final-normed hidden states in `ws.normed`. Shared by decode
    /// (T=1) and batched prefill.
    pub fn run(
        &self,
        backend: &B,
        model: &Model<B>,
        kv: &mut KvCache<B>,
        ws: &mut Workspace<B>,
        tokens: &[u32],
        pos: u32,
    ) -> Result<()> {
        backend.embed(&model.token_embd, tokens, &mut ws.x)?;
        self.run_layers(
            backend,
            &model.config,
            kv,
            ws,
            pos,
            tokens.len(),
            Some(&model.output_norm),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::model) fn run_layers(
        &self,
        backend: &B,
        c: &ModelConfig,
        kv: &mut KvCache<B>,
        ws: &mut Workspace<B>,
        pos: u32,
        n_tokens: usize,
        final_norm: Option<&B::Buf>,
    ) -> Result<()> {
        let rope_spec = RopeSpec {
            head_dim: c.head_dim,
            theta: c.rope_theta,
        };
        let attn_spec = AttnSpec {
            head_dim: c.head_dim,
            n_q_heads: c.n_heads,
            n_kv_heads: c.n_kv_heads,
        };

        // Fuse each residual add with the following norm: the preceding
        // FFN at the next layer, attention before FFN, and the last FFN
        // before the output norm.
        for (i, layer) in self.layers.iter().enumerate() {
            if i == 0 {
                backend.rmsnorm(&layer.attn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            } else {
                backend.add_rmsnorm(
                    &layer.attn_norm,
                    &ws.x,
                    &ws.proj,
                    &mut ws.x_alt,
                    &mut ws.normed,
                    c.rms_eps,
                )?;
                std::mem::swap(&mut ws.x, &mut ws.x_alt);
            }
            backend.qmatmul_qkv(
                &layer.wq, &layer.wk, &layer.wv, &ws.normed, &mut ws.q, &mut ws.k, &mut ws.v,
            )?;
            // Cache position-encoded K and unrotated V. The new rows must
            // be visible to this layer's attention, including themselves.
            kv.rope_write(
                backend,
                i,
                pos as usize,
                &mut ws.q,
                &mut ws.k,
                &ws.v,
                n_tokens,
                &rope_spec,
            )?;
            kv.set_len(pos as usize + n_tokens);
            backend.attn(&ws.q, &kv.view(i), &attn_spec, &mut ws.attn_out)?;
            backend.qmatmul(&layer.wo, &ws.attn_out, &mut ws.proj)?;
            backend.add_rmsnorm(
                &layer.ffn_norm,
                &ws.x,
                &ws.proj,
                &mut ws.x_alt,
                &mut ws.normed,
                c.rms_eps,
            )?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
            backend.ffn_gate_up(
                &layer.w_gate,
                &layer.w_up,
                &ws.normed,
                &mut ws.gate,
                &mut ws.up,
                &mut ws.act,
            )?;
            backend.qmatmul(&layer.w_down, &ws.act, &mut ws.proj)?;
        }
        if let Some(norm) = final_norm {
            backend.add_rmsnorm(
                norm,
                &ws.x,
                &ws.proj,
                &mut ws.x_alt,
                &mut ws.normed,
                c.rms_eps,
            )?;
        } else {
            backend.add(&ws.x, &ws.proj, &mut ws.x_alt)?;
        }
        std::mem::swap(&mut ws.x, &mut ws.x_alt);
        Ok(())
    }
}
