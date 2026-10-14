//! Elementwise and pooling tile ops and the instructions that feed them: LOGISTIC
//! (op 0x01 + the 0x19 NLU spline load), the requantize op (RELU-type / QUANTIZE),
//! MUL (two ops), ADD (an op with 16-bit weights), the pooling op, and the
//! narrowToWide / mesh-fill instructions that move data between chained ops. Op
//! builders return the fields for `op::encode_op`; strides are in bytes (narrow)
//! or 64-byte units (wide), `R` are (position, row, block) strides in words. The
//! quantization helpers reproduce edgetpu_compiler's float32 arithmetic.

use super::op::{no_requant, ttu_fine, wide};
use super::ring_mesh::encode_mesh;
use super::scalar::sync_drain;
use super::wide_narrow::{self as wn, encode_narrow_to_wide};
use super::{FieldSpec, Fields, Layout, cdiv, f32r, f64_bits, fields, spec, ttu};
use std::sync::LazyLock;

/// Opcode 0x19: the NLU spline load, 13 words: 8 x 5 coefficients (f32), 7
/// breakpoints (f32), a mode word, 2 reserved.
pub static NLU: LazyLock<Layout> = LazyLock::new(|| {
    let mut f: Vec<FieldSpec> = vec![spec("tile_mask", 12, 16), spec("seq", 46, 16)];
    f.extend((0..50).map(|k| spec(&format!("slot{k}"), 62 + 32 * k, 32)));
    Layout::new("nlu", &[0x19], 13, f, Fields::new())
});

/// LOGISTIC: a piecewise quartic in the dequantized input, its output in units of
/// 1/256; the same in every compiled program.
pub const LOGISTIC_SLOTS: [u32; 50] = [
    0x4200_549c,
    0x415d_b7c6,
    0x4011_1a70,
    0x3e29_f7bb,
    0x3b95_f260,
    0x4304_a187,
    0x42ab_6803,
    0x41ae_e024,
    0x4025_1eb8,
    0x3df1_36b6,
    0x4307_7562,
    0x42a4_6b63,
    0x4183_71fd,
    0x3f05_681a,
    0xbe04_c80f,
    0x4300_006a,
    0x427f_e396,
    0xbea8_c7fe,
    0xc0c9_f57d,
    0xbf94_c0e4,
    0x4300_00b8,
    0x427f_b331,
    0x3ef2_682d,
    0xc0cf_35b5,
    0x3f9c_b2b2,
    0x42dd_c7c6,
    0x42c5_6c6f,
    0xc1d7_2011,
    0x405b_e5c6,
    0xbe2f_f6d3,
    0x4343_d750,
    0x41ef_bc0b,
    0xc0b6_a9cd,
    0x3efb_0c86,
    0xbc82_c2d7,
    0x4374_cc57,
    0x4085_560c,
    0xbf16_8355,
    0x3d18_7e2a,
    0xba69_a584,
    0xc0b4_79b1,
    0xc03d_c92e,
    0xbfab_a1f5,
    0x3db0_69f8,
    0x3fe7_1fbc,
    0x4099_a343,
    0x4107_8f0a,
    0x0000_0003,
    0x0000_0000,
    0x0000_0000,
];
/// f32 10.396732: the op clamps x to [-c, c] before the NLU.
pub const LOGISTIC_CLAMP: i64 = 0x4126_5904;

/// Loads LOGISTIC's spline into the NLU of every tile in `tile_mask`.
pub fn encode_nlu(tile_mask: i64, seq: i64) -> Vec<u128> {
    encode_nlu_slots(tile_mask, seq, &LOGISTIC_SLOTS)
}

/// Loads a 50-slot table into the NLU of every tile in `tile_mask`.
pub fn encode_nlu_slots(tile_mask: i64, seq: i64, slots: &[u32]) -> Vec<u128> {
    let mut f = fields(&[("tile_mask", tile_mask), ("seq", seq)]);
    for (k, s) in slots.iter().enumerate() {
        f.put(&format!("slot{k}"), i64::from(*s));
    }
    NLU.encode(&f)
}

// *** tensor placement (compiler rules) ***

/// Rows (columns) of a spatial extent `d` on tile row (column) 0..3: d/4 each, the
/// remainder on 0 / 0,2 / 0,1,2.
pub fn tile_split(d: usize) -> [usize; 4] {
    let extra: &[usize] = match d % 4 {
        0 => &[],
        1 => &[0],
        2 => &[0, 2],
        _ => &[0, 1, 2],
    };
    std::array::from_fn(|i| d / 4 + usize::from(extra.contains(&i)))
}

/// A tensor's per-tile view as the compiler lays it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileGeom {
    /// Positions on the tile.
    pub p: i64,
    pub rows: i64,
    pub cols: i64,
    /// 4-byte words per position.
    pub w: i64,
    /// (position, row, block) strides in words.
    pub r: [i64; 3],
    /// cfg1 bit 8 of the requantize / LOGISTIC op (1-D tensors and C % 4 != 0).
    pub flat: i64,
}

/// `(H, W, C)` of a `[1, n]`, `[1, W, C]` or `[1, H, W, C]` tensor.
pub fn hwc(shape: &[usize]) -> (usize, usize, usize) {
    match shape.len() {
        2 => (1, 1, shape[1]),
        3 => (1, shape[1], shape[2]),
        _ => (shape[1], shape[2], shape[3]),
    }
}

/// Per-tile view of a tensor: a single-position tensor is 1-D, chunks of
/// 64*ceil(C/1024) bytes per tile with every stride the whole tensor's words.
pub fn tile_geometry(shape: &[usize], tile: usize) -> TileGeom {
    let (h, w, c) = hwc(shape);
    if h * w == 1 {
        let b = 64 * cdiv(c, 1024);
        let words = cdiv(b.min(c.saturating_sub(b * tile)), 4) as i64;
        let stride = cdiv(c, 4) as i64;
        return TileGeom {
            p: 1,
            rows: 1,
            cols: 1,
            w: words,
            r: [stride; 3],
            flat: 1,
        };
    }
    let rows = tile_split(h)[tile / 4] as i64;
    let cols = tile_split(w)[tile % 4] as i64;
    let words = cdiv(c, 4) as i64;
    TileGeom {
        p: rows * cols,
        rows,
        cols,
        w: words,
        r: [words, cols * words, rows * cols * words],
        flat: i64::from(c % 4 != 0),
    }
}

// *** quantization: float32 arithmetic exactly as edgetpu_compiler ***

/// A tensor's (scale, zero point).
pub type Q = (f64, i64);

pub fn mul32(a: f64, b: f64) -> f64 {
    f32r(f32r(a) * f32r(b))
}

pub fn recip32(s: f64) -> f64 {
    f32r(1.0 / f32r(s))
}

/// `(clamp_min, clamp_max)` relative to the output zero point: the real range
/// `[(0-zp)s, (255-zp)s]` times f32(1/s), every step rounded to float32.
pub fn out_clamps(scale: f64, zp: i64) -> (f64, f64) {
    let lo = mul32((-zp) as f64, scale);
    let hi = mul32((255 - zp) as f64, scale);
    (mul32(lo, recip32(scale)), mul32(hi, recip32(scale)))
}

/// The output stage of an op.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Requant {
    pub in_zp: i64,
    pub out_zp: i64,
    pub mult: f64,
    pub clamp_min: f64,
    pub clamp_max: f64,
}

/// QUANTIZE-type requantization `y = clamp((x - in_zp) * mult) + out_zp`.
pub fn requant_quant(in_q: Q, out_q: Q) -> Requant {
    let (clamp_min, clamp_max) = out_clamps(out_q.0, out_q.1);
    Requant {
        in_zp: in_q.1,
        out_zp: out_q.1,
        mult: mul32(in_q.0, recip32(out_q.0)),
        clamp_min,
        clamp_max,
    }
}

/// LOGISTIC: `x = clamp((q - in_zp) * in_scale, +-c)`, `y = spline(x) + out_zp`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogisticQuant {
    pub in_zp: i64,
    pub out_zp: i64,
    pub mult: f64,
}

pub fn logistic_quant(in_q: Q, out_zp: i64) -> LogisticQuant {
    LogisticQuant {
        in_zp: in_q.1,
        out_zp,
        mult: f32r(in_q.0),
    }
}

/// A requantized product of two operands, `y = clamp(sum (a - a_zp)(b - b_zp) *
/// mult) + out_zp`: MUL (a through the in TTU, b from the wide FIFO) and
/// FULLY_CONNECTED (a the input, b the weights).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProductQuant {
    pub a_zp: i64,
    pub b_zp: i64,
    pub out_zp: i64,
    pub mult: f64,
    pub clamp_min: f64,
    pub clamp_max: f64,
}

/// `mult = f32(f32(s_a s_b) f32(1/s_y))`, float32 clamps.
pub fn product_quant(a_q: Q, b_q: Q, y_q: Q) -> ProductQuant {
    let (clamp_min, clamp_max) = out_clamps(y_q.0, y_q.1);
    ProductQuant {
        a_zp: a_q.1,
        b_zp: b_q.1,
        out_zp: y_q.1,
        mult: mul32(mul32(a_q.0, b_q.0), recip32(y_q.0)),
        clamp_min,
        clamp_max,
    }
}

/// An exact rational `num / den` (both positive).
fn ratio_of(x: f64) -> (u128, u128) {
    let bits = (x as f32).to_bits();
    let exp = ((bits >> 23) & 0xff) as i32;
    let mantissa = u128::from(bits & 0x7f_ffff);
    let (m, e) = if exp == 0 {
        (mantissa, -149)
    } else {
        (mantissa | 1 << 23, exp - 150)
    };
    if e >= 0 {
        (m << e, 1)
    } else {
        (m, 1u128 << -e)
    }
}

const fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// CPython's `Fraction.limit_denominator` of `n / d` (reduced).
fn limit_denominator(n: u128, d: u128, max: u128) -> (u128, u128) {
    let g = gcd(n, d);
    let (n0, d0) = (n / g, d / g);
    if d0 <= max {
        return (n0, d0);
    }
    let (mut p0, mut q0, mut p1, mut q1) = (0u128, 1u128, 1u128, 0u128);
    let (mut n, mut d) = (n0, d0);
    loop {
        let a = n / d;
        let q2 = q0 + a * q1;
        if q2 > max {
            break;
        }
        (p0, q0, p1, q1) = (p1, q1, p0 + a * p1, q2);
        (n, d) = (d, n - a * d);
    }
    let k = (max - q0) / q1;
    if 2 * d * (q0 + k * q1) <= d0 {
        (p1, q1)
    } else {
        (p0 + k * p1, q0 + k * q1)
    }
}

/// Integer weights `(w1, w2)` of ADD: w2/w1 is the best rational approximation of
/// s2/s1 with terms <= 32767.
pub fn add_weights(s1: f64, s2: f64) -> (i64, i64) {
    const LIMIT: u128 = 32767;
    let (a, b) = ratio_of(s1);
    let (c, d) = ratio_of(s2);
    // r = s2 / s1 = (c / d) / (a / b) = (c b) / (d a)
    let (num, den) = (c * b, d * a);
    if num <= den {
        let (p, q) = limit_denominator(num, den, LIMIT);
        (q as i64, p as i64)
    } else {
        let (p, q) = limit_denominator(den, num, LIMIT);
        (p as i64, q as i64)
    }
}

/// ADD: operand 1 at in_base, operand 2 at in_base + delta,
/// `acc = w1 q1 + w2 q2 + offset`, `y = clamp(acc * mult) + out_zp`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AddQuant {
    pub w1: i64,
    pub w2: i64,
    pub offset: i64,
    pub out_zp: i64,
    pub mult: f64,
    pub clamp_min: f64,
    pub clamp_max: f64,
}

pub fn add_quant(q1: Q, q2: Q, y_q: Q) -> AddQuant {
    let (w1, w2) = add_weights(q1.0, q2.0);
    let (clamp_min, clamp_max) = out_clamps(y_q.0, y_q.1);
    AddQuant {
        w1,
        w2,
        offset: -(w1 * q1.1 + w2 * q2.1),
        out_zp: y_q.1,
        mult: mul32(f32r(f32r(q1.0) / w1 as f64), recip32(y_q.0)),
        clamp_min,
        clamp_max,
    }
}

fn out(out_zp: i64, mult: f64, clamp_min: f64, clamp_max: f64) -> Fields {
    fields(&[
        ("out_zp", out_zp),
        ("mult_bits", f64_bits(mult)),
        ("clamp_min_bits", f64_bits(clamp_min)),
        ("clamp_max_bits", f64_bits(clamp_max)),
    ])
}

// *** op builders ***

/// The requantize op `y = clamp((x - in_zp)*mult) + out_zp` over `p` positions x
/// `cw` words per tile: the MXU multiplies each input word by the 4x4 identity at
/// wide `ident_addr`. `r_in` / `r_out`: position strides in words.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn requant_op_fields(
    p: i64,
    cw: i64,
    r_in: i64,
    r_out: i64,
    tile_mask: i64,
    seq: i64,
    in_base: i64,
    out_base: i64,
    ident_addr: i64,
    q: &Requant,
    flat: i64,
) -> Fields {
    let single = p * cw == 1;
    let loops: Vec<i64> = [p, cw].into_iter().filter(|n| *n > 1).collect();
    fields(&[
        ("tile_mask", tile_mask),
        ("seq", seq),
        ("loop0", p - 1),
        ("loop1", cw - 1),
        ("in_base", in_base),
        ("in_mode7", 3),
        ("in_tflags", 1),
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 1),
    ])
    .merge(ttu_fine("in", &[4, 4 * r_in, 4], &[1, p, cw]))
    .merge(ttu_fine("out", &[4, 4 * r_out, 4], &[1, p, cw]))
    .merge(wide("par", ident_addr))
    .merge(ttu("par_", &[], &loops, 8, "cnt"))
    .merge(ttu("psum_", &[], &loops, 8, "cnt"))
    .merge(fields(&[
        ("par_hmode", i64::from(single)),
        ("psum_hmode", i64::from(single)),
        ("psum_tflags", if single { 0 } else { 0x800 }),
        ("cfg0", 0x5),
        ("cfg1", 0x4D | flat << 8),
        ("sync0", 0x4000),
        ("sync1", 0x4000),
        ("cfg2", 6),
        ("dp_mode", 1),
        ("out_ch", 3),
        ("out_ch_last", 3),
        ("in_zp", q.in_zp),
    ]))
    .merge(out(q.out_zp, q.mult, q.clamp_min, q.clamp_max))
}

/// LOGISTIC: the requantize op with mult = the input scale, both clamps = +-c and
/// bit 1946 set; the preceding 0x19 loads the spline.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn logistic_op_fields(
    p: i64,
    cw: i64,
    r_in: i64,
    r_out: i64,
    tile_mask: i64,
    seq: i64,
    in_base: i64,
    out_base: i64,
    ident_addr: i64,
    q: &LogisticQuant,
    flat: i64,
) -> Fields {
    let requant = Requant {
        in_zp: q.in_zp,
        out_zp: q.out_zp,
        mult: q.mult,
        clamp_min: 0.0,
        clamp_max: 0.0,
    };
    requant_op_fields(
        p, cw, r_in, r_out, tile_mask, seq, in_base, out_base, ident_addr, &requant, flat,
    )
    .merge(fields(&[
        ("rsv1946", 1),
        ("clamp_min_bits", LOGISTIC_CLAMP | 1 << 31),
        ("clamp_max_bits", LOGISTIC_CLAMP),
    ]))
}

/// Wide FIFO rows holding MUL operand b (one 4-byte word of b per 256-byte row).
pub fn mul_fifo_depth(w: i64) -> i64 {
    4.min((w + 1) / 2)
}

/// MUL step 1: every byte a_i (in TTU, one byte per step) times the FIFO row holding
/// b's word, requantized, written as one 4-byte word per element to the dense
/// intermediate at `out_base`.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn mul_op1_fields(
    w: i64,
    cols: i64,
    rows: i64,
    r: [i64; 3],
    tile_mask: i64,
    seq: i64,
    in_base: i64,
    out_base: i64,
    fifo_addr: i64,
    q: &ProductQuant,
) -> Fields {
    let depth = mul_fifo_depth(w);
    let (m4, p, rem) = (4 * w, cols * rows, w % depth);
    let mut d = fields(&[
        ("tile_mask", tile_mask),
        ("seq", seq),
        ("loop0", 4 * depth - 1),
        ("loop1", (w + depth - 1) / depth - 1),
        ("loop2", cols - 1),
        ("loop3", rows - 1),
        ("in_base", in_base),
        ("in_mode7", 3),
        ("in_tflags", 3),
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 7),
    ])
    .merge(ttu_fine(
        "in",
        &[1, 4 * r[0], 4 * r[1], 4 * r[2]],
        &[m4, cols, rows],
    ))
    .merge(ttu_fine(
        "out",
        &[4, 4, 4 * m4, 4 * cols * m4, 4 * p * m4],
        &[1, m4, cols, rows],
    ))
    .merge(ttu_fine(
        "par",
        &[1],
        &[4 * depth, (w + depth - 1) / depth, cols, rows],
    ))
    .merge(wide("par", fifo_addr))
    .merge(fields(&[
        ("par_mode7", 1),
        ("par_tflags", 0x40),
        ("par_fifo", depth | if rem != 0 { 0x2000 } else { 0 }),
    ]))
    .merge(ttu("psum_", &[], &[1, m4, cols, rows], 8, "cnt"))
    .merge(fields(&[
        ("psum_hmode", 1),
        ("psum_mode7", 1),
        ("psum_tflags", 0x840),
        ("cfg0", 0x60),
        ("cfg1", 0x12D),
        ("sync0", 0x4000),
        ("sync1", 0x4000),
        ("cfg2", 6),
        ("cfg3", 7),
        ("dp_mode", 4),
        ("out_ch", 3),
        ("out_ch_last", 3),
        ("in_zp", q.a_zp),
        ("w_zp", q.b_zp),
    ]))
    .merge(out(q.out_zp, q.mult, q.clamp_min, q.clamp_max));
    if rem != 0 {
        // The partial last FIFO round.
        d = d.merge(fields(&[
            ("rsv174", 0x4000 | (4 * rem - 1)),
            ("rsv1230", (1 << 13) | (2 * rem - 1) | (depth - rem) << 18),
        ]));
    }
    d
}

/// MUL step 2 (dp_mode 2): packs the intermediate back to bytes, output byte k of
/// word j = lane k of intermediate word 4j+k, no requantization.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn mul_op2_fields(
    w: i64,
    cols: i64,
    rows: i64,
    r_out: [i64; 3],
    tile_mask: i64,
    seq: i64,
    in_base: i64,
    out_base: i64,
    ident_addr: i64,
) -> Fields {
    let (m4, p) = (4 * w, cols * rows);
    fields(&[
        ("tile_mask", tile_mask),
        ("seq", seq),
        ("loop0", 3),
        ("loop1", w - 1),
        ("loop2", cols - 1),
        ("loop3", rows - 1),
        ("in_base", in_base),
        ("in_mode7", 3),
        ("in_tflags", 3),
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 3),
    ])
    .merge(ttu_fine(
        "in",
        &[4, 4 * m4, 4 * cols * m4, 4 * p * m4],
        &[m4, cols, rows],
    ))
    .merge(ttu_fine(
        "out",
        &[4, 4 * r_out[0], 4 * r_out[1], 4 * r_out[2]],
        &[w, cols, rows],
    ))
    .merge(ttu_fine("par", &[1], &[4, w, cols, rows]))
    .merge(wide("par", ident_addr))
    .merge(fields(&[("par_mode7", 1), ("par_tflags", 0x40)]))
    .merge(ttu("psum_", &[], &[4, w, cols, rows], 8, "cnt"))
    .merge(fields(&[
        ("psum_tflags", 0x840),
        ("cfg2", 6),
        ("cfg3", 7),
        ("dp_mode", 2),
        ("out_ch", 3),
        ("out_ch_last", 3),
    ]))
    .merge(no_requant())
}

/// ADD (dp_mode 5): per output word 4 accumulation steps, (operand 1 at in_base,
/// operand 2 at in_base + 4*delta bytes) x (low, high 16-bit half of the word),
/// each against one row of the 4x4 16-bit weight matrix at wide `wmat_addr`.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn add_op_fields(
    w: i64,
    cols: i64,
    rows: i64,
    r_in: [i64; 3],
    delta: i64,
    r_out: [i64; 3],
    tile_mask: i64,
    seq: i64,
    in_base: i64,
    out_base: i64,
    wmat_addr: i64,
    q: &AddQuant,
) -> Fields {
    let p = cols * rows;
    fields(&[
        ("tile_mask", tile_mask),
        ("seq", seq),
        ("loop0", 1),
        ("loop1", 1),
        ("loop2", cols - 1),
        ("loop3", rows - 1),
        ("loop5", w - 1),
        ("in_base", in_base),
        ("in_mode7", 3),
        ("in_tflags", 0xF),
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 7),
    ])
    .merge(ttu_fine(
        "in",
        &[2, 4 * delta, 4 * r_in[0], 4 * r_in[1], 4 * r_in[2], 4],
        &[2, 2, cols, rows, 1, w],
    ))
    .merge(ttu_fine(
        "out",
        &[4, 4 * r_out[0], 4 * r_out[1], 4 * r_out[2], 4],
        &[1, cols, rows, 1, w],
    ))
    .merge(ttu_fine("par", &[4], &[4, p * w]))
    .merge(wide("par", wmat_addr))
    .merge(fields(&[("par_mode7", 1)]))
    .merge(ttu("psum_", &[], &[4, p * w], 8, "cnt"))
    .merge(fields(&[
        ("psum_tflags", 0x840),
        ("cfg0", 0x9),
        ("cfg1", 0x1CD),
        ("sync0", 0x4000),
        ("sync1", 0x4000),
        ("cfg2", 0x16),
        ("dp_mode", 5),
        ("reduce_mask", 3),
        ("out_ch", 3),
        ("out_ch_last", 3),
        ("offset", q.offset),
    ]))
    .merge(out(q.out_zp, q.mult, q.clamp_min, q.clamp_max))
}

/// `(Sy, blk)` of the transposed window block in wide memory (64-byte units).
pub fn maxpool_layout(kh: i64, kw: i64, sh: i64, sw: i64, oh: i64, ow: i64) -> (i64, i64) {
    let hb = (oh - 1) * sh + kh;
    let wb = (ow - 1) * sw + kw;
    let sy = 4 * ((wb + 3) / 4);
    (sy, hb * sy)
}

/// The pooling op (opcode 0x02): reads the window from wide memory and reduces
/// loops 0, 1 (kw x kh); OH x OW outputs per tile, C channels in groups of 64.
#[expect(clippy::too_many_arguments, reason = "mirrors the op's fields")]
pub fn maxpool_op_fields(
    kh: i64,
    kw: i64,
    sh: i64,
    sw: i64,
    oh: i64,
    ow: i64,
    c: i64,
    r_out: [i64; 2],
    tile_mask: i64,
    seq: i64,
    out_base: i64,
    src_addr: i64,
    zp: i64,
    clamp_min: f64,
    clamp_max: f64,
) -> Fields {
    let g = (c + 63) / 64;
    let cg = 4 * ((c.min(64) + 3) / 4);
    let cl = 4 * ((c - 64 * (g - 1) + 3) / 4);
    let wg = cg / 4;
    let (sy, blk) = maxpool_layout(kh, kw, sh, sw, oh, ow);
    let mut d = fields(&[
        ("opcode", 2),
        ("tile_mask", tile_mask),
        ("seq", seq),
        ("loop0", kw - 1),
        ("loop1", kh - 1),
        ("loop2", ow - 1),
        ("loop3", oh - 1),
        ("loop4", g - 1),
    ])
    .merge(ttu_fine(
        "par",
        &[1, sy, sw, sh * sy, blk, g * blk],
        &[kw, kh, ow, oh, g],
    ))
    .merge(wide("par", src_addr))
    .merge(fields(&[("par_mode7", 3), ("par_tflags", 0xF)]))
    .merge(ttu_fine(
        "out",
        &[4, 4 * r_out[0], 4 * r_out[1], 4 * wg, 4 * oh * r_out[1]],
        &[wg, ow, oh, g],
    ))
    .merge(fields(&[
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 7),
    ]))
    .merge(ttu("psum_", &[], &[kw, kh, ow, oh, g], 8, "cnt"))
    .merge(fields(&[
        ("psum_tflags", 0x880),
        ("cfg0", 0x127),
        ("cfg2", 6),
        ("cfg3", 7),
        ("dp_mode", 4),
        ("reduce_mask", 3),
        ("out_ch", cg - 1),
        ("out_ch_last", cl - 1),
        ("in_zp", zp),
        ("offset", -zp),
    ]))
    .merge(out(zp, 1.0, clamp_min, clamp_max));
    if cl != cg {
        d = d.merge(fields(&[
            ("out_last_cnt", cl / 4 - 1),
            ("out_last_mode", 3),
            ("out_last_skip", wg - cl / 4),
        ]));
    }
    d
}

// *** data-flow instructions around the ops ***

/// The 4x4 identity row (the weights of the requantize / LOGISTIC / copy ops and of
/// MUL step 2): 4 inbound meshBus 0x17 fills of one word, optionally a zero row of
/// `zero_words` words behind it, then the narrowToWide of it all to `wide_addr`.
/// Consecutive seqs from `seq`.
pub fn ident_prologue(
    seq: i64,
    tiles: i64,
    narrow_byte: i64,
    wide_addr: i64,
    zero_words: i64,
) -> Vec<Vec<u128>> {
    let mut out: Vec<Vec<u128>> = (0..4)
        .map(|j| {
            encode_mesh(&fields(&[
                ("opcode", 0x17),
                ("tile_mask", tiles),
                ("seq", seq + j),
                ("i_addr", narrow_byte + 4 * j),
                ("i_inc0", 1),
                ("fill_en", 1),
                ("fill", 1 << (8 * j)),
                ("in_mode", if j == 3 && zero_words == 0 { 3 } else { 0 }),
            ]))
        })
        .collect();
    let n = 16 + 4 * zero_words;
    let sync = if zero_words != 0 {
        out.push(encode_mesh(
            &fields(&[
                ("opcode", 0x17),
                ("tile_mask", tiles),
                ("seq", seq + 4),
                ("i_addr", narrow_byte + 16),
                ("i_sdims", 1),
                ("fill_en", 1),
            ])
            .merge(ttu("i_", &[1], &[zero_words], 4, "cnt")),
        ));
        out.push(sync_drain(seq + 5, 0xffff));
        fields(&[("sync_en0", 0), ("sync_en1", 0), ("sync_en2", 0)])
    } else {
        fields(&[("sync_f1", 1), ("sync_id", 5), ("sync_wait_lvl", 1)])
    };
    let len = out.len() as i64;
    out.push(encode_narrow_to_wide(
        &fields(&[
            ("tile_mask", tiles),
            ("seq", seq + len),
            ("narrow_addr", narrow_byte / 4),
            ("narrow_lvl_mask", 1),
            ("wide_addr", wide_addr),
            ("wide_lvl_mask", i64::from(n > 256)),
            ("tail_lvl", 1),
        ])
        .merge(sync)
        .merge(wn::ttu("n", &[1], &[n / 4], 6))
        .merge(wn::ttu("w", &[1], &[(n + 255) / 256], 4)),
    ));
    out
}

/// The narrowToWide that streams MUL operand b (`w` words per position, a dense
/// `cols x rows` block at `narrow_byte`) into the wide FIFO, one narrow word per
/// wide row.
pub fn mul_feed_n2w_fields(
    seq: i64,
    tiles: i64,
    narrow_byte: i64,
    fifo_addr: i64,
    w: i64,
    cols: i64,
    rows: i64,
) -> Fields {
    let depth = mul_fifo_depth(w);
    let rem = w % depth;
    let mut dims = vec![(1, w)];
    if cols > 1 {
        dims.push((w, cols));
    }
    if rows > 1 {
        dims.push((cols * w, rows));
    }
    let single = w * cols * rows == 1;
    let circ = i64::from(depth > 1 || w == 1);
    let strides: Vec<i64> = dims.iter().map(|d| d.0).collect();
    let counts: Vec<i64> = dims.iter().map(|d| d.1).collect();
    let wide_walk = if depth > 1 {
        wn::ttu(
            "w",
            &[1, 0, 0],
            &[depth, (w + depth - 1) / depth, cols * rows],
            4,
        )
    } else {
        wn::ttu("w", &[i64::from(w == 1), 0], &[w, cols * rows], 4)
    };
    let mut f = fields(&[
        ("tile_mask", tiles),
        ("seq", seq),
        ("narrow_addr", narrow_byte / 4),
        (
            "narrow_lvl_mask",
            if single { 0 } else { (1 << dims.len()) - 1 },
        ),
        ("wide_addr", fifo_addr),
        ("wide_rows", depth),
        ("sync_id", 1),
        ("sync_val", 0xFFFF),
        ("sync_f45", 1),
    ])
    .merge(wn::ttu("n", &strides, &counts, 6))
    .merge(wide_walk)
    .merge(fields(&[
        ("wide_circ", circ),
        ("wide_lvl_mask", if single { 0 } else { circ }),
        ("sync_f1", circ),
        ("sync_wait_lvl", circ),
        ("tail_lvl", i64::from(single)),
    ]));
    if rem != 0 {
        f.put("rsv502", (rem - 1) << 6 | 1 << 22 | (depth - rem) << 24);
    }
    f
}

/// 10 inbound mesh fills (round robin over meshBus 0x17, 0x18, 0x16, 0x15) that
/// write the ADD weight matrix to `narrow_byte .. +63`: rows 0 / 1 the low / high
/// half-word of operand 1, rows 2 / 3 of operand 2; seqs `seq .. seq+9`.
pub fn add_weight_fills(
    seq: i64,
    tiles: i64,
    narrow_byte: i64,
    w1: i64,
    w2: i64,
) -> Vec<Vec<u128>> {
    let plan = [
        (0, 0, w1),
        (4, 0, w1 << 16),
        (8, 3, 0),
        (24, 0, w1),
        (28, 0, w1 << 16),
        (32, 0, w2),
        (36, 0, w2 << 16),
        (40, 3, 0),
        (56, 0, w2),
        (60, 0, w2 << 16),
    ];
    plan.iter()
        .enumerate()
        .map(|(j, &(off, cnt, fill))| {
            encode_mesh(
                &fields(&[
                    ("opcode", [0x17, 0x18, 0x16, 0x15][j % 4]),
                    ("tile_mask", tiles),
                    ("seq", seq + j as i64),
                    ("i_addr", narrow_byte + off),
                    ("i_sdims", i64::from(cnt > 0)),
                    ("fill_en", 1),
                    ("fill", fill),
                    ("in_mode", 3),
                ])
                .merge(ttu("i_", &[1], &[cnt + 1], 4, "cnt")),
            )
        })
        .collect()
}

/// Moves the 64-byte ADD weight matrix to 4 wide rows at `wide_addr`, waiting on
/// the 4 MESH_*_IN counters.
pub fn add_weight_n2w_fields(seq: i64, tiles: i64, narrow_byte: i64, wide_addr: i64) -> Fields {
    // The reserved sync records (MESH_SOUTH/WEST/EAST_IN), 83 bits from bit 631.
    const RSV589: i64 = 1_099_528_405_829;
    const RSV631: i64 = 2_551_966_504_846_150;
    const RSV693: i64 = 1_048_592;
    fields(&[
        ("tile_mask", tiles),
        ("seq", seq),
        ("narrow_addr", narrow_byte / 4),
        ("narrow_lvl_mask", 3),
        ("wide_addr", wide_addr),
        ("wide_lvl_mask", 1),
        ("sync_en1", 0),
        ("sync_f1", 1),
        ("sync_id", 3),
        ("sync_val", 0x8001),
        ("sync_wait_lvl", 2),
        ("rsv589", RSV589),
        ("rsv631", RSV631),
        ("rsv693", RSV693),
        ("tail_lvl", 1),
    ])
    .merge(wn::ttu("n", &[1, 4], &[4, 4], 6))
    .merge(wn::ttu("w", &[1], &[4], 4))
}

/// The transposing narrowToWide (tail_f799): a `hb x wb` block of `c`-channel
/// positions (position / row stride `rp` / `rr` words) to wide memory, every 4
/// positions x 4 channel words -> 4 columns x 4 byte lanes.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the instruction's fields"
)]
pub fn maxpool_relay_n2w_fields(
    seq: i64,
    tiles: i64,
    narrow_byte: i64,
    wide_addr: i64,
    rp: i64,
    rr: i64,
    wb: i64,
    hb: i64,
    c: i64,
) -> Fields {
    let g = (c + 63) / 64;
    let wg = if g > 1 { 16 } else { (c + 3) / 4 };
    let quads = (wb + 3) / 4;
    let mut dims = vec![(rp, 4)];
    if wg > 1 {
        dims.push((1, wg));
    }
    if quads > 1 {
        dims.push((4 * rp, quads));
    }
    if hb > 1 {
        dims.push((rr, hb));
    }
    if g > 1 {
        dims.push((16, g));
    }
    let strides: Vec<i64> = dims.iter().map(|d| d.0).collect();
    let counts: Vec<i64> = dims.iter().map(|d| d.1).collect();
    let mut f = fields(&[
        ("tile_mask", tiles),
        ("seq", seq),
        ("narrow_addr", narrow_byte / 4),
        ("narrow_lvl_mask", (1 << dims.len()) - 1),
        ("wide_addr", wide_addr),
        ("wide_lvl_mask", if g > 1 { 3 } else { 1 }),
        ("sync_en1", 0),
        ("sync_en2", 0),
        ("sync_f1", 1),
        ("tail_f799", 1),
        ("tail_lvl", if wg > 1 { 2 } else { 1 }),
    ])
    .merge(wn::ttu("n", &strides, &counts, 6))
    .merge(wn::ttu(
        "w",
        &[1, if g > 1 { hb * quads } else { 0 }],
        &[hb * quads, g],
        4,
    ));
    let wl = (c - 64 * (g - 1) + 3) / 4;
    if g > 1 && wl != 16 {
        f.put("rsv282", (wl - 1) << 20 | 3 << 36 | (16 - wl) << 39);
    }
    f
}
