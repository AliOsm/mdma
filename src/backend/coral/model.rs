//! The host side of the whole-model program (`codegen::layer`): the float model the
//! quantization is calibrated on, the per-tensor quantization of every activation
//! and weight, the uint8 weights with the RMSNorm weights folded in, and the
//! per-position RoPE inputs. TinyStories-15M geometry: dim 288, FFN 768, 6 heads
//! of 48, llama2.c's interleaved RoPE with base 10000.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    reason = "token ids, positions and uint8 codes are small integers"
)]

use super::codegen::fc::param_blob;
use super::codegen::layer::{LayerQuant, MATS, Mat};
use super::isa::eltwise::Q;
use rayon::prelude::*;
use std::collections::HashMap;

pub const DM: usize = 288;
pub const HD: usize = 768;
pub const NH: usize = 6;
pub const DH: usize = 48;
/// TFLite's L2_NORMALIZATION output quantization.
pub const L2_OUT: Q = (1.0 / 128.0, 128);

/// The q and k rows' order on the chip: per group of 16 outputs the first elements
/// of 8 consecutive RoPE pairs, then their second elements, so swap(q) is the half
/// swap of every 16-byte group.
pub fn sigma() -> [usize; DM] {
    std::array::from_fn(|p| 2 * (8 * (p / 16) + p % 8) + usize::from(p % 16 >= 8))
}

/// One layer's float weights, row-major `[out, in]`.
#[derive(Clone, Debug)]
pub struct FloatLayer {
    pub att_norm: Vec<f32>,
    pub wq: Vec<f32>,
    pub wk: Vec<f32>,
    pub wv: Vec<f32>,
    pub wo: Vec<f32>,
    pub ffn_norm: Vec<f32>,
    pub w1: Vec<f32>,
    pub w2: Vec<f32>,
    pub w3: Vec<f32>,
}

/// The model's float weights.
#[derive(Clone, Debug)]
pub struct FloatWeights {
    pub emb: Vec<f32>,
    pub layers: Vec<FloatLayer>,
    pub norm: Vec<f32>,
    pub out: Vec<f32>,
    pub eps: f32,
    pub bos: u32,
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(a, b)| a * b).sum()
}

/// `y = W x` for a row-major `[n, k]` W.
fn matvec(w: &[f32], x: &[f32], y: &mut [f32]) {
    for (row, out) in w.chunks_exact(x.len()).zip(y.iter_mut()) {
        *out = dot(row, x);
    }
}

fn norm2(x: &[f32]) -> f32 {
    x.iter().map(|v| v * v).sum::<f32>().sqrt()
}

/// Every element's RoPE angle at `pos` (pairs share theirs).
pub fn rope_angles(pos: usize) -> [f64; DM] {
    std::array::from_fn(|p| {
        let i = (p % DH) / 2 * 2;
        pos as f64 * (1.0 / 10000f64.powf(i as f64 / DH as f64))
    })
}

fn rope(x: &[f32], ang: &[f64; DM]) -> Vec<f32> {
    let mut o = vec![0f32; DM];
    for j in 0..DM / 2 {
        let (c, s) = (ang[2 * j].cos() as f32, ang[2 * j].sin() as f32);
        let (a, b) = (x[2 * j], x[2 * j + 1]);
        o[2 * j] = a * c - b * s;
        o[2 * j + 1] = a * s + b * c;
    }
    o
}

/// The recorded tensors of a layer.
const KEYS: [&str; 22] = [
    "res", "x", "qf", "kf", "v", "qc", "kc", "q", "k", "qk", "s", "pv", "att", "o", "mid", "hn",
    "h1", "h3", "a", "m", "y", "out",
];

fn key(name: &str) -> usize {
    KEYS.iter()
        .position(|k| *k == name)
        .unwrap_or_else(|| panic!("no recorded tensor {name}"))
}

/// `[min, max]` of every recorded tensor of every layer.
#[derive(Clone, Debug)]
pub struct Ranges(Vec<[(f32, f32); KEYS.len()]>);

impl Ranges {
    pub fn new(layers: usize) -> Self {
        Self(vec![
            [(f32::INFINITY, f32::NEG_INFINITY); KEYS.len()];
            layers
        ])
    }

    fn record(&mut self, l: usize, name: &str, xs: impl IntoIterator<Item = f32>) {
        let r = &mut self.0[l][key(name)];
        for x in xs {
            r.0 = r.0.min(x);
            r.1 = r.1.max(x);
        }
    }

    fn merge(mut self, other: &Self) -> Self {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            for (x, y) in a.iter_mut().zip(b) {
                *x = (x.0.min(y.0), x.1.max(y.1));
            }
        }
        self
    }

    pub fn get(&self, l: usize, name: &str) -> (f32, f32) {
        self.0[l][key(name)]
    }
}

/// The weights as the chip computes with them, in f32: the RMSNorm weight and
/// sqrt(288) folded into the columns of q, k, v, w1 and w3.
struct Folded {
    wq: Vec<f32>,
    wk: Vec<f32>,
    wv: Vec<f32>,
    w1: Vec<f32>,
    w3: Vec<f32>,
}

fn fold(w: &[f32], g: &[f32]) -> Vec<f32> {
    let k = g.len();
    let scale = (DM as f64).sqrt();
    w.chunks_exact(k)
        .flat_map(|row| {
            row.iter()
                .zip(g)
                .map(move |(a, b)| (f64::from(*a) * (f64::from(*b) * scale)) as f32)
        })
        .collect()
}

/// The float model in the chip's form (folded norm weights, L2 normalizations,
/// no epsilon inside the layers), recording every quantization point.
struct FloatModel<'a> {
    w: &'a FloatWeights,
    folded: &'a [Folded],
    classifier: &'a Classifier,
    k: Vec<Vec<f32>>,
    v: Vec<Vec<f32>>,
}

impl<'a> FloatModel<'a> {
    fn new(w: &'a FloatWeights, folded: &'a [Folded], classifier: &'a Classifier) -> Self {
        Self {
            w,
            folded,
            classifier,
            k: vec![Vec::new(); w.layers.len()],
            v: vec![Vec::new(); w.layers.len()],
        }
    }

    fn layer(&mut self, l: usize, x: &[f32], pos: usize, rec: &mut Ranges, all: bool) -> Vec<f32> {
        let (lw, f) = (&self.w.layers[l], &self.folded[l]);
        // Every tensor is recorded after the BOS position; k and v (the KV rows the
        // later positions read) at every position.
        let mut r = |name: &str, xs: &[f32]| {
            if all || name == "k" || name == "v" {
                rec.record(l, name, xs.iter().copied());
            }
        };
        r("res", x);
        let n = norm2(x);
        let xn: Vec<f32> = x.iter().map(|v| v / n).collect();
        r("x", &xn);
        let (mut q0, mut k0, mut v) = (vec![0f32; DM], vec![0f32; DM], vec![0f32; DM]);
        matvec(&f.wq, &xn, &mut q0);
        matvec(&f.wk, &xn, &mut k0);
        matvec(&f.wv, &xn, &mut v);
        r("qf", &q0);
        r("kf", &k0);
        r("v", &v);
        let ang = rope_angles(pos);
        let cs: Vec<f32> = ang.iter().map(|a| a.cos() as f32).collect();
        let sn: Vec<f32> = ang
            .iter()
            .enumerate()
            .map(|(i, a)| if i % 2 == 0 { -a.sin() } else { a.sin() } as f32)
            .collect();
        for (src, name) in [(&q0, "qc"), (&k0, "kc")] {
            let prod: Vec<f32> = src.iter().zip(&cs).map(|(a, b)| a * b).collect();
            r(name, &prod);
            let swapped: Vec<f32> = (0..DM).map(|i| src[i ^ 1] * sn[i]).collect();
            r(name, &swapped);
        }
        let q = rope(&q0, &ang);
        let k = rope(&k0, &ang);
        r("q", &q);
        r("k", &k);
        self.k[l].extend_from_slice(&k);
        self.v[l].extend_from_slice(&v);
        let positions = pos + 1;
        let (keys, values) = (&self.k[l], &self.v[l]);
        let mut att = vec![0f32; DM];
        let beta = (DH as f32).powf(-0.5);
        let mut qk = Vec::with_capacity(positions * DM);
        let mut scores = Vec::with_capacity(NH * positions);
        let mut pv = Vec::with_capacity(positions * DM);
        for h in 0..NH {
            let qh = &q[h * DH..(h + 1) * DH];
            let s: Vec<f32> = (0..positions)
                .map(|p| {
                    let kh = &keys[p * DM + h * DH..p * DM + (h + 1) * DH];
                    qk.extend(qh.iter().zip(kh).map(|(a, b)| a * b));
                    dot(qh, kh)
                })
                .collect();
            scores.extend_from_slice(&s);
            let max = s.iter().map(|x| x * beta).fold(f32::NEG_INFINITY, f32::max);
            let e: Vec<f32> = s.iter().map(|x| (x * beta - max).exp()).collect();
            let total: f32 = e.iter().sum();
            for (p, ep) in e.iter().enumerate() {
                let prob = ep / total;
                let vh = &values[p * DM + h * DH..p * DM + (h + 1) * DH];
                pv.extend(vh.iter().map(|x| prob * x));
                for (o, x) in att[h * DH..(h + 1) * DH].iter_mut().zip(vh) {
                    *o += prob * x;
                }
            }
        }
        r("qk", &qk);
        r("s", &scores);
        r("pv", &pv);
        r("att", &att);
        let mut o = vec![0f32; DM];
        matvec(&lw.wo, &att, &mut o);
        r("o", &o);
        let h: Vec<f32> = x.iter().zip(&o).map(|(a, b)| a + b).collect();
        r("mid", &h);
        let n = norm2(&h);
        let hn: Vec<f32> = h.iter().map(|v| v / n).collect();
        r("hn", &hn);
        let (mut h1, mut h3) = (vec![0f32; HD], vec![0f32; HD]);
        matvec(&f.w1, &hn, &mut h1);
        matvec(&f.w3, &hn, &mut h3);
        r("h1", &h1);
        r("h3", &h3);
        let a: Vec<f32> = h1.iter().map(|v| v / (1.0 + (-v).exp())).collect();
        r("a", &a);
        let m: Vec<f32> = a.iter().zip(&h3).map(|(a, b)| a * b).collect();
        r("m", &m);
        let mut y = vec![0f32; DM];
        matvec(&lw.w2, &m, &mut y);
        r("y", &y);
        let out: Vec<f32> = h.iter().zip(&y).map(|(a, b)| a + b).collect();
        r("out", &out);
        out
    }

    fn step(&mut self, token: u32, pos: usize, rec: &mut Ranges) -> Vec<f32> {
        let t = token as usize;
        let mut x = self.w.emb[t * DM..(t + 1) * DM].to_vec();
        for l in 0..self.w.layers.len() {
            x = self.layer(l, &x, pos, rec, pos > 0);
        }
        self.classifier.logits(&x)
    }
}

/// A `[n, k]` matrix in int8 with one scale per row, multiplied on the host: the
/// activation goes to i16 (one scale for the vector) and every row is an i8 x i16
/// dot product accumulated in i32, which the baseline x86-64 target vectorizes as
/// a multiply-add of 16-bit pairs. A quarter of the f32 matrix's memory traffic.
pub struct Int8Rows {
    k: usize,
    weights: Vec<i8>,
    scales: Vec<f32>,
}

/// The symmetric scale mapping `xs`' largest magnitude to `top`.
fn absmax_scale(xs: &[f32], top: f32) -> f32 {
    let max = xs.iter().fold(0f32, |m, v| m.max(v.abs()));
    if max > 0.0 { max / top } else { 1.0 }
}

impl Int8Rows {
    pub fn new(rows: &[f32], k: usize) -> Self {
        let mut weights = Vec::with_capacity(rows.len());
        let scales = rows
            .chunks_exact(k)
            .map(|row| {
                let scale = absmax_scale(row, 127.0);
                weights.extend(
                    row.iter()
                        .map(|v| (v / scale).round().clamp(-127.0, 127.0) as i8),
                );
                scale
            })
            .collect();
        Self { k, weights, scales }
    }

    pub const fn k(&self) -> usize {
        self.k
    }

    pub fn rows(&self) -> usize {
        self.scales.len()
    }

    /// `out[r] = row r . x`.
    pub fn matvec(&self, x: &[f32], out: &mut [f32]) {
        let k = self.k;
        let x_scale = absmax_scale(x, 32767.0);
        let xq: Vec<i16> = x
            .iter()
            .map(|v| (v / x_scale).round().clamp(-32767.0, 32767.0) as i16)
            .collect();
        out[..self.rows()]
            .par_chunks_mut(1024)
            .enumerate()
            .for_each(|(c, chunk)| {
                for (i, o) in chunk.iter_mut().enumerate() {
                    let row = c * 1024 + i;
                    let w = &self.weights[row * k..(row + 1) * k];
                    let dot: i32 = w
                        .iter()
                        .zip(&xq)
                        .map(|(a, b)| i32::from(*a) * i32::from(*b))
                        .sum();
                    *o = dot as f32 * (self.scales[row] * x_scale);
                }
            });
    }
}

/// The classifier as the host runs it during decode: the final RMSNorm, then the
/// classifier weights as `Int8Rows`.
pub struct Classifier {
    norm: Vec<f32>,
    eps: f32,
    rows: Int8Rows,
}

impl Classifier {
    pub fn new(w: &FloatWeights) -> Self {
        Self {
            norm: w.norm.clone(),
            eps: w.eps,
            rows: Int8Rows::new(&w.out, DM),
        }
    }

    pub fn logits(&self, x: &[f32]) -> Vec<f32> {
        let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        let inv = 1.0 / (ms + self.eps).sqrt();
        let xn: Vec<f32> = x.iter().zip(&self.norm).map(|(a, g)| a * inv * g).collect();
        let mut out = vec![0f32; self.rows.rows()];
        self.rows.matvec(&xn, &mut out);
        out
    }
}

/// xorshift64* in [0, 1).
fn uniform(state: &mut u64) -> f64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
}

/// Samples at `temperature` with the Gumbel-max trick.
pub fn sample(logits: &[f32], temperature: f32, state: &mut u64) -> u32 {
    logits
        .iter()
        .map(|&l| f64::from(l / temperature) - (-uniform(state).max(1e-300).ln()).ln())
        .enumerate()
        .fold((0, f64::NEG_INFINITY), |best, (i, v)| {
            if v > best.1 { (i, v) } else { best }
        })
        .0 as u32
}

/// The ranges of `stories` stories the float model samples from BOS (temperature
/// 0.8, each ending before the next BOS or at `max_tokens`), every tensor recorded
/// after the BOS position. Stories run in parallel. Returns the ranges and the
/// tokens run.
pub fn calibrate(
    w: &FloatWeights,
    stories: usize,
    max_tokens: usize,
    seed: u64,
) -> (Ranges, usize) {
    let folded: Vec<Folded> = w
        .layers
        .iter()
        .map(|l| Folded {
            wq: fold(&l.wq, &l.att_norm),
            wk: fold(&l.wk, &l.att_norm),
            wv: fold(&l.wv, &l.att_norm),
            w1: fold(&l.w1, &l.ffn_norm),
            w3: fold(&l.w3, &l.ffn_norm),
        })
        .collect();
    let classifier = Classifier::new(w);
    let runs: Vec<(Ranges, usize)> = (0..stories)
        .into_par_iter()
        .map(|i| {
            let mut rec = Ranges::new(w.layers.len());
            let mut state = seed ^ (0x9e37_79b9_7f4a_7c15u64.wrapping_mul(i as u64 + 1));
            let mut model = FloatModel::new(w, &folded, &classifier);
            let mut token = w.bos;
            let mut n = 0;
            for pos in 0..max_tokens {
                let logits = model.step(token, pos, &mut rec);
                n += 1;
                token = sample(&logits, 0.8, &mut state);
                if token == w.bos {
                    break;
                }
            }
            (rec, n)
        })
        .collect();
    let mut ranges = Ranges::new(w.layers.len());
    let mut tokens = 0;
    for (r, n) in &runs {
        ranges = ranges.merge(r);
        tokens += n;
    }
    (ranges, tokens)
}

/// Asymmetric uint8 quantization covering `[lo, hi]` (always including 0).
pub fn quantize_params(lo: f64, hi: f64) -> Q {
    let (lo, hi) = (lo.min(0.0), hi.max(0.0));
    let scale = if hi > lo { (hi - lo) / 255.0 } else { 1.0 };
    (
        scale,
        (-lo / scale).round_ties_even().clamp(0.0, 255.0) as i64,
    )
}

/// The folded weights of layer `l` in f64, as the oracle quantizes them: wqkv
/// (q | k | v rows), wo, w1, w3, w2.
fn folded_f64(l: &FloatLayer) -> HashMap<&'static str, (Vec<f64>, usize)> {
    let scale = (DM as f64).sqrt();
    let fold = |w: &[f32], g: &[f32]| -> Vec<f64> {
        w.chunks_exact(g.len())
            .flat_map(|row| {
                row.iter()
                    .zip(g)
                    .map(move |(a, b)| f64::from(*a) * (f64::from(*b) * scale))
            })
            .collect()
    };
    let plain = |w: &[f32]| -> Vec<f64> { w.iter().map(|v| f64::from(*v)).collect() };
    let mut wqkv = fold(&l.wq, &l.att_norm);
    wqkv.extend(fold(&l.wk, &l.att_norm));
    wqkv.extend(fold(&l.wv, &l.att_norm));
    HashMap::from([
        ("wqkv", (wqkv, DM)),
        ("wo", (plain(&l.wo), DM)),
        ("w1", (fold(&l.w1, &l.ffn_norm), DM)),
        ("w3", (fold(&l.w3, &l.ffn_norm), DM)),
        ("w2", (plain(&l.w2), HD)),
    ])
}

fn min_max(xs: &[f64]) -> (f64, f64) {
    xs.iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| {
            (a.min(x), b.max(x))
        })
}

/// Every layer's quantization from the recorded ranges and the weights.
pub fn quants_from(w: &FloatWeights, ranges: &Ranges) -> Vec<LayerQuant> {
    let mut out: Vec<LayerQuant> = w
        .layers
        .iter()
        .enumerate()
        .map(|(l, lw)| {
            let mut q = HashMap::new();
            for name in [
                "res", "qf", "kf", "v", "qc", "kc", "q", "k", "qk", "s", "pv", "att", "o", "mid",
                "h1", "h3", "a", "m", "y", "out",
            ] {
                let (lo, hi) = ranges.get(l, name);
                q.insert(
                    name.to_owned(),
                    quantize_params(f64::from(lo), f64::from(hi)),
                );
            }
            for (name, (values, _)) in folded_f64(lw) {
                let (lo, hi) = min_max(&values);
                q.insert(name.to_owned(), quantize_params(lo, hi));
            }
            q.insert("x".into(), L2_OUT);
            q.insert("hn".into(), L2_OUT);
            q.insert("cos".into(), (1.0 / 127.0, 128));
            q.insert("sin".into(), (1.0 / 127.0, 128));
            q.insert("p".into(), (1.0 / 256.0, 0));
            q.insert("g".into(), (1.0 / 256.0, 0));
            LayerQuant {
                q,
                beta: (DH as f64).powf(-0.5),
            }
        })
        .collect();
    // One quantization of the residual between two layers.
    for l in 1..out.len() {
        let res = out[l - 1].get("out");
        out[l].q.insert("res".into(), res);
    }
    out
}

/// `clip(rint(x / s) + z, 0, 255)`.
pub fn quant(x: f64, q: Q) -> u8 {
    ((x / q.0).round_ties_even() + q.1 as f64).clamp(0.0, 255.0) as u8
}

/// `(b - z) s`.
pub fn dequant(b: u8, q: Q) -> f32 {
    ((f64::from(b) - q.1 as f64) * q.0) as f32
}

/// The parameter blob of the whole-model program: per layer the conv blob (a zero
/// bias row per 64 outputs, then `[K/4][64][4]` weights) of q, k, v, wo, w1, w3,
/// w2, the folded weights quantized per tensor, q and k rows in the order SIGMA.
pub fn model_params(w: &FloatWeights, quants: &[LayerQuant]) -> Vec<u8> {
    let sig = sigma();
    let mut blob = Vec::new();
    for (lw, q) in w.layers.iter().zip(quants) {
        let f = folded_f64(lw);
        for m in MATS {
            let (_, wkey, _) = m.keys();
            let wq = q.get(wkey);
            let (values, k) = &f[wkey];
            let rows: Vec<usize> = match m {
                Mat::Q => sig.to_vec(),
                Mat::K => sig.iter().map(|r| DM + r).collect(),
                Mat::V => (2 * DM..3 * DM).collect(),
                _ => (0..values.len() / k).collect(),
            };
            let data: Vec<u8> = rows
                .iter()
                .flat_map(|r| values[r * k..(r + 1) * k].iter().map(|x| quant(*x, wq)))
                .collect();
            blob.extend(param_blob(&data, rows.len(), *k, wq.1 as u8, None, 64));
        }
    }
    blob
}

/// (cos, sin') of position `pos` in the order SIGMA: sin' carries the rotation's
/// sign (`-sin` at a pair's first element), so `q' = q cos + swap(q) sin'`.
pub fn rope_inputs(pos: usize, q: &LayerQuant) -> ([u8; DM], [u8; DM]) {
    let ang = rope_angles(pos);
    let (cq, sq) = (q.get("cos"), q.get("sin"));
    let sig = sigma();
    let cos: [u8; DM] = std::array::from_fn(|p| quant(ang[sig[p]].cos(), cq));
    let sin: [u8; DM] = std::array::from_fn(|p| {
        let i = sig[p];
        let s = if i.is_multiple_of(2) {
            -ang[i].sin()
        } else {
            ang[i].sin()
        };
        quant(s, sq)
    });
    (cos, sin)
}

/// The quantized embedding of `token` (the first layer's residual quantization).
pub fn embed(w: &FloatWeights, token: u32, q: &LayerQuant) -> [u8; DM] {
    let res = q.get("res");
    let t = token as usize;
    std::array::from_fn(|i| quant(f64::from(w.emb[t * DM + i]), res))
}
