//! ModernBERT encoder: alternating local/global attention and GeGLU.
//! Inputs are right-padded batches; outputs stay on the selected backend.
mod config;
pub use config::{EncoderConfig, Error as ConfigError, LayerKind, RopeParams};

#[cfg(test)]
pub(crate) mod tests;

use crate::backend::{Backend, EncoderSpec, Error, Pool, QWeight, Result};
use crate::model::layers::{AttentionWorkspace, Linear, Norm};
use crate::model::weights::{self, Weights};

struct Layer<B: Backend> {
    attn_norm: Option<Norm<B>>,
    wq: Linear<B>,
    wk: Linear<B>,
    wv: Linear<B>,
    wo: Linear<B>,
    mlp_norm: Norm<B>,
    w_in: Linear<B>,
    w_gate: Linear<B>,
    w_out: Linear<B>,
    kind: LayerKind,
}

pub struct ModernBert<B: Backend> {
    config: EncoderConfig,
    tok_embd: QWeight<B::Buf>,
    emb_norm: Norm<B>,
    layers: Vec<Layer<B>>,
    final_norm: Norm<B>,
}

/// Scratch for one right-padded batch shape `(n, seq_len)`.
pub struct Workspace<B: Backend> {
    n: usize,
    seq_len: usize,
    pub(crate) hidden: AttentionWorkspace<B>,
    ff_a: B::Buf,
    ff_b: B::Buf,
    ff_act: B::Buf,
}

impl<B: Backend> Workspace<B> {
    /// Final encoder states, `[n, seq_len, hidden_size]`, after `forward`.
    pub fn hidden_states(&self) -> &B::Buf {
        &self.hidden.x
    }
}

impl<B: Backend> ModernBert<B> {
    /// Load encoder tensors under `prefix` (`"encoder"` for Laya, `""`
    /// for unprefixed encoder weights). No decision-head tensors are read.
    pub fn load(
        backend: &B,
        w: &Weights,
        config: EncoderConfig,
        prefix: &str,
    ) -> weights::Result<Self> {
        config
            .validate()
            .map_err(|e| weights::Error::Checkpoint(e.to_string()))?;
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}.")
        };
        let d = config.hidden_size;
        let inter = config.intermediate_size;
        let mut layers = Vec::with_capacity(config.num_hidden_layers);
        for i in 0..config.num_hidden_layers {
            let p = format!("{prefix}layers.{i}");
            let qkv = |j| {
                Linear::part(
                    backend,
                    w,
                    &format!("{p}.attn.Wqkv.weight"),
                    &format!("{p}.attn.Wqkv.bias"),
                    config.attention_bias,
                    j,
                    d,
                )
            };
            let mlp = |j| {
                Linear::part(
                    backend,
                    w,
                    &format!("{p}.mlp.Wi.weight"),
                    &format!("{p}.mlp.Wi.bias"),
                    config.mlp_bias,
                    j,
                    inter,
                )
            };
            layers.push(Layer {
                attn_norm: if i == 0 {
                    None
                } else {
                    Some(Norm::load(
                        backend,
                        w,
                        &format!("{p}.attn_norm"),
                        config.norm_bias,
                    )?)
                },
                wq: qkv(0)?,
                wk: qkv(1)?,
                wv: qkv(2)?,
                wo: Linear::load(backend, w, &format!("{p}.attn.Wo"), config.attention_bias)?,
                mlp_norm: Norm::load(backend, w, &format!("{p}.mlp_norm"), config.norm_bias)?,
                w_in: mlp(0)?,
                w_gate: mlp(1)?,
                w_out: Linear::load(backend, w, &format!("{p}.mlp.Wo"), config.mlp_bias)?,
                kind: config.layer_kind(i),
            });
        }
        Ok(Self {
            tok_embd: w.matrix(
                backend,
                &format!("{prefix}embeddings.tok_embeddings.weight"),
                None,
            )?,
            emb_norm: Norm::load(
                backend,
                w,
                &format!("{prefix}embeddings.norm"),
                config.norm_bias,
            )?,
            layers,
            final_norm: Norm::load(backend, w, &format!("{prefix}final_norm"), config.norm_bias)?,
            config,
        })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    pub fn alloc_workspace(&self, backend: &B, n: usize, seq_len: usize) -> Result<Workspace<B>> {
        if n == 0 || seq_len == 0 || seq_len > self.config.max_position_embeddings {
            return Err(Error::ShapeMismatch(
                "ModernBERT workspace requires a nonempty batch within context".into(),
            ));
        }
        let t = n * seq_len;
        let ff = || backend.alloc(t * self.config.intermediate_size * 4, Pool::Scratch);
        Ok(Workspace {
            n,
            seq_len,
            hidden: AttentionWorkspace::alloc(backend, t, self.config.hidden_size)?,
            ff_a: ff()?,
            ff_b: ff()?,
            ff_act: ff()?,
        })
    }

    /// Encode flattened token IDs and valid lengths, leaving hidden states
    /// in `ws`. No token types, marker scoring, or host downloads.
    pub fn forward(
        &self,
        backend: &B,
        ws: &mut Workspace<B>,
        ids: &[u32],
        lens: &[u32],
    ) -> Result<()> {
        if ids.len() != ws.n * ws.seq_len
            || lens.len() != ws.n
            || lens.iter().any(|&n| n == 0 || n as usize > ws.seq_len)
            || ids.iter().any(|&id| id as usize >= self.config.vocab_size)
        {
            return Err(Error::ShapeMismatch(
                "ModernBERT requires matching token/length shapes and valid tokens".into(),
            ));
        }
        let c = &self.config;
        let h = &mut ws.hidden;
        backend.embed(&self.tok_embd, ids, &mut h.x_alt)?;
        self.emb_norm
            .forward(backend, &h.x_alt, &mut h.x, c.norm_eps)?;
        for layer in &self.layers {
            let window = match layer.kind {
                LayerKind::Full => None,
                LayerKind::Sliding => Some(c.local_attention / 2),
            };
            let spec = EncoderSpec {
                seq_len: ws.seq_len,
                n_heads: c.num_attention_heads,
                head_dim: c.head_dim(),
                window,
            };
            match &layer.attn_norm {
                Some(norm) => norm.forward(backend, &h.x, &mut h.normed, c.norm_eps)?,
                None => backend.copy(
                    &h.x,
                    0,
                    &mut h.normed,
                    0,
                    ws.n * ws.seq_len * c.hidden_size * 4,
                )?,
            }
            h.attention(
                backend,
                lens,
                &spec,
                Some(c.rope_theta(layer.kind)),
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
            )?;
            layer
                .mlp_norm
                .forward(backend, &h.x, &mut h.normed, c.norm_eps)?;
            layer.w_in.forward(backend, &h.normed, &mut ws.ff_a)?;
            layer.w_gate.forward(backend, &h.normed, &mut ws.ff_b)?;
            backend.gelu_mul(&ws.ff_a, &ws.ff_b, &mut ws.ff_act)?;
            layer.w_out.forward(backend, &ws.ff_act, &mut h.proj)?;
            backend.add(&h.x, &h.proj, &mut h.x_alt)?;
            std::mem::swap(&mut h.x, &mut h.x_alt);
        }
        self.final_norm
            .forward(backend, &h.x, &mut h.x_alt, c.norm_eps)?;
        std::mem::swap(&mut h.x, &mut h.x_alt);
        Ok(())
    }
}
