//! Hybrid Qwen3.5 text decoder: gated DeltaNet layers, with every
//! `full_attention_interval`-th layer gated full attention. Multi-token
//! prediction blocks after the trunk are neither loaded nor run.
use super::Decoder;
use crate::backend::{
    AttnSpec, Backend, DeltaInputs, DeltaSpec, Error, Pool, QWeight, Result, RopeSpec,
};
use crate::gguf::GgufModel;
use crate::kv_cache::KvCache;
use crate::memory::{product, sum};
use crate::model::preflight::{positive, positive_f32, unsupported};
use crate::model::{Model, ModelConfig, load_norm, load_output, load_weight};

mod preflight;
#[cfg(test)]
mod tests;

pub(in crate::model) const ARCH: &str = "qwen35";

const KEYS: &[&str] = &[
    "block_count",
    "context_length",
    "embedding_length",
    "feed_forward_length",
    "attention.head_count",
    "attention.head_count_kv",
    "attention.key_length",
    "attention.value_length",
    "attention.layer_norm_rms_epsilon",
    "rope.freq_base",
    "rope.dimension_count",
    "rope.dimension_sections",
    "ssm.conv_kernel",
    "ssm.state_size",
    "ssm.group_count",
    "ssm.time_step_rank",
    "ssm.inner_size",
    "full_attention_interval",
    "nextn_predict_layers",
    "vocab_size",
];

pub(in crate::model) struct Config {
    /// Per trunk layer: DeltaNet when true, gated attention otherwise.
    pub recurrent: Vec<bool>,
    /// Trunk plus multi-token prediction blocks.
    pub block_count: usize,
    pub delta: DeltaSpec,
    pub conv_taps: usize,
    pub rotary_dim: usize,
}

/// Whether interleaved multi-section RoPE rotates every pair by the text
/// position. Pair `i` reads section `i % 3` while that section has pairs
/// left; the rest fall to the fourth section, whose text position is 0.
fn text_rope(sections: &[u32], pairs: usize) -> bool {
    let total: usize = sections.iter().map(|&s| s as usize).sum();
    sections.len() == 4
        && total > 0
        && (0..pairs).all(|i| {
            let sector = i % total;
            sector < 3 * sections[sector % 3] as usize
        })
}

impl Config {
    pub fn read(g: &GgufModel) -> Result<(ModelConfig, Self)> {
        let key = |suffix: &str| format!("{ARCH}.{suffix}");
        let u = |suffix: &str| positive(g, ARCH, suffix);
        let f = |suffix: &str| positive_f32(g, ARCH, suffix);
        if let Some(name) = g
            .metadata_keys()
            .filter_map(|k| k.strip_prefix(&key("")))
            .find(|suffix| !KEYS.contains(suffix))
        {
            return Err(unsupported(format!(
                "unimplemented architecture metadata {}",
                key(name)
            )));
        }
        let block_count = u("block_count")?;
        if block_count > g.tensor_names().count() {
            return Err(unsupported("block_count exceeds tensor count"));
        }
        let mtp = if g.has_metadata(&key("nextn_predict_layers")) {
            g.metadata_u32(&key("nextn_predict_layers"))
                .ok_or_else(|| unsupported("nextn_predict_layers must be an integer"))?
                as usize
        } else {
            0
        };
        // llama.cpp's default when the file names no interval.
        let interval = if g.has_metadata(&key("full_attention_interval")) {
            u("full_attention_interval")?
        } else {
            4
        };
        let n_layers = block_count
            .checked_sub(mtp)
            .filter(|&n| n >= interval)
            .ok_or_else(|| unsupported("Qwen3.5 needs at least one attention layer"))?;
        let c = ModelConfig {
            n_layers,
            n_embd: u("embedding_length")?,
            n_ff: u("feed_forward_length")?,
            n_heads: u("attention.head_count")?,
            n_kv_heads: u("attention.head_count_kv")?,
            head_dim: u("attention.key_length")?,
            rope_theta: f("rope.freq_base")?,
            rms_eps: f("attention.layer_norm_rms_epsilon")?,
            n_ctx: u("context_length")?,
        };
        let delta = DeltaSpec {
            n_k_heads: u("ssm.group_count")?,
            n_v_heads: u("ssm.time_step_rank")?,
            head_dim: u("ssm.state_size")?,
            eps: c.rms_eps,
        };
        let rotary_dim = u("rope.dimension_count")?;
        let conv_taps = u("ssm.conv_kernel")?;
        let sections = g
            .metadata_u32_array(&key("rope.dimension_sections"))
            .ok_or_else(|| unsupported("Qwen3.5 needs rope.dimension_sections"))?;
        if !c.n_heads.is_multiple_of(c.n_kv_heads)
            || !c.head_dim.is_multiple_of(2)
            || u("attention.value_length")? != c.head_dim
            || !rotary_dim.is_multiple_of(2)
            || rotary_dim > c.head_dim
            || !text_rope(&sections, rotary_dim / 2)
            || !delta.n_v_heads.is_multiple_of(delta.n_k_heads)
            || u("ssm.inner_size")? != product(&[delta.n_v_heads, delta.head_dim])?
            || conv_taps < 2
        {
            return Err(unsupported("unsupported Qwen3.5 geometry"));
        }
        product(&[2, c.n_heads, c.head_dim])?;
        product(&[c.n_kv_heads, c.head_dim])?;
        product(&[
            2 * delta.n_k_heads + delta.n_v_heads,
            delta.head_dim,
            conv_taps,
        ])?;
        product(&[delta.n_v_heads, delta.head_dim, delta.head_dim, 4])?;
        let config = Self {
            recurrent: (0..n_layers).map(|i| (i + 1) % interval != 0).collect(),
            block_count,
            delta,
            conv_taps,
            rotary_dim,
        };
        Ok((c, config))
    }

    /// Width of the convolved `[q | k | v]` row.
    fn conv_channels(&self) -> usize {
        (2 * self.delta.n_k_heads + self.delta.n_v_heads) * self.delta.head_dim
    }

    fn value_width(&self) -> usize {
        self.delta.n_v_heads * self.delta.head_dim
    }

    /// One KV layer per attention layer.
    pub fn kv_widths(&self, c: &ModelConfig) -> Result<Vec<usize>> {
        let attention = self.recurrent.iter().filter(|&&r| !r).count();
        Ok(vec![product(&[c.n_kv_heads, c.head_dim])?; attention])
    }

    /// Per DeltaNet layer: its convolution window, then its delta-rule state.
    pub fn state_bytes(&self) -> Result<Vec<usize>> {
        let conv = product(&[self.conv_channels(), self.conv_taps - 1, size_of::<f32>()])?;
        let delta = product(&[
            self.delta.n_v_heads,
            self.delta.head_dim,
            self.delta.head_dim,
            size_of::<f32>(),
        ])?;
        Ok(self
            .recurrent
            .iter()
            .filter(|&&r| r)
            .flat_map(|_| [conv, delta])
            .collect())
    }

    fn workspace_sizes(&self, c: &ModelConfig) -> Result<[usize; 9]> {
        let f32s = |n: usize| product(&[n, size_of::<f32>()]);
        let conv = f32s(self.conv_channels())?;
        let value = f32s(self.value_width())?;
        let heads = f32s(self.delta.n_v_heads)?;
        Ok([
            conv,
            conv,
            value,
            heads,
            heads,
            value,
            value,
            product(&[c.n_heads, c.head_dim, size_of::<f32>()])?,
            product(&[c.n_kv_heads, c.head_dim, size_of::<f32>()])?,
        ])
    }

    pub fn workspace_bytes(&self, c: &ModelConfig) -> Result<usize> {
        sum(self.workspace_sizes(c)?)
    }
}

enum Mixer<B: Backend> {
    Attention {
        q: QWeight<B::Buf>,
        q_gate: QWeight<B::Buf>,
        k: QWeight<B::Buf>,
        v: QWeight<B::Buf>,
        out: QWeight<B::Buf>,
        q_norm: B::Buf,
        k_norm: B::Buf,
        /// KV cache layer.
        slot: usize,
    },
    Delta {
        qkv: QWeight<B::Buf>,
        z: QWeight<B::Buf>,
        alpha: QWeight<B::Buf>,
        beta: QWeight<B::Buf>,
        conv: B::Buf,
        dt_bias: B::Buf,
        a: B::Buf,
        norm: B::Buf,
        out: QWeight<B::Buf>,
        /// The first of this layer's two recurrent state buffers.
        state: usize,
    },
}

struct Layer<B: Backend> {
    attn_norm: B::Buf,
    mixer: Mixer<B>,
    ffn_norm: B::Buf,
    gate: QWeight<B::Buf>,
    up: QWeight<B::Buf>,
    down: QWeight<B::Buf>,
}

pub(in crate::model) struct Qwen35<B: Backend> {
    layers: Vec<Layer<B>>,
    pub config: Config,
}

pub(in crate::model) struct Workspace<B: Backend> {
    qkv: B::Buf,
    conv: B::Buf,
    z: B::Buf,
    alpha: B::Buf,
    beta: B::Buf,
    delta: B::Buf,
    gated: B::Buf,
    q_gate: B::Buf,
    kn: B::Buf,
}

/// Splits a projection whose rows alternate one head of query with one head
/// of gate into separate query and gate weights. Rows are whole blocks in
/// every weight layout, so the split never re-encodes them.
fn load_query_gate<B: Backend>(
    b: &B,
    g: &mut GgufModel,
    name: &str,
    heads: usize,
    head_dim: usize,
) -> Result<[QWeight<B::Buf>; 2]> {
    let desc = g
        .tensor_desc(name)
        .ok_or_else(|| Error::ShapeMismatch(format!("missing tensor {name}")))?
        .clone();
    let &[rows, cols] = desc.shape.as_slice() else {
        return Err(Error::ShapeMismatch(format!("{name}: expected a matrix")));
    };
    let (raw, layout) = g
        .read_weight_bytes(name)
        .map_err(|e| Error::ShapeMismatch(e.to_string()))?;
    let block = raw.len() / (2 * heads);
    if rows != 2 * heads * head_dim || block == 0 || raw.len() != 2 * heads * block {
        return Err(Error::ShapeMismatch(format!(
            "{name}: rows are not whole query/gate heads"
        )));
    }
    let mut halves = [
        Vec::with_capacity(raw.len() / 2),
        Vec::with_capacity(raw.len() / 2),
    ];
    for pair in raw.chunks_exact(2 * block) {
        halves[0].extend_from_slice(&pair[..block]);
        halves[1].extend_from_slice(&pair[block..]);
    }
    let [q, gate] = halves;
    let weight = |bytes: &[u8]| -> Result<QWeight<B::Buf>> {
        Ok(QWeight {
            dtype: desc.dtype,
            shape: vec![heads * head_dim, cols],
            buf: b.upload_weight(bytes, layout)?,
        })
    };
    Ok([weight(&q)?, weight(&gate)?])
}

impl<B: Backend> Qwen35<B> {
    pub fn load(b: &B, g: &mut GgufModel) -> Result<Model<B>> {
        let (c, config) = Config::read(g)?;
        let token_embd = load_weight(b, g, "token_embd.weight")?;
        let output_norm = load_norm(b, g, "output_norm.weight")?;
        let output = load_output(b, g)?;
        let (mut slots, mut states) = (0, 0);
        let mut layers = Vec::with_capacity(c.n_layers);
        for (i, &recurrent) in config.recurrent.iter().enumerate() {
            let name = |s: &str| format!("blk.{i}.{s}.weight");
            let mixer = if recurrent {
                states += 2;
                Mixer::Delta {
                    qkv: load_weight(b, g, &name("attn_qkv"))?,
                    z: load_weight(b, g, &name("attn_gate"))?,
                    alpha: load_weight(b, g, &name("ssm_alpha"))?,
                    beta: load_weight(b, g, &name("ssm_beta"))?,
                    conv: load_norm(b, g, &name("ssm_conv1d"))?,
                    dt_bias: load_norm(b, g, &format!("blk.{i}.ssm_dt.bias"))?,
                    a: load_norm(b, g, &format!("blk.{i}.ssm_a"))?,
                    norm: load_norm(b, g, &name("ssm_norm"))?,
                    out: load_weight(b, g, &name("ssm_out"))?,
                    state: states - 2,
                }
            } else {
                let [q, q_gate] = load_query_gate(b, g, &name("attn_q"), c.n_heads, c.head_dim)?;
                slots += 1;
                Mixer::Attention {
                    q,
                    q_gate,
                    k: load_weight(b, g, &name("attn_k"))?,
                    v: load_weight(b, g, &name("attn_v"))?,
                    out: load_weight(b, g, &name("attn_output"))?,
                    q_norm: load_norm(b, g, &name("attn_q_norm"))?,
                    k_norm: load_norm(b, g, &name("attn_k_norm"))?,
                    slot: slots - 1,
                }
            };
            layers.push(Layer {
                attn_norm: load_norm(b, g, &name("attn_norm"))?,
                mixer,
                ffn_norm: load_norm(b, g, &name("post_attention_norm"))?,
                gate: load_weight(b, g, &name("ffn_gate"))?,
                up: load_weight(b, g, &name("ffn_up"))?,
                down: load_weight(b, g, &name("ffn_down"))?,
            });
        }
        Ok(Model {
            config: c,
            token_embd,
            output_norm,
            output,
            architecture: Decoder::Qwen35(Self { layers, config }),
            bos: None,
            memory: None,
        })
    }

    pub fn workspace(&self, b: &B, c: &ModelConfig) -> Result<Workspace<B>> {
        let s = self.config.workspace_sizes(c)?;
        Ok(Workspace {
            qkv: b.alloc(s[0], Pool::Scratch)?,
            conv: b.alloc(s[1], Pool::Scratch)?,
            z: b.alloc(s[2], Pool::Scratch)?,
            alpha: b.alloc(s[3], Pool::Scratch)?,
            beta: b.alloc(s[4], Pool::Scratch)?,
            delta: b.alloc(s[5], Pool::Scratch)?,
            gated: b.alloc(s[6], Pool::Scratch)?,
            q_gate: b.alloc(s[7], Pool::Scratch)?,
            kn: b.alloc(s[8], Pool::Scratch)?,
        })
    }

    pub fn run(
        &self,
        b: &B,
        model: &Model<B>,
        kv: &mut KvCache<B>,
        ws: &mut crate::model::Workspace<B>,
        token: u32,
        pos: u32,
    ) -> Result<()> {
        let c = &model.config;
        let e = ws.qwen35.as_mut().expect("Qwen3.5 workspace was allocated");
        if pos == 0 {
            kv.clear_states(b)?;
        }
        let rope = RopeSpec {
            head_dim: c.head_dim,
            theta: c.rope_theta,
        };
        let attn = AttnSpec {
            head_dim: c.head_dim,
            n_q_heads: c.n_heads,
            n_kv_heads: c.n_kv_heads,
        };
        b.embed(&model.token_embd, &[token], &mut ws.x)?;
        for l in &self.layers {
            b.rmsnorm(&l.attn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            match &l.mixer {
                Mixer::Attention {
                    q,
                    q_gate,
                    k,
                    v,
                    out,
                    q_norm,
                    k_norm,
                    slot,
                } => {
                    b.qmatmul_qkv(q, k, v, &ws.normed, &mut ws.q, &mut ws.k, &mut ws.v)?;
                    b.qmatmul(q_gate, &ws.normed, &mut e.q_gate)?;
                    b.rmsnorm(q_norm, &ws.q, &mut ws.attn_out, c.rms_eps)?;
                    std::mem::swap(&mut ws.q, &mut ws.attn_out);
                    b.rmsnorm(k_norm, &ws.k, &mut e.kn, c.rms_eps)?;
                    std::mem::swap(&mut ws.k, &mut e.kn);
                    b.rope_partial_split_half(&mut ws.q, pos, &rope, self.config.rotary_dim)?;
                    b.rope_partial_split_half(&mut ws.k, pos, &rope, self.config.rotary_dim)?;
                    kv.write(b, *slot, pos as usize, &ws.k, &ws.v, 1)?;
                    kv.set_len(pos as usize + 1);
                    b.attn(&ws.q, &kv.view(*slot), &attn, &mut ws.attn_out)?;
                    b.sigmoid_mul(&e.q_gate, &ws.attn_out, &mut ws.q)?;
                    b.qmatmul(out, &ws.q, &mut ws.proj)?;
                }
                Mixer::Delta {
                    qkv,
                    z,
                    alpha,
                    beta,
                    conv,
                    dt_bias,
                    a,
                    norm,
                    out,
                    state,
                } => {
                    b.qmatmul(qkv, &ws.normed, &mut e.qkv)?;
                    b.qmatmul(z, &ws.normed, &mut e.z)?;
                    b.qmatmul(alpha, &ws.normed, &mut e.alpha)?;
                    b.qmatmul(beta, &ws.normed, &mut e.beta)?;
                    b.ssm_conv_silu(conv, kv.state_mut(*state)?, &e.qkv, &mut e.conv)?;
                    let inputs = DeltaInputs {
                        qkv: &e.conv,
                        alpha: &e.alpha,
                        beta: &e.beta,
                        dt_bias,
                        a,
                    };
                    b.gated_delta_step(
                        &self.config.delta,
                        &inputs,
                        kv.state_mut(state + 1)?,
                        &mut e.delta,
                    )?;
                    b.rmsnorm(norm, &e.delta, &mut e.gated, c.rms_eps)?;
                    b.silu_mul(&e.z, &e.gated, &mut e.delta)?;
                    b.qmatmul(out, &e.delta, &mut ws.proj)?;
                }
            }
            b.add_rmsnorm(
                &l.ffn_norm,
                &ws.x,
                &ws.proj,
                &mut ws.x_alt,
                &mut ws.normed,
                c.rms_eps,
            )?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
            b.ffn_gate_up(
                &l.gate,
                &l.up,
                &ws.normed,
                &mut ws.gate,
                &mut ws.up,
                &mut ws.act,
            )?;
            b.qmatmul(&l.down, &ws.act, &mut ws.proj)?;
            b.add(&ws.x, &ws.proj, &mut ws.x_alt)?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
        }
        b.rmsnorm(&model.output_norm, &ws.x, &mut ws.normed, c.rms_eps)
    }
}
