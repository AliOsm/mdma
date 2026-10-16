//! Rows dotted without writing them out. Each weight is dequantized exactly
//! as Candle's `to_float` does and added to the lane `row_dot` would add it
//! to, in the same order, so the sum is bit-identical to dequantizing the row
//! and calling `row_dot`, minus a row of f32 stored and read back.
use super::{CpuBuf, LANES};
use crate::backend::QWeight;
use crate::gguf::GgmlDType;
use half::f16;

const Q4K_VALUES: usize = 256;
/// d and dmin (f16 each), 12 bytes of 6-bit scales and mins, 128 of nibbles.
const Q4K_BYTES: usize = 144;

/// Row `row` of `w` against `x`, when `w`'s type has a fused kernel.
pub(super) fn row_dot(w: &QWeight<CpuBuf>, row: usize, n_in: usize, x: &[f32]) -> Option<f32> {
    match w.dtype {
        GgmlDType::Q4K if n_in.is_multiple_of(Q4K_VALUES) && x.len() == n_in => {
            let row_bytes = Q4K_BYTES * (n_in / Q4K_VALUES);
            let start = row.checked_mul(row_bytes)?;
            let bytes = w.buf.0.get(start..start.checked_add(row_bytes)?)?;
            Some(super::kernels().q4k(bytes, x))
        }
        _ => None,
    }
}

/// Candle's `get_scale_min_k4`.
fn scale_min(j: usize, q: &[u8]) -> (u8, u8) {
    if j < 4 {
        (q[j] & 63, q[j + 4] & 63)
    } else {
        (
            (q[j + 4] & 0xF) | ((q[j - 4] >> 6) << 4),
            (q[j + 4] >> 4) | ((q[j] >> 6) << 4),
        )
    }
}

super::isa::multiversion! {
    /// `q4k` per level; every level gives the same bits.
    pub(super) fn q4k_at(row: &[u8], x: &[f32]) -> f32 = q4k;
}

#[expect(
    clippy::inline_always,
    reason = "each multiversioned variant compiles this body with its own instruction sets"
)]
#[inline(always)]
pub(super) fn q4k(row: &[u8], x: &[f32]) -> f32 {
    let mut lanes = [0f32; LANES];
    // Every run below is 16 values starting at a multiple of 16, taken in
    // increasing order, so value i lands in lane i % 16 after value i - 16.
    let mut add = |weights: &[f32; LANES], x: &[f32]| {
        for i in 0..LANES {
            lanes[i] += weights[i] * x[i];
        }
    };
    for (block, x) in row
        .as_chunks::<Q4K_BYTES>()
        .0
        .iter()
        .zip(x.as_chunks::<Q4K_VALUES>().0)
    {
        let d = f16::from_le_bytes([block[0], block[1]]).to_f32();
        let min = f16::from_le_bytes([block[2], block[3]]).to_f32();
        let (scales, qs) = (&block[4..16], &block[16..]);
        for (chunk, j) in (0..Q4K_VALUES).step_by(64).enumerate() {
            let q = &qs[j / 2..j / 2 + 32];
            let (sc, m) = scale_min(chunk * 2, scales);
            let (d1, m1) = (d * f32::from(sc), min * f32::from(m));
            let (sc, m) = scale_min(chunk * 2 + 1, scales);
            let (d2, m2) = (d * f32::from(sc), min * f32::from(m));
            let mut weights = [0f32; LANES];
            for half in 0..2 {
                let q = &q[half * LANES..(half + 1) * LANES];
                for (w, q) in weights.iter_mut().zip(q) {
                    *w = d1 * f32::from(q & 0xF) - m1;
                }
                add(&weights, &x[j + half * LANES..]);
            }
            for half in 0..2 {
                let q = &q[half * LANES..(half + 1) * LANES];
                for (w, q) in weights.iter_mut().zip(q) {
                    *w = d2 * f32::from(q >> 4) - m2;
                }
                add(&weights, &x[j + 32 + half * LANES..]);
            }
        }
    }
    let mut sum = 0.0;
    for lane in lanes {
        sum += lane;
    }
    sum
}
