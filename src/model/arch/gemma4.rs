//! Dense Gemma 4 with per-layer inputs and shared KV. No host compute fallback.
use crate::backend::{AttnSpec, Backend, Error, Operation, Pool, QWeight, Result, RopeSpec};
use crate::gguf::GgufModel;
use crate::kv_cache::KvCache;
use crate::memory::{product, sum};
use crate::model::{Model, ModelConfig, load_norm, load_weight};

mod preflight;

pub(in crate::model) struct Config {
    pub local: Vec<bool>,
    pub ffn: Vec<usize>,
    pub producers: usize,
    pub local_dim: usize,
    pub global_dim: usize,
    pub per_layer: usize,
    pub window: usize,
    pub local_theta: f32,
    pub softcap: f32,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::ShapeMismatch(message.into())
}

impl Config {
    pub fn read(g: &GgufModel) -> Result<(ModelConfig, Self)> {
        let u = |key: &str| {
            g.metadata_u32(&format!("gemma4.{key}"))
                .map(|v| v as usize)
                .ok_or_else(|| invalid(format!("missing gemma4.{key}")))
        };
        let f = |key: &str| {
            g.metadata_f32(&format!("gemma4.{key}"))
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| invalid(format!("invalid gemma4.{key}")))
        };
        let n_layers = u("block_count")?;
        let n_embd = u("embedding_length")?;
        let n_heads = u("attention.head_count")?;
        let n_kv_heads = u("attention.head_count_kv")?;
        let n_ctx = u("context_length")?;
        let per_layer = u("embedding_length_per_layer_input")?;
        let shared = u("attention.shared_kv_layers")?;
        let local_dim = u("attention.key_length_swa")?;
        let global_dim = u("attention.key_length")?;
        let local = g
            .metadata_bool_array("gemma4.attention.sliding_window_pattern")
            .ok_or_else(|| invalid("missing Gemma attention pattern"))?;
        let ffn: Vec<usize> = g
            .metadata_u32_array("gemma4.feed_forward_length")
            .ok_or_else(|| invalid("missing Gemma FFN widths"))?
            .into_iter()
            .map(|v| v as usize)
            .collect();
        if n_layers == 0
            || n_layers > 256
            || n_embd == 0
            || n_heads == 0
            || n_kv_heads == 0
            || n_heads % n_kv_heads != 0
            || n_ctx == 0
            || per_layer == 0
            || local.len() != n_layers
            || ffn.len() != n_layers
            || ffn.contains(&0)
            || shared >= n_layers
            || local_dim == 0
            || global_dim == 0
            || local_dim % 2 != 0
            || global_dim % 2 != 0
            || u("attention.value_length_swa")? != local_dim
            || u("attention.value_length")? != global_dim
            || g.metadata_u32("gemma4.expert_count").unwrap_or(0) != 0
        {
            return Err(invalid("unsupported dense Gemma 4 geometry"));
        }
        let producers = n_layers - shared;
        if shared > 0 && (producers < 2 || !local[producers - 2] || local[producers - 1]) {
            return Err(invalid(
                "Gemma shared KV requires local/global producer pair",
            ));
        }
        let window = u("attention.sliding_window")?;
        if window == 0 {
            return Err(invalid("Gemma window must be nonzero"));
        }
        let config = ModelConfig {
            n_layers,
            n_embd,
            n_ff: *ffn.iter().max().expect("n_layers is validated nonzero"),
            n_heads,
            n_kv_heads,
            head_dim: global_dim,
            rope_theta: f("rope.freq_base")?,
            rms_eps: f("attention.layer_norm_rms_epsilon")?,
            n_ctx,
        };
        let extra = Self {
            local,
            ffn,
            producers,
            local_dim,
            global_dim,
            per_layer,
            window,
            local_theta: f("rope.freq_base_swa")?,
            softcap: f("final_logit_softcapping")?,
        };
        Ok((config, extra))
    }

    fn dim(&self, layer: usize) -> usize {
        if self.local[layer] {
            self.local_dim
        } else {
            self.global_dim
        }
    }
    fn source(&self, layer: usize) -> usize {
        if layer < self.producers {
            layer
        } else {
            self.producers - if self.local[layer] { 2 } else { 1 }
        }
    }
    pub fn kv_widths(&self, c: &ModelConfig) -> Result<Vec<usize>> {
        (0..self.producers)
            .map(|i| product(&[c.n_kv_heads, self.dim(i)]))
            .collect()
    }

    pub fn generated_weight_bytes(&self, c: &ModelConfig) -> Result<usize> {
        let widths = sum((0..c.n_layers).map(|i| self.dim(i)))?;
        product(&[sum([self.local_dim / 2, widths])?, 4])
    }

    pub fn workspace_bytes(&self, c: &ModelConfig) -> Result<usize> {
        let inputs = product(&[c.n_layers, self.per_layer, 4, 4])?;
        (0..c.n_layers).try_fold(inputs, |bytes, i| {
            sum([
                bytes,
                product(&[c.n_heads, self.dim(i), 3, 4])?,
                product(&[c.n_kv_heads, self.dim(i), 4, 4])?,
                product(&[self.ffn[i], 3, 4])?,
                product(&[self.per_layer, 3, 4])?,
            ])
        })
    }
}

struct Layer<B: Backend> {
    attn_norm: B::Buf,
    q: QWeight<B::Buf>,
    k: Option<QWeight<B::Buf>>,
    v: Option<QWeight<B::Buf>>,
    q_norm: B::Buf,
    k_norm: Option<B::Buf>,
    v_norm: B::Buf,
    out: QWeight<B::Buf>,
    post_attn: B::Buf,
    ffn_norm: B::Buf,
    gate: QWeight<B::Buf>,
    up: QWeight<B::Buf>,
    down: QWeight<B::Buf>,
    post_ffn: B::Buf,
    input_gate: QWeight<B::Buf>,
    input_proj: QWeight<B::Buf>,
    post_input: B::Buf,
    scale: f32,
}

pub(in crate::model) struct Gemma<B: Backend> {
    pub config: Config,
    layers: Vec<Layer<B>>,
    input_embd: QWeight<B::Buf>,
    input_proj: QWeight<B::Buf>,
    input_norm: B::Buf,
    local_factors: B::Buf,
    global_factors: B::Buf,
}

struct LayerScratch<B: Backend> {
    q: B::Buf,
    qn: B::Buf,
    k: B::Buf,
    kn: B::Buf,
    v: B::Buf,
    vn: B::Buf,
    attn: B::Buf,
    gate: B::Buf,
    up: B::Buf,
    act: B::Buf,
    input: B::Buf,
    input_gate: B::Buf,
    input_act: B::Buf,
}

pub(in crate::model) struct Workspace<B: Backend> {
    embd: B::Buf,
    proj: B::Buf,
    norm: B::Buf,
    inputs: B::Buf,
    layers: Vec<LayerScratch<B>>,
}

fn tensor(g: &GgufModel, name: &str, shape: &[usize]) -> Result<()> {
    let t = g
        .tensor_desc(name)
        .ok_or_else(|| invalid(format!("missing tensor {name}")))?;
    if t.shape != shape {
        return Err(invalid(format!(
            "{name}: expected {shape:?}, got {:?}",
            t.shape
        )));
    }
    Ok(())
}

fn weight<B: Backend>(
    b: &B,
    g: &mut GgufModel,
    name: &str,
    rows: usize,
    cols: usize,
) -> Result<QWeight<B::Buf>> {
    tensor(g, name, &[rows, cols])?;
    load_weight(b, g, name)
}

fn norm<B: Backend>(b: &B, g: &mut GgufModel, name: &str, n: usize) -> Result<B::Buf> {
    tensor(g, name, &[n])?;
    load_norm(b, g, name)
}

fn ones<B: Backend>(b: &B, n: usize) -> Result<B::Buf> {
    let mut buf = b.alloc(n * 4, Pool::Weights)?;
    b.upload(&mut buf, 0, bytemuck::cast_slice(&vec![1f32; n]))?;
    Ok(buf)
}

impl<B: Backend> Gemma<B> {
    pub fn load(b: &B, g: &mut GgufModel) -> Result<Model<B>> {
        let (c, config) = Self::validate(b, g)?;
        let vocab = g
            .tensor_desc("token_embd.weight")
            .and_then(|t| t.shape.first())
            .copied()
            .ok_or_else(|| invalid("missing Gemma vocabulary"))?;
        let token_embd = weight(b, g, "token_embd.weight", vocab, c.n_embd)?;
        let output_norm = norm(b, g, "output_norm.weight", c.n_embd)?;
        let output = if g.tensor_desc("output.weight").is_some() {
            Some(weight(b, g, "output.weight", vocab, c.n_embd)?)
        } else {
            None
        };
        let total = c.n_layers * config.per_layer;
        let input_embd = weight(b, g, "per_layer_token_embd.weight", vocab, total)?;
        let input_proj = weight(b, g, "per_layer_model_proj.weight", total, c.n_embd)?;
        let input_norm = norm(b, g, "per_layer_proj_norm.weight", config.per_layer)?;
        let local_factors = ones(b, config.local_dim / 2)?;
        let global_factors = norm(b, g, "rope_freqs.weight", config.global_dim / 2)?;
        let mut layers = Vec::with_capacity(c.n_layers);
        for i in 0..c.n_layers {
            let name = |s: &str| format!("blk.{i}.{s}.weight");
            let d = config.dim(i);
            let kv = i < config.producers;
            let scale = if let Some(desc) = g.tensor_desc(&name("layer_output_scale")) {
                if desc.shape != [1] || desc.dtype != crate::gguf::GgmlDType::F32 {
                    return Err(invalid("invalid Gemma layer scale"));
                }
                let raw = g
                    .read_tensor_bytes(&name("layer_output_scale"))
                    .map_err(|e| invalid(e.to_string()))?;
                let scale = f32::from_le_bytes(
                    raw.try_into()
                        .map_err(|_| invalid("invalid Gemma scale bytes"))?,
                );
                if !scale.is_finite() {
                    return Err(invalid("nonfinite Gemma layer scale"));
                }
                scale
            } else {
                1.0
            };
            layers.push(Layer {
                attn_norm: norm(b, g, &name("attn_norm"), c.n_embd)?,
                q: weight(b, g, &name("attn_q"), c.n_heads * d, c.n_embd)?,
                k: if kv {
                    Some(weight(b, g, &name("attn_k"), c.n_kv_heads * d, c.n_embd)?)
                } else {
                    None
                },
                v: if kv && g.tensor_desc(&name("attn_v")).is_some() {
                    Some(weight(b, g, &name("attn_v"), c.n_kv_heads * d, c.n_embd)?)
                } else {
                    None
                },
                q_norm: norm(b, g, &name("attn_q_norm"), d)?,
                k_norm: if kv {
                    Some(norm(b, g, &name("attn_k_norm"), d)?)
                } else {
                    None
                },
                v_norm: ones(b, d)?,
                out: weight(b, g, &name("attn_output"), c.n_embd, c.n_heads * d)?,
                post_attn: norm(b, g, &name("post_attention_norm"), c.n_embd)?,
                ffn_norm: norm(b, g, &name("ffn_norm"), c.n_embd)?,
                gate: weight(b, g, &name("ffn_gate"), config.ffn[i], c.n_embd)?,
                up: weight(b, g, &name("ffn_up"), config.ffn[i], c.n_embd)?,
                down: weight(b, g, &name("ffn_down"), c.n_embd, config.ffn[i])?,
                post_ffn: norm(b, g, &name("post_ffw_norm"), c.n_embd)?,
                input_gate: weight(b, g, &name("inp_gate"), config.per_layer, c.n_embd)?,
                input_proj: weight(b, g, &name("proj"), c.n_embd, config.per_layer)?,
                post_input: norm(b, g, &name("post_norm"), c.n_embd)?,
                scale,
            });
        }
        let gemma = Self {
            config,
            layers,
            input_embd,
            input_proj,
            input_norm,
            local_factors,
            global_factors,
        };
        Ok(Model {
            config: c,
            token_embd,
            output_norm,
            output,
            architecture: super::Decoder::Gemma4(gemma),
            memory: None,
        })
    }

    pub fn workspace(&self, b: &B, c: &ModelConfig) -> Result<Workspace<B>> {
        let alloc = |n: usize| b.alloc(n * 4, Pool::Scratch);
        let total = c.n_layers * self.config.per_layer;
        let layers = (0..c.n_layers)
            .map(|i| {
                let q = c.n_heads * self.config.dim(i);
                let kv = c.n_kv_heads * self.config.dim(i);
                let ff = self.config.ffn[i];
                let pe = self.config.per_layer;
                Ok(LayerScratch {
                    q: alloc(q)?,
                    qn: alloc(q)?,
                    k: alloc(kv)?,
                    kn: alloc(kv)?,
                    v: alloc(kv)?,
                    vn: alloc(kv)?,
                    attn: alloc(q)?,
                    gate: alloc(ff)?,
                    up: alloc(ff)?,
                    act: alloc(ff)?,
                    input: alloc(pe)?,
                    input_gate: alloc(pe)?,
                    input_act: alloc(pe)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Workspace {
            embd: alloc(total)?,
            proj: alloc(total)?,
            norm: alloc(total)?,
            inputs: alloc(total)?,
            layers,
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
        let extra = ws
            .gemma
            .as_mut()
            .ok_or_else(|| invalid("missing Gemma workspace"))?;
        b.embed(&model.token_embd, &[token], &mut ws.x)?;
        b.scale(&mut ws.x, (c.n_embd as f32).sqrt())?;
        b.embed(&self.input_embd, &[token], &mut extra.embd)?;
        b.scale(&mut extra.embd, (self.config.per_layer as f32).sqrt())?;
        b.qmatmul(&self.input_proj, &ws.x, &mut extra.proj)?;
        b.scale(&mut extra.proj, 1.0 / (c.n_embd as f32).sqrt())?;
        b.rmsnorm(&self.input_norm, &extra.proj, &mut extra.norm, c.rms_eps)?;
        b.add(&extra.embd, &extra.norm, &mut extra.inputs)?;
        b.scale(&mut extra.inputs, std::f32::consts::FRAC_1_SQRT_2)?;
        for (i, (l, s)) in self.layers.iter().zip(&mut extra.layers).enumerate() {
            let local = self.config.local[i];
            let d = self.config.dim(i);
            let factors = if local {
                &self.local_factors
            } else {
                &self.global_factors
            };
            let rope = RopeSpec {
                head_dim: d,
                theta: if local {
                    self.config.local_theta
                } else {
                    c.rope_theta
                },
            };
            b.rmsnorm(&l.attn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            b.qmatmul(&l.q, &ws.normed, &mut s.q)?;
            b.rmsnorm(&l.q_norm, &s.q, &mut s.qn, c.rms_eps)?;
            b.rope_split_half(&mut s.qn, pos, &rope, factors)?;
            if let Some(k) = &l.k {
                b.qmatmul(k, &ws.normed, &mut s.k)?;
                if let Some(v) = &l.v {
                    b.qmatmul(v, &ws.normed, &mut s.v)?;
                } else {
                    b.copy(&s.k, 0, &mut s.v, 0, c.n_kv_heads * d * 4)?;
                }
                b.rmsnorm(
                    l.k_norm
                        .as_ref()
                        .expect("KV-producing layers load a K norm"),
                    &s.k,
                    &mut s.kn,
                    c.rms_eps,
                )?;
                b.rmsnorm(&l.v_norm, &s.v, &mut s.vn, c.rms_eps)?;
                b.rope_split_half(&mut s.kn, pos, &rope, factors)?;
                kv.write(b, i, pos as usize, &s.kn, &s.vn, 1)?;
            }
            kv.set_len(pos as usize + 1);
            let attn = AttnSpec {
                head_dim: d,
                n_q_heads: c.n_heads,
                n_kv_heads: c.n_kv_heads,
            };
            b.attn_windowed(
                &s.qn,
                &kv.view(self.config.source(i)),
                &attn,
                1.0,
                if local { self.config.window } else { 0 },
                &mut s.attn,
            )?;
            b.qmatmul(&l.out, &s.attn, &mut ws.proj)?;
            b.rmsnorm(&l.post_attn, &ws.proj, &mut ws.normed, c.rms_eps)?;
            b.add(&ws.x, &ws.normed, &mut ws.x_alt)?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
            b.rmsnorm(&l.ffn_norm, &ws.x, &mut ws.normed, c.rms_eps)?;
            b.qmatmul(&l.gate, &ws.normed, &mut s.gate)?;
            b.qmatmul(&l.up, &ws.normed, &mut s.up)?;
            b.gelu_tanh_mul(&s.gate, &s.up, &mut s.act)?;
            b.qmatmul(&l.down, &s.act, &mut ws.proj)?;
            b.rmsnorm(&l.post_ffn, &ws.proj, &mut ws.normed, c.rms_eps)?;
            b.add(&ws.x, &ws.normed, &mut ws.x_alt)?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
            b.copy(
                &extra.inputs,
                i * self.config.per_layer * 4,
                &mut s.input,
                0,
                self.config.per_layer * 4,
            )?;
            b.qmatmul(&l.input_gate, &ws.x, &mut s.input_gate)?;
            b.gelu_tanh_mul(&s.input_gate, &s.input, &mut s.input_act)?;
            b.qmatmul(&l.input_proj, &s.input_act, &mut ws.proj)?;
            b.rmsnorm(&l.post_input, &ws.proj, &mut ws.normed, c.rms_eps)?;
            b.add(&ws.x, &ws.normed, &mut ws.x_alt)?;
            std::mem::swap(&mut ws.x, &mut ws.x_alt);
            b.scale(&mut ws.x, l.scale)?;
        }
        b.rmsnorm(&model.output_norm, &ws.x, &mut ws.normed, c.rms_eps)
    }
}
