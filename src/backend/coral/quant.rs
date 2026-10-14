//! Host-side numerics of the chip's uint8 domain: affine quantization of
//! activations and weights, and stacking several weight matrices into one
//! program so each compute tile holds rows of one matrix only (a tile op carries
//! its own zero points and multiplier, so every tile gets its own scales).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "values are clamped to the uint8 domain before the casts"
)]

use super::codegen::fc::FcProgram;
use super::isa::round_up;

/// `real = scale * (q - zp)`; the range always includes zero, so `zp` is exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub scale: f32,
    pub zp: u8,
}

impl Affine {
    /// Spans `[min, max]` widened to include zero; a degenerate range gets a
    /// tiny positive scale so the quantization stays defined.
    pub fn from_range(min: f32, max: f32) -> Self {
        let (min, max) = (min.min(0.0), max.max(0.0));
        let scale = ((max - min) / 255.0).max(f32::MIN_POSITIVE * 256.0);
        let zp = (-min / scale).round().clamp(0.0, 255.0) as u8;
        Self { scale, zp }
    }

    /// The range of `xs`.
    pub fn covering(xs: &[f32]) -> Self {
        let (min, max) = range(xs);
        Self::from_range(min, max)
    }

    pub fn quantize(&self, x: f32) -> u8 {
        (x / self.scale + f32::from(self.zp))
            .round_ties_even()
            .clamp(0.0, 255.0) as u8
    }

    pub fn dequantize(&self, q: u8) -> f32 {
        self.scale * (f32::from(q) - f32::from(self.zp))
    }
}

/// `(min, max)` of `xs`; `(0, 0)` when empty.
pub fn range(xs: &[f32]) -> (f32, f32) {
    xs.iter()
        .fold(None, |acc: Option<(f32, f32)>, &x| {
            Some(acc.map_or((x, x), |(lo, hi)| (lo.min(x), hi.max(x))))
        })
        .unwrap_or((0.0, 0.0))
}

/// Running `[min, max]` of the values one output group produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observed {
    pub min: f32,
    pub max: f32,
}

impl Default for Observed {
    fn default() -> Self {
        Self {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
        }
    }
}

impl Observed {
    pub fn update(&mut self, xs: &[f32]) {
        for &x in xs {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.min > self.max
    }

    pub fn affine(&self) -> Affine {
        if self.is_empty() {
            Affine::from_range(0.0, 0.0)
        } else {
            Affine::from_range(self.min, self.max)
        }
    }
}

/// Several `[n_i, k]` matrices as one program's rows: part i starts at row
/// `offsets[i]`, a multiple of the rows a compute tile holds, so no tile mixes
/// two matrices. The padding rows are zero weights and their outputs are dropped.
#[derive(Clone, Debug)]
pub struct Stack {
    pub k: usize,
    pub parts: Vec<usize>,
    pub offsets: Vec<usize>,
    pub program: FcProgram,
}

impl Stack {
    /// `None` when the stack would need more than the chip's 16 tiles per pass
    /// layout the program supports.
    pub fn new(parts: &[usize], k: usize) -> Option<Self> {
        if parts.is_empty() || parts.contains(&0) || k == 0 {
            return None;
        }
        // Rows per tile depend on the total, and the total on the padding:
        // grow the per-tile rows until the padded layout is stable.
        let mut per_tile = 64;
        loop {
            let offsets: Vec<usize> = parts
                .iter()
                .scan(0, |at, &n| {
                    let start = *at;
                    *at += round_up(n, per_tile);
                    Some(start)
                })
                .collect();
            let n = offsets.last()? + parts.last()?;
            let program = FcProgram::new(n, k);
            let tile = 64 * program.geom().p();
            if tile == per_tile || parts.len() == 1 {
                return Some(Self {
                    k,
                    parts: parts.to_vec(),
                    offsets,
                    program,
                });
            }
            if tile < per_tile {
                return None;
            }
            per_tile = tile;
        }
    }

    /// Program outputs: the parts and the padding between them.
    pub const fn n(&self) -> usize {
        self.program.n
    }

    /// Rows a compute tile holds.
    pub fn tile_rows(&self) -> usize {
        64 * self.program.geom().p()
    }

    pub fn tiles(&self) -> usize {
        self.program.geom().t()
    }

    /// The program rows of tile t.
    pub fn tile_span(&self, t: usize) -> std::ops::Range<usize> {
        let per = self.tile_rows();
        per * t..(per * (t + 1)).min(self.n())
    }

    /// Part and row within it of program row `row`; `None` for padding.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        let i = self.offsets.partition_point(|&o| o <= row).checked_sub(1)?;
        let r = row - self.offsets[i];
        (r < self.parts[i]).then_some((i, r))
    }
}

/// One tile's rows of the stacked weights, quantized with their own scale.
#[derive(Clone, Debug, PartialEq)]
pub struct QuantizedRows {
    pub weights: Affine,
    /// `[rows, round_up(k, 4)]`, K padded with the zero point.
    pub data: Vec<u8>,
}

/// Quantizes the rows `rows` of `w` (row-major `[*, k]` f32) with one affine
/// scale; `None` rows (padding) are zero weights at the zero point.
pub fn quantize_rows(rows: &[Option<&[f32]>], k: usize) -> QuantizedRows {
    let (lo, hi) = rows
        .iter()
        .flatten()
        .map(|row| range(row))
        .fold((0.0f32, 0.0f32), |(a, b), (lo, hi)| (a.min(lo), b.max(hi)));
    let weights = Affine::from_range(lo, hi);
    let k4 = round_up(k, 4);
    let mut data = Vec::with_capacity(rows.len() * k4);
    for row in rows {
        match row {
            Some(row) => {
                data.extend(row.iter().map(|&w| weights.quantize(w)));
                data.resize(data.len() + k4 - k, weights.zp);
            }
            None => data.resize(data.len() + k4, weights.zp),
        }
    }
    QuantizedRows { weights, data }
}

/// Input bytes of a program with `k` inputs padded to `len` with the zero point.
pub fn quantize_input(x: &[f32], affine: Affine, out: &mut [u8]) {
    let (head, tail) = out.split_at_mut(x.len());
    for (q, &v) in head.iter_mut().zip(x) {
        *q = affine.quantize(v);
    }
    tail.fill(affine.zp);
}
