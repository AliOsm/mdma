//! Laya type embeddings, decision layers, marker scorer, and action head
//! over a reusable ModernBERT encoder.
use super::config::{AgentConfig, EncoderConfig};
use super::prompt::Batch;
use super::weights::Weights;
use super::{Error, Result};
use crate::backend::{Backend, EncoderSpec, Pool, QWeight};
use crate::model::arch::modernbert::{self, ModernBert};
use crate::model::layers::{Linear, Norm};

// Fixed by the Laya checkpoint's decision/action-head implementation.
const HEAD_NORM_EPS: f32 = 1e-5;
const MASKED_OPTION_LOGIT: f32 = -1e4;
const ACTION_ENTROPY_EPS: f32 = 1e-9;
const OPTION_COUNT_SCALE: f32 = 255.0;
const ACTION_FEATURE_COUNT: usize = 4;

struct HeadLayer<B: Backend> {
    norm1: Norm<B>,
    wq: Linear<B>,
    wk: Linear<B>,
    wv: Linear<B>,
    wo: Linear<B>,
    norm2: Norm<B>,
    linear1: Linear<B>,
    linear2: Linear<B>,
}

pub struct DecisionModel<B: Backend> {
    encoder: ModernBert<B>,
    agent: AgentConfig,
    head: Vec<HeadLayer<B>>,
    type_emb: QWeight<B::Buf>,
    scorer_norm: Norm<B>,
    scorer_1: Linear<B>,
    scorer_2: Linear<B>,
    act_1: Linear<B>,
    act_2: Linear<B>,
}

/// Scratch for one batch shape `(n, seq_len, kmax)`.
pub struct Workspace<B: Backend> {
    n: usize,
    seq_len: usize,
    kmax: usize,
    encoder: modernbert::Workspace<B>,
    h1: B::Buf,
    h2: B::Buf,
    markers: B::Buf,
    s_norm: B::Buf,
    s_h: B::Buf,
    s_g: B::Buf,
    s_out: B::Buf,
    pooled: B::Buf,
    a_h: B::Buf,
    a_g: B::Buf,
    a_out: B::Buf,
}

pub struct Output {
    /// `[n][kmax]` raw marker logits, masked slots at -1e4.
    pub logits: Vec<Vec<f32>>,
    /// `[n][n_act]` action-head logits.
    pub act: Vec<Vec<f32>>,
}

impl<B: Backend> DecisionModel<B> {
    pub fn load(
        backend: &B,
        w: &Weights,
        encoder: EncoderConfig,
        agent: AgentConfig,
    ) -> Result<Self> {
        let d = encoder.hidden_size;
        let encoder = ModernBert::load(backend, w, encoder, "encoder")?;
        let norm = |name: &str, bias: bool| Norm::load(backend, w, name, bias);
        let linear = |name: &str, bias: bool| Linear::load(backend, w, name, bias);
        let mut head = Vec::with_capacity(agent.head_layers);
        for i in 0..agent.head_layers {
            let p = format!("head.layers.{i}");
            let qkv = |j| {
                Linear::part(
                    backend,
                    w,
                    &format!("{p}.self_attn.in_proj_weight"),
                    &format!("{p}.self_attn.in_proj_bias"),
                    true,
                    j,
                    d,
                )
            };
            head.push(HeadLayer {
                norm1: norm(&format!("{p}.norm1"), true)?,
                wq: qkv(0)?,
                wk: qkv(1)?,
                wv: qkv(2)?,
                wo: linear(&format!("{p}.self_attn.out_proj"), true)?,
                norm2: norm(&format!("{p}.norm2"), true)?,
                linear1: linear(&format!("{p}.linear1"), true)?,
                linear2: linear(&format!("{p}.linear2"), true)?,
            });
        }
        let model = Self {
            head,
            type_emb: w.matrix(backend, "type_emb.weight", None)?,
            scorer_norm: norm("scorer.0", true)?,
            scorer_1: linear("scorer.1", true)?,
            scorer_2: linear("scorer.3", true)?,
            act_1: linear("act_head.0", true)?,
            act_2: linear("act_head.2", true)?,
            encoder,
            agent,
        };
        let act_in = model.act_1.w.shape[1];
        let n_act = model.act_2.w.shape[0];
        if act_in != d + ACTION_FEATURE_COUNT
            || n_act != model.agent.n_act()
            || model.type_emb.shape != [3, d]
        {
            return Err(Error::Checkpoint(format!(
                "head geometry: act_head {act_in}->{n_act}, type_emb {:?}, hidden {d}",
                model.type_emb.shape
            )));
        }
        Ok(model)
    }

    pub fn encoder_config(&self) -> &EncoderConfig {
        self.encoder.config()
    }

    pub fn agent_config(&self) -> &AgentConfig {
        &self.agent
    }

    /// Checked total of the workspace's logical tensor allocations. Attention
    /// scratch is reused across encoder/head layers, not allocated per layer.
    pub(crate) fn workspace_bytes(&self, n: usize, seq_len: usize, kmax: usize) -> Result<usize> {
        use crate::memory::{product, sum};
        let d = self.encoder.config().hidden_size;
        let inter = self.encoder.config().intermediate_size;
        let elements = sum([
            product(&[16, n, seq_len, d])?, // 8 attention + two four-width head tensors.
            product(&[3, n, seq_len, inter])?,
            product(&[4, n, kmax, d])?, // markers, norm, scorer hidden + activation.
            product(&[n, kmax])?,
            product(&[n, d])?,
            product(&[n, ACTION_FEATURE_COUNT])?,
            product(&[n, 512])?,
            product(&[n, self.agent.n_act()])?,
        ])?;
        Ok(product(&[elements, size_of::<f32>()])?)
    }

    pub fn alloc_workspace(
        &self,
        backend: &B,
        n: usize,
        seq_len: usize,
        kmax: usize,
    ) -> Result<Workspace<B>> {
        let d = self.encoder.config().hidden_size;
        let t = n * seq_len;
        let rows = |r: usize, c: usize| backend.alloc(r * c * size_of::<f32>(), Pool::Scratch);
        Ok(Workspace {
            n,
            seq_len,
            kmax,
            encoder: self.encoder.alloc_workspace(backend, n, seq_len)?,
            h1: rows(t, 4 * d)?,
            h2: rows(t, 4 * d)?,
            markers: rows(n * kmax, d)?,
            s_norm: rows(n * kmax, d)?,
            s_h: rows(n * kmax, d)?,
            s_g: rows(n * kmax, d)?,
            s_out: rows(n * kmax, 1)?,
            pooled: rows(n, d + ACTION_FEATURE_COUNT)?,
            a_h: rows(n, 256)?,
            a_g: rows(n, 256)?,
            a_out: rows(n, self.agent.n_act())?,
        })
    }

    pub fn forward(&self, backend: &B, ws: &mut Workspace<B>, batch: &Batch) -> Result<Output> {
        if (ws.n, ws.seq_len, ws.kmax) != (batch.n, batch.seq_len, batch.kmax) {
            return Err(Error::Checkpoint(
                "workspace does not match the batch shape".into(),
            ));
        }
        let config = self.encoder.config();
        let d = config.hidden_size;
        let spec = EncoderSpec {
            seq_len: batch.seq_len,
            n_heads: config.num_attention_heads,
            head_dim: config.head_dim(),
            window: None,
        };
        self.encoder
            .forward(backend, &mut ws.encoder, &batch.ids, &batch.lens)?;
        // Reuse the encoder's attention scratch for decision layers.
        let h = &mut ws.encoder.hidden;

        // h += type_emb[qtype], broadcast over the sequence.
        let type_ids: Vec<u32> = batch
            .kinds
            .iter()
            .flat_map(|k| std::iter::repeat_n(k.index(), batch.seq_len))
            .collect();
        backend.embed(&self.type_emb, &type_ids, &mut h.proj)?;
        backend.add(&h.x, &h.proj, &mut h.x_alt)?;
        std::mem::swap(&mut h.x, &mut h.x_alt);

        // PyTorch TransformerEncoderLayer, norm_first, ReLU.
        for layer in &self.head {
            layer
                .norm1
                .forward(backend, &h.x, &mut h.normed, HEAD_NORM_EPS)?;
            h.attention(
                backend,
                &batch.lens,
                &spec,
                None,
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
            )?;
            layer
                .norm2
                .forward(backend, &h.x, &mut h.normed, HEAD_NORM_EPS)?;
            layer.linear1.forward(backend, &h.normed, &mut ws.h1)?;
            backend.relu(&ws.h1, &mut ws.h2)?;
            layer.linear2.forward(backend, &ws.h2, &mut h.proj)?;
            backend.add(&h.x, &h.proj, &mut h.x_alt)?;
            std::mem::swap(&mut h.x, &mut h.x_alt);
        }

        for b in 0..batch.n {
            for j in 0..batch.kmax {
                let pos = batch.markers[b * batch.kmax + j];
                backend.copy(
                    &h.x,
                    (b * batch.seq_len + pos) * d * size_of::<f32>(),
                    &mut ws.markers,
                    (b * batch.kmax + j) * d * size_of::<f32>(),
                    d * size_of::<f32>(),
                )?;
            }
        }
        self.scorer_norm
            .forward(backend, &ws.markers, &mut ws.s_norm, HEAD_NORM_EPS)?;
        self.scorer_1.forward(backend, &ws.s_norm, &mut ws.s_h)?;
        backend.gelu(&ws.s_h, &mut ws.s_g)?;
        self.scorer_2.forward(backend, &ws.s_g, &mut ws.s_out)?;
        let mut flat = vec![0f32; batch.n * batch.kmax];
        backend.download(&ws.s_out, 0, bytemuck::cast_slice_mut(&mut flat))?;

        // Action head: pooled [CLS] row plus a summary of the answer distribution.
        let mut logits = Vec::with_capacity(batch.n);
        let mut pooled = vec![0f32; batch.n * (d + ACTION_FEATURE_COUNT)];
        for b in 0..batch.n {
            let row = &mut flat[b * batch.kmax..(b + 1) * batch.kmax];
            for slot in row.iter_mut().skip(batch.counts[b]) {
                *slot = MASKED_OPTION_LOGIT;
            }
            let p = super::calibrate::softmax(row);
            let k = batch.counts[b].max(2) as f32;
            let entropy = -p
                .iter()
                .map(|&v| v * v.max(ACTION_ENTROPY_EPS).ln())
                .sum::<f32>()
                / k.ln();
            let mut sorted = p.clone();
            sorted.sort_by(|a, b| b.total_cmp(a));
            let out =
                &mut pooled[b * (d + ACTION_FEATURE_COUNT)..(b + 1) * (d + ACTION_FEATURE_COUNT)];
            backend.download(
                &h.x,
                b * batch.seq_len * d * size_of::<f32>(),
                bytemuck::cast_slice_mut(&mut out[..d]),
            )?;
            out[d..].copy_from_slice(&[
                sorted[0],
                sorted[0] - sorted[1],
                entropy,
                k / OPTION_COUNT_SCALE,
            ]);
            logits.push(row.to_vec());
        }
        backend.upload(&mut ws.pooled, 0, bytemuck::cast_slice(&pooled))?;
        self.act_1.forward(backend, &ws.pooled, &mut ws.a_h)?;
        backend.gelu(&ws.a_h, &mut ws.a_g)?;
        self.act_2.forward(backend, &ws.a_g, &mut ws.a_out)?;
        let n_act = self.agent.n_act();
        let mut act_flat = vec![0f32; batch.n * n_act];
        backend.download(&ws.a_out, 0, bytemuck::cast_slice_mut(&mut act_flat))?;
        let act = act_flat.chunks_exact(n_act).map(<[f32]>::to_vec).collect();
        Ok(Output { logits, act })
    }
}
