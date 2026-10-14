//! The whole model on the chip: one program call per token runs every layer
//! (`codegen::layer`), the host keeps the embedding lookup, the final RMSNorm and
//! the classifier, and the uint8 KV cache the programs read. The weights are cached
//! in the tiles' wide memory once; one execution program per position count P is
//! generated and loaded up front.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "positions and uint8 codes are small integers"
)]

use super::codegen::layer::{self, LayerQuant, gen_model, plan};
use super::executable::{Bitstream, Description, Executable, FieldOffset};
use super::isa::eltwise::Q;
use super::model::{self, DH, DM, FloatWeights, NH, sigma};
use super::tpu::{Loaded, Parameters, Tpu};
use super::{Error, Result};
use rayon::prelude::*;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// The uint8 KV rows the programs read, per layer head-major `[6][n_ctx][48]`;
/// k' in the order SIGMA.
pub struct Kv8 {
    k: Vec<Vec<u8>>,
    v: Vec<Vec<u8>>,
    n_ctx: usize,
    /// Positions held.
    pub len: usize,
}

impl Kv8 {
    pub fn new(layers: usize, n_ctx: usize) -> Self {
        Self {
            k: vec![vec![0; NH * n_ctx * DH]; layers],
            v: vec![vec![0; NH * n_ctx * DH]; layers],
            n_ctx,
            len: 0,
        }
    }

    /// Stores row `pos` of layer `l` (288 bytes each, heads contiguous).
    pub fn put(&mut self, l: usize, pos: usize, k: &[u8], v: &[u8]) {
        for h in 0..NH {
            let at = (h * self.n_ctx + pos) * DH;
            self.k[l][at..at + DH].copy_from_slice(&k[h * DH..(h + 1) * DH]);
            self.v[l][at..at + DH].copy_from_slice(&v[h * DH..(h + 1) * DH]);
        }
    }

    /// The program input of `p` positions: rows `0..p-1`, row `p-1` zero.
    fn input(&self, rows: &[u8], p: usize, out: &mut [u8]) {
        for h in 0..NH {
            let src = &rows[h * self.n_ctx * DH..(h * self.n_ctx + p - 1) * DH];
            let dst = &mut out[h * p * DH..(h + 1) * p * DH];
            dst[..src.len()].copy_from_slice(src);
            dst[src.len()..].fill(0);
        }
    }
}

/// The resident whole-model state.
pub struct Whole {
    pub fingerprint: u64,
    /// The embedding table (the layers are on the chip, the classifier in `classifier`).
    pub host: Arc<FloatWeights>,
    pub classifier: Arc<model::Classifier>,
    pub quants: Arc<Vec<LayerQuant>>,
    parameters: Option<Parameters>,
    /// Execution program of P positions at index P (from 2).
    programs: Vec<Option<Loaded>>,
    /// Live decoders.
    pub attached: usize,
}

/// One token's results.
pub struct Step {
    /// The last layer's output, dequantized.
    pub hidden: Vec<f32>,
    /// Per layer k' (order SIGMA) | v, uint8.
    pub kv: Vec<[u8; 2 * DM]>,
}

impl Whole {
    /// Caches the weights and loads the programs of P = 2..=n_ctx, generated or
    /// read from `cache` (a file the generated programs are written to).
    pub fn build(
        tpu: &mut Tpu,
        mut host: FloatWeights,
        quants: Vec<LayerQuant>,
        n_ctx: usize,
        fingerprint: u64,
        cache: Option<&Path>,
    ) -> Result<Self> {
        let pl = plan(quants.len())?;
        let blob = model::model_params(&host, &quants);
        let classifier = Arc::new(model::Classifier::new(&host));
        host.layers.clear();
        host.out = Vec::new();
        let start = Instant::now();
        let positions: Vec<usize> = (2..=n_ctx).collect();
        let cached = cache.and_then(|path| read_programs(path, &positions, quants.len()));
        let from_cache = cached.is_some();
        let executions = if let Some(executions) = cached {
            executions
        } else {
            let executions: Vec<Executable> = positions
                .par_iter()
                .map(|&p| gen_model(p, &quants, &pl).map(|programs| programs.execution))
                .collect::<Result<_>>()?;
            if let Some(path) = cache
                && let Err(error) = write_programs(path, &positions, &executions)
            {
                eprintln!(
                    "mdma: coral: programs not cached at {}: {error}",
                    path.display()
                );
            }
            executions
        };
        let generation = start.elapsed();
        let mut whole = Self {
            fingerprint,
            host: Arc::new(host),
            classifier,
            quants: Arc::new(quants),
            parameters: None,
            programs: (0..=n_ctx).map(|_| None).collect(),
            attached: 0,
        };
        let loaded = (|| -> Result<()> {
            whole.parameters =
                Some(tpu.cache_parameters(&layer::model_caching_executable(&pl), &blob)?);
            for (p, execution) in positions.iter().zip(&executions) {
                whole.programs[*p] = Some(tpu.load(execution, whole.parameters.as_ref())?);
            }
            Ok(())
        })();
        if let Err(error) = loaded {
            whole.release(tpu)?;
            return Err(error);
        }
        eprintln!(
            "mdma: coral: the whole model on the chip: {} programs {} in {generation:.2?}, \
             {:.1} MB of parameters, loaded in {:.2?}",
            positions.len(),
            if from_cache { "read" } else { "generated" },
            blob.len() as f64 / 1e6,
            start.elapsed().saturating_sub(generation)
        );
        Ok(whole)
    }

    pub fn release(&mut self, tpu: &mut Tpu) -> Result<()> {
        let mut first = None;
        for program in self.programs.iter_mut().filter_map(Option::take) {
            if let Err(error) = tpu.unload(program) {
                first.get_or_insert(error);
            }
        }
        if let Some(parameters) = self.parameters.take()
            && let Err(error) = tpu.unload_parameters(parameters)
        {
            first.get_or_insert(error);
        }
        first.map_or(Ok(()), Err)
    }

    pub fn max_positions(&self) -> usize {
        self.programs.len() - 1
    }

    /// Runs `token` at `pos` (>= 1) over the KV rows `0..pos`.
    pub fn step(&mut self, tpu: &mut Tpu, kv: &Kv8, token: u32, pos: usize) -> Result<Step> {
        let q0 = &self.quants[0];
        let x = model::embed(&self.host, token, q0);
        let (cos, sin) = model::rope_inputs(pos, q0);
        self.run(tpu, kv, &x, &cos, &sin, pos + 1)
    }

    /// Runs the program of `p` positions on the quantized embedding `x`, the RoPE
    /// inputs and the KV rows `0..p-1`.
    pub fn run(
        &mut self,
        tpu: &mut Tpu,
        kv: &Kv8,
        x: &[u8],
        cos: &[u8],
        sin: &[u8],
        p: usize,
    ) -> Result<Step> {
        let layers = self.quants.len();
        let program = self
            .programs
            .get_mut(p)
            .and_then(Option::as_mut)
            .ok_or_else(|| Error::Protocol(format!("no whole-model program for {p} positions")))?;
        program.input_mut("x")?.copy_from_slice(x);
        program.input_mut("cos")?.copy_from_slice(cos);
        program.input_mut("sin")?.copy_from_slice(sin);
        for l in 0..layers {
            for (name, rows) in [("k", &kv.k[l]), ("v", &kv.v[l])] {
                kv.input(rows, p, program.input_mut(&format!("{name}{l}"))?);
            }
        }
        tpu.invoke(program)?;
        let out_q: Q = self.quants[layers - 1].get("out");
        let hidden = program
            .output("out")?
            .iter()
            .map(|&b| model::dequant(b, out_q))
            .collect();
        let kv = (0..layers)
            .map(|l| {
                let mut row = [0u8; 2 * DM];
                row.copy_from_slice(program.output(&format!("kv{l}"))?);
                Ok(row)
            })
            .collect::<Result<_>>()?;
        Ok(Step { hidden, kv })
    }
}

/// A float KV row as the chip holds it: k' (natural order, rotated) quantized and
/// put in the order SIGMA, v quantized.
pub fn quantize_kv(k: &[f32], v: &[f32], q: &LayerQuant) -> ([u8; DM], [u8; DM]) {
    let (kq, vq) = (q.get("k"), q.get("v"));
    let sig = sigma();
    (
        std::array::from_fn(|p| model::quant(f64::from(k[sig[p]]), kq)),
        std::array::from_fn(|p| model::quant(f64::from(v[p]), vq)),
    )
}

/// The chip's k' | v row back in float, k in the natural order.
pub fn dequantize_kv(row: &[u8; 2 * DM], q: &LayerQuant) -> (Vec<f32>, Vec<f32>) {
    let (kq, vq) = (q.get("k"), q.get("v"));
    let sig = sigma();
    let mut k = vec![0f32; DM];
    for (p, b) in row[..DM].iter().enumerate() {
        k[sig[p]] = model::dequant(*b, kq);
    }
    (
        k,
        row[DM..].iter().map(|b| model::dequant(*b, vq)).collect(),
    )
}

/// The program cache format's first bytes; a change of the generator changes it.
const PROGRAMS_MAGIC: &[u8; 16] = b"mdma-coral-prg01";

/// The execution programs of `positions` (the caching program is regenerated: it
/// depends on the plan only).
fn write_programs(path: &Path, positions: &[usize], executions: &[Executable]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut w = io::BufWriter::new(std::fs::File::create(&tmp)?);
    let u32 = |w: &mut io::BufWriter<std::fs::File>, x: usize| {
        w.write_all(&u32::try_from(x).map_err(io::Error::other)?.to_le_bytes())
    };
    w.write_all(PROGRAMS_MAGIC)?;
    u32(&mut w, positions.len())?;
    for (p, exe) in positions.iter().zip(executions) {
        u32(&mut w, *p)?;
        u32(&mut w, exe.bitstreams.len())?;
        for b in &exe.bitstreams {
            u32(&mut w, b.data.len())?;
            w.write_all(&b.data)?;
            u32(&mut w, b.relocs.len())?;
            for r in &b.relocs {
                let desc = match r.desc {
                    Description::InputActivation => 0u8,
                    Description::OutputActivation => 1,
                    Description::Parameter => 2,
                    Description::Scratch => 3,
                };
                let name = r.name.as_deref().unwrap_or("");
                w.write_all(&[desc, u8::from(r.upper)])?;
                u32(&mut w, r.offset_bit)?;
                u32(&mut w, name.len())?;
                w.write_all(name.as_bytes())?;
            }
        }
    }
    w.into_inner().map_err(io::Error::other)?.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Reads the cache format's fields.
struct Reader(io::BufReader<std::fs::File>);

impl Reader {
    fn bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut b = vec![0u8; n];
        self.0.read_exact(&mut b).ok()?;
        Some(b)
    }

    fn u32(&mut self) -> Option<usize> {
        let b = self.bytes(4)?;
        Some(u32::from_le_bytes(b.try_into().ok()?) as usize)
    }

    /// A length-prefixed byte string.
    fn sized(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()?;
        self.bytes(n)
    }
}

/// The cached execution programs of `positions`, or `None` when the file is
/// missing, from another generator or for other positions.
fn read_programs(path: &Path, positions: &[usize], layers: usize) -> Option<Vec<Executable>> {
    let mut r = Reader(io::BufReader::new(std::fs::File::open(path).ok()?));
    if r.bytes(16)? != PROGRAMS_MAGIC || r.u32()? != positions.len() {
        return None;
    }
    positions
        .iter()
        .map(|&p| {
            if r.u32()? != p {
                return None;
            }
            let bitstreams = (0..r.u32()?)
                .map(|_| {
                    let data = r.sized()?;
                    let relocs = (0..r.u32()?)
                        .map(|_| {
                            let head = r.bytes(2)?;
                            let offset_bit = r.u32()?;
                            let name = String::from_utf8(r.sized()?).ok()?;
                            let desc = match head[0] {
                                0 => Description::InputActivation,
                                1 => Description::OutputActivation,
                                2 => Description::Parameter,
                                _ => Description::Scratch,
                            };
                            Some(FieldOffset {
                                desc,
                                batch: 0,
                                name: (!name.is_empty()).then_some(name),
                                upper: head[1] != 0,
                                offset_bit,
                            })
                        })
                        .collect::<Option<_>>()?;
                    Some(Bitstream { data, relocs })
                })
                .collect::<Option<_>>()?;
            let execution = layer::model_execution(p, layers, bitstreams);
            execution.validate().ok()?;
            Some(execution)
        })
        .collect()
}
