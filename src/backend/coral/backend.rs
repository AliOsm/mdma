//! `CoralBackend`: the CPU backend with a Llama model's decode matmuls on the
//! Edge TPU. Every op runs on the CPU unless a decoder is attached: `decoder`
//! sees the whole model, calibrates the activation ranges on stories the float
//! model writes, quantizes each layer's q|k|v, o, gate|up and down weights to
//! uint8 (one scale per compute tile), places them side by side in the tiles'
//! wide memory and keeps their programs loaded. While that decoder lives, a
//! one-token matmul on those weights runs on the chip: the input is quantized
//! over its own range, every tile's requantization is patched into the program,
//! and an output a tile clamped (0 or 255) is recomputed on the CPU. Prefill,
//! attention, norms and the classifier stay on the CPU.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "token ids, positions and seeds are small integers"
)]

use super::codegen::fc::{FcPrograms, param_blob};
use super::codegen::layer::{self, LayerQuant};
use super::codegen::{Quant, patch_quant};
use super::model::{self as host_model, DM, FloatLayer, FloatWeights};
use super::plan;
use super::quant::{Affine, Observed, Stack, quantize_input, quantize_rows};
use super::tpu::{Loaded, Options, Parameters, Tpu};
use super::whole::{Kv8, Whole, dequantize_kv, quantize_kv};
use crate::backend::cpu::{CpuBackend, CpuBuf, dequant_row};
use crate::backend::{
    AttnSpec, Backend, EncoderSpec, Error, KvView, Operation, Pool, QWeight, Result, RopeSpec,
};
use crate::gguf::GgmlDType;
use crate::kv_cache::KvCache;
use crate::model::decoder::{Decoder, EagerDecoder};
use crate::model::{LlamaMatmuls, Model, Workspace};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Calibration stories, their length (at most the model's context) and seed.
const STORIES: usize = 8;
const STORY_TOKENS: usize = 256;
const SEED: u64 = 0x5eed_c0a1;

/// How a decoder uses the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    /// The decode matmuls on the chip, the rest on the CPU: the closest to the float
    /// model.
    Matmul,
    /// The whole model per token when its geometry fits the program (else the
    /// matmuls): the CPU only runs the classifier.
    Model,
}

pub struct CoralBackend {
    cpu: CpuBackend,
    decode: Decode,
    chip: Mutex<Chip>,
    /// Serializes attaching, which drops the chip lock while it calibrates.
    attaching: Mutex<()>,
}

/// What the chip holds and whether matmuls route to it.
struct Chip {
    tpu: Tpu,
    model: Option<OnChip>,
    /// The whole model on the chip, when its geometry fits the program.
    whole: Option<Whole>,
    mode: Mode,
    /// Live decoders of the resident model.
    attached: usize,
    /// While calibrating: whether this forward's outputs count (not the BOS position).
    record: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Idle,
    /// Matmuls run on the CPU and their output ranges are recorded.
    Record,
    Run,
}

/// The resident model: its matmuls, found by the address of their first weight.
struct OnChip {
    fingerprint: u64,
    sites: HashMap<usize, usize>,
    matmuls: Vec<Matmul>,
    /// The classifier on the host in int8, by its weight's address.
    head: Option<(usize, Arc<host_model::Int8Rows>)>,
}

struct Matmul {
    stack: Stack,
    /// Weight buffer addresses of the stacked parts.
    weights: Vec<usize>,
    /// Output range per compute tile, from calibration.
    observed: Vec<Observed>,
    program: Option<Program>,
}

struct Program {
    programs: FcPrograms,
    parameters: Parameters,
    loaded: Loaded,
    /// Weight and output quantization per compute tile.
    tiles: Vec<(Affine, Affine)>,
}

fn address(w: &QWeight<CpuBuf>) -> usize {
    w.buf.0.as_ptr() as usize
}

fn f32s(buf: &CpuBuf) -> Result<&[f32]> {
    bytemuck::try_cast_slice(&buf.0).map_err(|e| Error::ShapeMismatch(e.to_string()))
}

fn f32s_mut(buf: &mut CpuBuf) -> Result<&mut [f32]> {
    bytemuck::try_cast_slice_mut(&mut buf.0).map_err(|e| Error::ShapeMismatch(e.to_string()))
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("coral: {error}"))
}

/// Row `row` of `w` dotted with `x`, in f32: the CPU's answer for one output.
fn cpu_dot(w: &QWeight<CpuBuf>, row: usize, x: &[f32]) -> Result<f32> {
    let mut values = vec![0f32; x.len()];
    dequant_row(w, row, &mut values)?;
    Ok(host_model::dot(&values, x))
}

/// A layer's matmul weights in the order the chip stacks them.
fn matmuls<'a>(l: &LlamaMatmuls<'a, CpuBuf>) -> [&'a QWeight<CpuBuf>; 7] {
    [l.wq, l.wk, l.wv, l.wo, l.w_gate, l.w_up, l.w_down]
}

/// Names the model's cache directory: every tensor's shape, size and sampled
/// bytes, so another model never reads its calibration or programs.
fn weights_key<B: Backend<Buf = CpuBuf>>(model: &Model<B>) -> Option<String> {
    use sha2::{Digest, Sha256};
    let (layers, norms) = (model.llama_matmuls()?, model.llama_norms()?);
    let mut h = Sha256::new();
    h.update(b"mdma-coral-weights-v2");
    let mut tensor = |shape: &[usize], bytes: &[u8]| {
        for d in shape {
            h.update((*d as u64).to_le_bytes());
        }
        h.update((bytes.len() as u64).to_le_bytes());
        for i in 0..64 {
            let at = bytes.len().saturating_sub(8) * i / 63;
            if let Some(word) = bytes.get(at..at + 8) {
                h.update(word);
            }
        }
    };
    let (emb, norm, out) = model.head();
    for w in [emb, out] {
        tensor(&w.shape, &w.buf.0);
    }
    tensor(&[], &norm.0);
    for (l, (attn_norm, ffn_norm)) in layers.iter().zip(norms) {
        for w in matmuls(l) {
            tensor(&w.shape, &w.buf.0);
        }
        tensor(&[], &attn_norm.0);
        tensor(&[], &ffn_norm.0);
    }
    Some(hex::encode(h.finalize())[..32].to_owned())
}

/// The resident model's identity: its weights' contents and, since the matmul
/// mode routes by weight address, where they live.
fn fingerprint(key: &str, layers: &[LlamaMatmuls<'_, CpuBuf>]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    key.hash(&mut h);
    for w in layers.iter().flat_map(matmuls) {
        address(w).hash(&mut h);
    }
    h.finish()
}

/// The model's directory in the cache.
fn cache_dir(key: &str) -> Option<std::path::PathBuf> {
    crate::cache::cache_root()
        .ok()
        .map(|root| root.join("coral").join(key))
}

impl CoralBackend {
    /// Opens the card at `MDMA_CORAL_DEVICE`, or the platform's first card;
    /// `MDMA_CORAL_MODE=model` puts a whole model that fits on the chip.
    pub fn new() -> Result<Self> {
        let mut options = Options::default();
        if let Ok(device) = std::env::var("MDMA_CORAL_DEVICE") {
            options.device = device;
        }
        let decode = match std::env::var("MDMA_CORAL_MODE").as_deref() {
            Ok("matmul") | Err(_) => Decode::Matmul,
            Ok("model") => Decode::Model,
            Ok(other) => {
                return Err(Error::Backend(format!(
                    "MDMA_CORAL_MODE must be model or matmul, not {other:?}"
                )));
            }
        };
        Self::with_decode(options, decode)
    }

    pub fn open(options: Options) -> Result<Self> {
        Self::with_decode(options, Decode::Matmul)
    }

    pub fn with_decode(options: Options, decode: Decode) -> Result<Self> {
        let tpu = Tpu::open(options)?;
        Ok(Self {
            cpu: CpuBackend,
            decode,
            chip: Mutex::new(Chip {
                tpu,
                model: None,
                whole: None,
                mode: Mode::Idle,
                attached: 0,
                record: false,
            }),
            attaching: Mutex::new(()),
        })
    }

    fn chip(&self) -> Result<MutexGuard<'_, Chip>> {
        self.chip.lock().map_err(backend_error)
    }

    /// Runs the stacked matmul whose first weight is `weights[0]` for one token,
    /// on the chip or, while calibrating, on the CPU with its ranges recorded.
    /// `false` when these weights are not routed to the chip.
    fn try_chip(
        &self,
        weights: &[&QWeight<CpuBuf>],
        x: &CpuBuf,
        outs: &mut [&mut CpuBuf],
    ) -> Result<bool> {
        let mut chip = self.chip()?;
        let routed = match chip.mode {
            Mode::Idle => false,
            Mode::Record => true,
            Mode::Run => chip.attached > 0,
        };
        let Some(model) = chip.model.as_ref().filter(|_| routed) else {
            return Ok(false);
        };
        let Some(&index) = model.sites.get(&address(weights[0])) else {
            return Ok(false);
        };
        let m = &model.matmuls[index];
        let x_f32 = f32s(x)?;
        if m.weights.len() != weights.len()
            || m.weights.iter().zip(weights).any(|(a, w)| *a != address(w))
            || x_f32.len() != m.stack.k
        {
            return Ok(false);
        }
        if chip.mode == Mode::Record {
            for (w, out) in weights.iter().zip(outs.iter_mut()) {
                self.cpu.qmatmul(w, x, out)?;
            }
            if chip.record {
                let parts: Vec<&[f32]> = outs.iter().map(|o| f32s(o)).collect::<Result<_>>()?;
                let model = chip
                    .model
                    .as_mut()
                    .ok_or_else(|| backend_error("no model"))?;
                let m = &mut model.matmuls[index];
                for (t, observed) in m.observed.iter_mut().enumerate() {
                    for row in m.stack.tile_span(t) {
                        if let Some((part, r)) = m.stack.locate(row) {
                            observed.update(&[parts[part][r]]);
                        }
                    }
                }
            }
            return Ok(true);
        }
        let Chip { tpu, model, .. } = &mut *chip;
        let m = &mut model
            .as_mut()
            .ok_or_else(|| backend_error("no model"))?
            .matmuls[index];
        let Some(program) = m.program.as_mut() else {
            return Ok(false);
        };
        let input = Affine::covering(x_f32);
        for (&word, (w, out)) in program.programs.ops.iter().zip(&program.tiles) {
            let quant = Quant {
                in_zp: input.zp,
                w_zp: w.zp,
                out_zp: out.zp,
                mult: input.scale * w.scale / out.scale,
                lo: 0,
                hi: 255,
            };
            patch_quant(program.loaded.chunk_mut(0)?, word, &quant)?;
        }
        quantize_input(x_f32, input, program.loaded.input_mut("x")?);
        tpu.invoke(&program.loaded)?;
        let y = program.loaded.output("y")?;
        let mut parts: Vec<&mut [f32]> = outs
            .iter_mut()
            .map(|o| f32s_mut(o))
            .collect::<Result<_>>()?;
        for (t, (_, out)) in program.tiles.iter().enumerate() {
            for row in m.stack.tile_span(t) {
                let Some((part, r)) = m.stack.locate(row) else {
                    continue;
                };
                let q = y[row];
                parts[part][r] = if q == 0 || q == 255 {
                    cpu_dot(weights[part], r, x_f32)?
                } else {
                    out.dequantize(q)
                };
            }
        }
        Ok(true)
    }

    /// Makes `model`'s matmuls resident (calibrating them first) and counts one
    /// more decoder on them; `false` when the model cannot use the chip.
    fn attach(&self, model: &Model<Self>) -> Result<bool> {
        let (Some(layers), Some(key)) = (model.llama_matmuls(), weights_key(model)) else {
            return Ok(false);
        };
        let _attaching = self.attaching.lock().map_err(backend_error)?;
        let fingerprint = fingerprint(&key, &layers);
        {
            let mut chip = self.chip()?;
            if chip
                .model
                .as_ref()
                .is_some_and(|m| m.fingerprint == fingerprint)
                && chip.mode == Mode::Run
            {
                chip.attached += 1;
                return Ok(true);
            }
            if chip.attached > 0 || chip.whole.as_ref().is_some_and(|w| w.attached > 0) {
                // Another model's decoders hold the chip.
                return Ok(false);
            }
        }
        let Some(on_chip) = Self::layout(&layers, fingerprint) else {
            eprintln!("mdma: coral: the model's matmuls do not fit the chip; decoding on the CPU");
            return Ok(false);
        };
        {
            let mut chip = self.chip()?;
            Self::release(&mut chip)?;
            chip.model = Some(on_chip);
            chip.mode = Mode::Record;
        }
        let start = Instant::now();
        let cache = cache_dir(&key).map(|dir| dir.join("matmul-ranges.json"));
        let calibrated = match cache.as_deref().and_then(read_observed) {
            Some(observed) => {
                let mut chip = self.chip()?;
                let on_chip = chip
                    .model
                    .as_mut()
                    .ok_or_else(|| backend_error("no model"))?;
                if observed.len() == on_chip.matmuls.len()
                    && on_chip
                        .matmuls
                        .iter()
                        .zip(&observed)
                        .all(|(m, o)| m.observed.len() == o.len())
                {
                    for (m, o) in on_chip.matmuls.iter_mut().zip(observed) {
                        m.observed = o;
                    }
                    Ok(0)
                } else {
                    drop(chip);
                    self.calibrate(model)
                }
            }
            None => self.calibrate(model),
        };
        let mut chip = self.chip()?;
        if let Err(error) = calibrated.and_then(|tokens| {
            let calibration = start.elapsed();
            if tokens > 0
                && let (Some(path), Some(on_chip)) = (&cache, chip.model.as_ref())
            {
                let observed: Vec<Vec<Observed>> =
                    on_chip.matmuls.iter().map(|m| m.observed.clone()).collect();
                if let Err(error) = write_observed(path, &observed) {
                    eprintln!("mdma: coral: calibration not cached at {}: {error}", path.display());
                }
            }
            Self::upload(&mut chip, &layers)?;
            let out = model.head().2;
            let head = host_model::Int8Rows::new(&dequant_all(out)?, out.shape[1]);
            if let Some(on_chip) = chip.model.as_mut() {
                on_chip.head = Some((address(out), Arc::new(head)));
            }
            if tokens > 0 {
                eprintln!(
                    "mdma: coral: calibrated on {tokens} tokens in {calibration:.2?}, weights on the chip in {:.2?}",
                    start.elapsed().saturating_sub(calibration)
                );
            } else {
                eprintln!(
                    "mdma: coral: calibration read from the cache, weights on the chip in {:.2?}",
                    start.elapsed()
                );
            }
            Ok(())
        }) {
            Self::release(&mut chip)?;
            return Err(error);
        }
        chip.mode = Mode::Run;
        chip.attached += 1;
        Ok(true)
    }

    /// The int8 classifier of the attached model when `w` is its weight.
    fn head(&self, w: &QWeight<CpuBuf>) -> Option<Arc<host_model::Int8Rows>> {
        let chip = self.chip.lock().ok()?;
        if chip.mode != Mode::Run || chip.attached == 0 {
            return None;
        }
        let (at, rows) = chip.model.as_ref()?.head.as_ref()?;
        (*at == address(w)).then(|| Arc::clone(rows))
    }

    fn detach(&self) {
        if let Ok(mut chip) = self.chip.lock() {
            chip.attached = chip.attached.saturating_sub(1);
        }
    }

    /// Each layer's stacks: q|k|v, o, gate|up, down; `None` if they do not fit.
    fn layout(layers: &[LlamaMatmuls<'_, CpuBuf>], fingerprint: u64) -> Option<OnChip> {
        let mut matmuls = Vec::new();
        let mut sites = HashMap::new();
        for l in layers {
            for group in [
                vec![l.wq, l.wk, l.wv],
                vec![l.wo],
                vec![l.w_gate, l.w_up],
                vec![l.w_down],
            ] {
                let k = group[0].shape.get(1).copied()?;
                if group.iter().any(|w| w.shape.len() != 2 || w.shape[1] != k) {
                    return None;
                }
                let parts: Vec<usize> = group.iter().map(|w| w.shape[0]).collect();
                let stack = Stack::new(&parts, k)?;
                sites.insert(address(group[0]), matmuls.len());
                matmuls.push(Matmul {
                    observed: vec![Observed::default(); stack.tiles()],
                    stack,
                    weights: group.iter().map(|w| address(w)).collect(),
                    program: None,
                });
            }
        }
        let programs: Vec<_> = matmuls.iter().map(|m| m.stack.program).collect();
        plan::place(&programs)?;
        Some(OnChip {
            fingerprint,
            sites,
            matmuls,
            head: None,
        })
    }

    /// Calibration: `STORIES` sequences of up to `STORY_TOKENS` the float model
    /// samples from BOS, every matmul output after position 0 recorded. Returns the
    /// tokens run.
    fn calibrate(&self, model: &Model<Self>) -> Result<usize> {
        let n_ctx = model.config().n_ctx.min(STORY_TOKENS);
        let bos = model.bos_token().unwrap_or(1);
        let mut kv = model.new_kv_cache(self, n_ctx)?;
        let mut ws = model.alloc_workspace(self, 1)?;
        let mut rng = SEED;
        let mut tokens = 0;
        for _ in 0..STORIES {
            kv.set_len(0);
            let mut token = bos;
            for pos in 0..n_ctx {
                self.chip()?.record = pos > 0;
                let logits = model.forward(self, &mut kv, &mut ws, token, pos as u32)?;
                tokens += 1;
                token = host_model::sample(&logits, 1.0, &mut rng);
            }
        }
        self.chip()?.record = false;
        Ok(tokens)
    }

    /// Quantizes, places and caches every matmul, and loads its program.
    fn upload(chip: &mut Chip, layers: &[LlamaMatmuls<'_, CpuBuf>]) -> Result<()> {
        let Chip { tpu, model, .. } = chip;
        let model = model.as_mut().ok_or_else(|| backend_error("no model"))?;
        let weights: Vec<&QWeight<CpuBuf>> = layers.iter().flat_map(matmuls).collect();
        let by_address: HashMap<usize, &QWeight<CpuBuf>> =
            weights.iter().map(|w| (address(w), *w)).collect();
        let programs: Vec<_> = model.matmuls.iter().map(|m| m.stack.program).collect();
        let placements =
            plan::place(&programs).ok_or_else(|| backend_error("the model no longer fits"))?;
        for (m, at) in model.matmuls.iter_mut().zip(placements) {
            let k = m.stack.k;
            let parts: Vec<Vec<f32>> = m
                .weights
                .iter()
                .map(|a| dequant_all(by_address[a]))
                .collect::<Result<_>>()?;
            let rows = m.stack.n();
            let k4 = k.next_multiple_of(4);
            let mut data = Vec::with_capacity(rows * k4);
            let mut tiles = Vec::with_capacity(m.stack.tiles());
            for (t, observed) in m.observed.iter().enumerate() {
                let span: Vec<Option<&[f32]>> = m
                    .stack
                    .tile_span(t)
                    .map(|row| {
                        m.stack
                            .locate(row)
                            .map(|(part, r)| &parts[part][r * k..(r + 1) * k])
                    })
                    .collect();
                let q = quantize_rows(&span, k);
                data.extend_from_slice(&q.data);
                tiles.push((q.weights, observed.affine()));
            }
            let blob = param_blob(&data, rows, k4, 0, Some(&vec![0; rows]), 64);
            let programs = m
                .stack
                .program
                .programs(at.shift, at.offset, Quant::REFERENCE)?;
            let parameters = tpu.cache_parameters(&programs.caching, &blob)?;
            match tpu.load(&programs.execution, Some(&parameters)) {
                Ok(loaded) => {
                    m.program = Some(Program {
                        programs,
                        parameters,
                        loaded,
                        tiles,
                    });
                }
                Err(error) => {
                    tpu.unload_parameters(parameters)?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    /// Unloads the resident model's programs and parameters, both forms.
    fn release(chip: &mut Chip) -> Result<()> {
        if let Some(mut whole) = chip.whole.take() {
            whole.release(&mut chip.tpu)?;
        }
        chip.mode = Mode::Idle;
        let Some(model) = chip.model.take() else {
            return Ok(());
        };
        for m in model.matmuls {
            if let Some(p) = m.program {
                chip.tpu.unload(p.loaded)?;
                chip.tpu.unload_parameters(p.parameters)?;
            }
        }
        Ok(())
    }
}

fn read_observed(path: &std::path::Path) -> Option<Vec<Vec<Observed>>> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    v.as_array()?
        .iter()
        .map(|m| {
            m.as_array()?
                .iter()
                .map(|r| {
                    let r = r.as_array()?;
                    let (min, max) = (r.first()?.as_f64()?, r.get(1)?.as_f64()?);
                    Some(Observed {
                        min: min as f32,
                        max: max as f32,
                    })
                })
                .collect()
        })
        .collect()
}

fn write_observed(path: &std::path::Path, observed: &[Vec<Observed>]) -> std::io::Result<()> {
    // An empty range has infinite bounds, which JSON cannot hold: it is written as (0, 0).
    let doc: Vec<Vec<[f32; 2]>> = observed
        .iter()
        .map(|m| {
            m.iter()
                .map(|o| {
                    if o.is_empty() {
                        [0.0, 0.0]
                    } else {
                        [o.min, o.max]
                    }
                })
                .collect()
        })
        .collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        serde_json::to_vec(&doc).map_err(std::io::Error::other)?,
    )
}

/// Calibration of the whole-model program: stories and their seed.
const WHOLE_STORIES: usize = 8;
const WHOLE_SEED: u64 = 0x7e57_c0de;

/// Whether the whole-model program fits the model: TinyStories-15M's geometry.
fn whole_eligible(c: &crate::model::ModelConfig) -> bool {
    c.n_embd == DM
        && c.n_ff == host_model::HD
        && c.n_heads == host_model::NH
        && c.n_kv_heads == host_model::NH
        && c.head_dim == host_model::DH
        && (c.rope_theta - 10_000.0).abs() < 1e-3
        && (2..=super::codegen::layer::MAX_POSITIONS).contains(&c.n_ctx)
        && c.n_layers <= super::codegen::layer::LAYERS
}

fn dequant_all(w: &QWeight<CpuBuf>) -> Result<Vec<f32>> {
    let (rows, cols) = (w.shape[0], w.shape[1]);
    let mut out = vec![0f32; rows * cols];
    for (row, chunk) in out.chunks_exact_mut(cols).enumerate() {
        dequant_row(w, row, chunk)?;
    }
    Ok(out)
}

/// The model's weights in float, for calibration and the host's part; `None`
/// for an architecture other than Llama.
pub fn float_weights<B: Backend<Buf = CpuBuf>>(model: &Model<B>) -> Result<Option<FloatWeights>> {
    let (Some(matmuls), Some(norms)) = (model.llama_matmuls(), model.llama_norms()) else {
        return Ok(None);
    };
    let (emb, norm, out) = model.head();
    let layers = matmuls
        .iter()
        .zip(norms)
        .map(|(m, (attn_norm, ffn_norm))| {
            Ok(FloatLayer {
                att_norm: f32s(attn_norm)?.to_vec(),
                wq: dequant_all(m.wq)?,
                wk: dequant_all(m.wk)?,
                wv: dequant_all(m.wv)?,
                wo: dequant_all(m.wo)?,
                ffn_norm: f32s(ffn_norm)?.to_vec(),
                w1: dequant_all(m.w_gate)?,
                w2: dequant_all(m.w_down)?,
                w3: dequant_all(m.w_up)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(FloatWeights {
        emb: dequant_all(emb)?,
        layers,
        norm: f32s(norm)?.to_vec(),
        out: dequant_all(out)?,
        eps: model.config().rms_eps,
        bos: model.bos_token().unwrap_or(1),
    }))
}

/// The whole-model quantization: from the cache, or calibrated on stories the
/// float model samples (and cached).
fn whole_quants(w: &FloatWeights, n_ctx: usize, dir: Option<&std::path::Path>) -> Vec<LayerQuant> {
    let path = dir.map(|dir| dir.join("quants.json"));
    if let Some(quants) = path
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| layer::quants_from_json(&v["quants"]))
        .filter(|q| q.len() == w.layers.len())
    {
        return quants;
    }
    let start = Instant::now();
    let (ranges, tokens) = host_model::calibrate(w, WHOLE_STORIES, n_ctx, WHOLE_SEED);
    let quants = host_model::quants_from(w, &ranges);
    eprintln!(
        "mdma: coral: calibrated the whole model on {tokens} tokens in {:.2?}",
        start.elapsed()
    );
    if let Some(path) = path {
        let doc = serde_json::json!({ "version": 1, "quants": layer::quants_json(&quants) });
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, doc.to_string()));
        if let Err(error) = written {
            eprintln!(
                "mdma: coral: calibration not cached at {}: {error}",
                path.display()
            );
        }
    }
    quants
}

/// What a whole-model decoder shares with the chip.
struct WholeHandle {
    classifier: Arc<host_model::Classifier>,
    quants: Arc<Vec<LayerQuant>>,
    max_positions: usize,
}

impl CoralBackend {
    /// Makes the whole model resident (calibrating it first) and counts one more
    /// decoder on it; `None` when the model or the chip's state does not allow it.
    fn attach_whole(&self, model: &Model<Self>) -> Result<Option<WholeHandle>> {
        if !whole_eligible(model.config()) {
            return Ok(None);
        }
        let (Some(layers), Some(key)) = (model.llama_matmuls(), weights_key(model)) else {
            return Ok(None);
        };
        let _attaching = self.attaching.lock().map_err(backend_error)?;
        let fingerprint = fingerprint(&key, &layers);
        {
            let mut chip = self.chip()?;
            if let Some(whole) = chip.whole.as_mut().filter(|w| w.fingerprint == fingerprint) {
                whole.attached += 1;
                return Ok(Some(WholeHandle {
                    classifier: Arc::clone(&whole.classifier),
                    quants: Arc::clone(&whole.quants),
                    max_positions: whole.max_positions(),
                }));
            }
            if chip.attached > 0 || chip.whole.as_ref().is_some_and(|w| w.attached > 0) {
                return Ok(None);
            }
        }
        let Some(weights) = float_weights(model)? else {
            return Ok(None);
        };
        let n_ctx = model.config().n_ctx;
        let dir = cache_dir(&key);
        let quants = whole_quants(&weights, n_ctx, dir.as_deref());
        // The programs depend on the quantization: key their file by it.
        let programs = dir.map(|dir| {
            use sha2::{Digest, Sha256};
            let key = Sha256::digest(layer::quants_json(&quants).to_string());
            dir.join(format!("programs-{}-{n_ctx}.bin", &hex::encode(key)[..16]))
        });
        let mut chip = self.chip()?;
        Self::release(&mut chip)?;
        let mut whole = Whole::build(
            &mut chip.tpu,
            weights,
            quants,
            n_ctx,
            fingerprint,
            programs.as_deref(),
        )?;
        whole.attached = 1;
        let handle = WholeHandle {
            classifier: Arc::clone(&whole.classifier),
            quants: Arc::clone(&whole.quants),
            max_positions: whole.max_positions(),
        };
        chip.whole = Some(whole);
        Ok(Some(handle))
    }

    fn detach_whole(&self) {
        if let Ok(mut chip) = self.chip.lock()
            && let Some(whole) = chip.whole.as_mut()
        {
            whole.attached = whole.attached.saturating_sub(1);
        }
    }
}

/// Decodes with every layer on the chip: one program call per token. The first
/// position (and any past the generated programs) runs on the CPU; the KV rows
/// the CPU wrote (the prompt) are quantized for the chip on first use, and the
/// chip's rows are written back to the model's KV cache.
struct WholeDecoder<'a> {
    backend: &'a CoralBackend,
    model: &'a Model<CoralBackend>,
    kv: &'a mut KvCache<CoralBackend>,
    ws: &'a mut Workspace<CoralBackend>,
    handle: WholeHandle,
    kv8: Kv8,
}

impl WholeDecoder<'_> {
    /// The uint8 rows `0..pos` from the model's KV cache.
    fn sync(&mut self, pos: usize) -> Result<()> {
        self.kv8.len = self.kv8.len.min(pos);
        for p in self.kv8.len..pos {
            for (l, q) in self.handle.quants.iter().enumerate() {
                let view = self.kv.view(l);
                let (k, v) = (f32s(view.k)?, f32s(view.v)?);
                let (kq, vq) = quantize_kv(&k[p * DM..(p + 1) * DM], &v[p * DM..(p + 1) * DM], q);
                self.kv8.put(l, p, &kq, &vq);
            }
        }
        self.kv8.len = pos;
        Ok(())
    }
}

impl Decoder for WholeDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        let p = pos as usize;
        if p == 0 || p >= self.handle.max_positions {
            return self
                .model
                .forward(self.backend, self.kv, self.ws, token, pos);
        }
        if p >= self.kv.capacity() || !self.kv.can_resume(p) {
            return Err(Error::ShapeMismatch(
                "decode requires a resumable KV prefix within capacity".into(),
            ));
        }
        self.sync(p)?;
        let step = {
            let mut chip = self.backend.chip()?;
            let Chip { tpu, whole, .. } = &mut *chip;
            whole
                .as_mut()
                .ok_or_else(|| backend_error("the whole model left the chip"))?
                .step(tpu, &self.kv8, token, p)?
        };
        for (l, row) in step.kv.iter().enumerate() {
            self.kv8.put(l, p, &row[..DM], &row[DM..]);
            let (k, v) = dequantize_kv(row, &self.handle.quants[l]);
            let (k, v) = (
                CpuBuf(bytemuck::cast_slice(&k).to_vec()),
                CpuBuf(bytemuck::cast_slice(&v).to_vec()),
            );
            self.kv.write(self.backend, l, p, &k, &v, 1)?;
        }
        self.kv8.len = p + 1;
        self.kv.set_len(p + 1);
        Ok(self.handle.classifier.logits(&step.hidden))
    }
}

impl Drop for WholeDecoder<'_> {
    fn drop(&mut self) {
        self.backend.detach_whole();
    }
}

impl Drop for CoralBackend {
    fn drop(&mut self) {
        if let Ok(chip) = self.chip.get_mut() {
            let _ = Self::release(chip);
        }
    }
}

/// The CPU decoder with the backend's chip routing switched on while it lives.
struct ChipDecoder<'a> {
    backend: &'a CoralBackend,
    inner: EagerDecoder<'a, CoralBackend>,
}

impl Decoder for ChipDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.inner.forward(token, pos)
    }
}

impl Drop for ChipDecoder<'_> {
    fn drop(&mut self) {
        self.backend.detach();
    }
}

impl Backend for CoralBackend {
    type Buf = CpuBuf;

    fn device_label(&self) -> Option<String> {
        Some("Coral Edge TPU".into())
    }

    /// Every buffer is host memory the chip reads and writes through its page table.
    fn unified_memory(&self) -> bool {
        true
    }

    fn supports(&self, operation: Operation) -> bool {
        self.cpu.supports(operation)
    }

    fn supports_weight_dtype(&self, dtype: GgmlDType) -> bool {
        self.cpu.supports_weight_dtype(dtype)
    }

    fn decoder<'a>(
        &'a self,
        model: &'a Model<Self>,
        kv: &'a mut KvCache<Self>,
        ws: &'a mut Workspace<Self>,
    ) -> Result<Box<dyn Decoder + 'a>> {
        if self.decode == Decode::Model
            && let Some(handle) = self.attach_whole(model)?
        {
            let n_ctx = model.config().n_ctx;
            return Ok(Box::new(WholeDecoder {
                backend: self,
                model,
                kv,
                ws,
                kv8: Kv8::new(handle.quants.len(), n_ctx),
                handle,
            }));
        }
        let inner = EagerDecoder {
            backend: self,
            model,
            kv,
            ws,
        };
        if self.attach(model)? {
            Ok(Box::new(ChipDecoder {
                backend: self,
                inner,
            }))
        } else {
            Ok(Box::new(inner))
        }
    }

    fn scale(&self, x: &mut CpuBuf, factor: f32) -> Result<()> {
        self.cpu.scale(x, factor)
    }

    fn gelu_tanh_mul(&self, a: &CpuBuf, b: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.gelu_tanh_mul(a, b, y)
    }

    fn softcap(&self, x: &mut CpuBuf, cap: f32) -> Result<()> {
        self.cpu.softcap(x, cap)
    }

    fn rope_split_half(
        &self,
        x: &mut CpuBuf,
        pos: u32,
        spec: &RopeSpec,
        factors: &CpuBuf,
    ) -> Result<()> {
        self.cpu.rope_split_half(x, pos, spec, factors)
    }

    fn rope_partial_interleaved(
        &self,
        x: &mut CpuBuf,
        pos: u32,
        spec: &RopeSpec,
        rotary_dim: usize,
    ) -> Result<()> {
        self.cpu.rope_partial_interleaved(x, pos, spec, rotary_dim)
    }

    fn attn_windowed(
        &self,
        q: &CpuBuf,
        kv: &KvView<CpuBuf>,
        spec: &AttnSpec,
        scale: f32,
        window: usize,
        o: &mut CpuBuf,
    ) -> Result<()> {
        self.cpu.attn_windowed(q, kv, spec, scale, window, o)
    }

    fn alloc(&self, bytes: usize, pool: Pool) -> Result<CpuBuf> {
        self.cpu.alloc(bytes, pool)
    }

    fn upload(&self, dst: &mut CpuBuf, offset: usize, src: &[u8]) -> Result<()> {
        self.cpu.upload(dst, offset, src)
    }

    fn download(&self, src: &CpuBuf, offset: usize, dst: &mut [u8]) -> Result<()> {
        self.cpu.download(src, offset, dst)
    }

    fn copy(
        &self,
        src: &CpuBuf,
        src_off: usize,
        dst: &mut CpuBuf,
        dst_off: usize,
        len: usize,
    ) -> Result<()> {
        self.cpu.copy(src, src_off, dst, dst_off, len)
    }

    fn qmatmul_qkv(
        &self,
        wq: &QWeight<CpuBuf>,
        wk: &QWeight<CpuBuf>,
        wv: &QWeight<CpuBuf>,
        x: &CpuBuf,
        q: &mut CpuBuf,
        k: &mut CpuBuf,
        v: &mut CpuBuf,
    ) -> Result<()> {
        if self.try_chip(&[wq, wk, wv], x, &mut [q, k, v])? {
            return Ok(());
        }
        self.cpu.qmatmul(wq, x, q)?;
        self.cpu.qmatmul(wk, x, k)?;
        self.cpu.qmatmul(wv, x, v)
    }

    fn qmatmul(&self, w: &QWeight<CpuBuf>, x: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        if self.try_chip(&[w], x, &mut [y])? {
            return Ok(());
        }
        if let Some(head) = self.head(w) {
            let x = f32s(x)?;
            if x.len() == head.k() {
                head.matvec(x, f32s_mut(y)?);
                return Ok(());
            }
        }
        self.cpu.qmatmul(w, x, y)
    }

    fn ffn_gate_up(
        &self,
        gate: &QWeight<CpuBuf>,
        up: &QWeight<CpuBuf>,
        x: &CpuBuf,
        gate_scratch: &mut CpuBuf,
        up_scratch: &mut CpuBuf,
        y: &mut CpuBuf,
    ) -> Result<()> {
        if !self.try_chip(&[gate, up], x, &mut [gate_scratch, up_scratch])? {
            self.cpu.qmatmul(gate, x, gate_scratch)?;
            self.cpu.qmatmul(up, x, up_scratch)?;
        }
        self.cpu.silu_mul(gate_scratch, up_scratch, y)
    }

    fn embed(&self, table: &QWeight<CpuBuf>, token_ids: &[u32], out: &mut CpuBuf) -> Result<()> {
        self.cpu.embed(table, token_ids, out)
    }

    fn rmsnorm(&self, w: &CpuBuf, x: &CpuBuf, y: &mut CpuBuf, eps: f32) -> Result<()> {
        self.cpu.rmsnorm(w, x, y, eps)
    }

    fn add(&self, a: &CpuBuf, b: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.add(a, b, y)
    }

    fn rope(
        &self,
        q: &mut CpuBuf,
        k: &mut CpuBuf,
        pos: u32,
        n_tokens: usize,
        spec: &RopeSpec,
    ) -> Result<()> {
        self.cpu.rope(q, k, pos, n_tokens, spec)
    }

    fn attn(&self, q: &CpuBuf, kv: &KvView<CpuBuf>, spec: &AttnSpec, o: &mut CpuBuf) -> Result<()> {
        self.cpu.attn(q, kv, spec, o)
    }

    fn silu_mul(&self, gate: &CpuBuf, up: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.silu_mul(gate, up, y)
    }

    fn layernorm(
        &self,
        w: &CpuBuf,
        b: Option<&CpuBuf>,
        x: &CpuBuf,
        y: &mut CpuBuf,
        eps: f32,
    ) -> Result<()> {
        self.cpu.layernorm(w, b, x, y, eps)
    }

    fn add_bias(&self, b: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.add_bias(b, y)
    }

    fn gelu(&self, x: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.gelu(x, y)
    }

    fn gelu_mul(&self, a: &CpuBuf, gate: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.gelu_mul(a, gate, y)
    }

    fn relu(&self, x: &CpuBuf, y: &mut CpuBuf) -> Result<()> {
        self.cpu.relu(x, y)
    }

    fn rope_neox(
        &self,
        q: &mut CpuBuf,
        k: &mut CpuBuf,
        spec: &EncoderSpec,
        theta: f32,
    ) -> Result<()> {
        self.cpu.rope_neox(q, k, spec, theta)
    }

    fn attn_bidir(
        &self,
        q: &CpuBuf,
        k: &CpuBuf,
        v: &CpuBuf,
        lens: &[u32],
        spec: &EncoderSpec,
        o: &mut CpuBuf,
    ) -> Result<()> {
        self.cpu.attn_bidir(q, k, v, lens, spec, o)
    }
}
