//! L2_NORMALIZATION on tile 0 (the RMSNorm of the whole-model program, its weight
//! folded into the next matmul): `v = max(sum((x - zp)^2) s^2, s^2)`, `r = K /
//! sqrt(v)` from the NLU's reciprocal-sqrt spline as a 16-bit integer, then
//! `y = clamp((x - zp) r mult) + zp_out`.

use super::Emitter;
use crate::backend::coral::isa::eltwise::{self as el, mul_feed_n2w_fields, mul32, recip32};
use crate::backend::coral::isa::op::{encode_op, ttu_fine, wide};
use crate::backend::coral::isa::scalar::{self as sc, Sync, tc};
use crate::backend::coral::isa::wide_narrow::{self as wn, encode_narrow_to_wide};
use crate::backend::coral::isa::{Fields, f32r, f64_bits, fields, ttu};

/// The 4-row FIFO that streams x through wide memory.
pub const ROW_FIFO: i64 = 0x2060;
/// x again, one word per wide row, for the scaling op.
pub const ROW_FIFO2: i64 = 0x2058;

/// 1.174 .. 3.437, inf.
const RSQRT_BREAKS: [u32; 7] = [
    0x3f96_545a,
    0x3fb9_f21d,
    0x3fe5_fda8,
    0x4010_06eb,
    0x4033_2a53,
    0x405b_ff99,
    0x7f80_0000,
];
/// The float32 spline of 1/sqrt(m), m in [1, 4): c0..c4 of segments 0..6;
/// segment 7 repeats segment 6.
const RSQRT_C: [u32; 35] = [
    0x4017_74aa,
    0xc03a_a03d,
    0x401b_19a9,
    0xbf88_4aed,
    0x3e43_75ff,
    0x4009_d075,
    0xc00c_8cff,
    0x3fc1_41fd,
    0xbf0c_6a0b,
    0x3da6_6a68,
    0x3ff7_d508,
    0xbfcc_5845,
    0x3f63_2952,
    0xbe85_6fdc,
    0x3cff_b721,
    0x3fde_2545,
    0xbf93_25ab,
    0x3f03_63d0,
    0xbdf7_eecc,
    0x3c3e_c282,
    0x3fc6_d5cf,
    0xbf53_0bc5,
    0x3e96_feed,
    0xbd64_530c,
    0x3b8c_c91c,
    0x3fb2_da7d,
    0xbf19_9e29,
    0x3e31_e7c5,
    0xbcd9_c230,
    0x3ad9_68d1,
    0x3fa3_95cf,
    0xbeeb_2d04,
    0x3de4_1050,
    0xbc69_e220,
    0x3a43_bfa9,
];

/// K: the NLU output r = K / sqrt(v) as a 16-bit integer, K = f32(65535 / f32(1/s)).
pub fn rsqrt_scale(s_in: f64) -> f64 {
    f32r(65535.0 / recip32(s_in))
}

/// The 0x19 NLU table: f32(C K) for the spline coefficients, then the breakpoints
/// and mode 3.
pub fn rsqrt_slots(s_in: f64) -> Vec<u32> {
    let k = rsqrt_scale(s_in);
    let coefficients = RSQRT_C.iter().chain(&RSQRT_C[30..]);
    let mut out: Vec<u32> = coefficients
        .map(|c| (f64::from(f32::from_bits(*c)) * k) as f32)
        .map(f32::to_bits)
        .collect();
    out.extend(RSQRT_BREAKS);
    out.extend([3, 0, 0]);
    out
}

/// `(s^2, mult)` of the TFLite output quantization (1/128, 128).
pub fn l2norm_quant(s_in: f64) -> (f64, f64) {
    let s = f32r(s_in);
    (mul32(s, s), mul32(mul32(s, recip32(rsqrt_scale(s))), 128.0))
}

/// x (`w` words at narrow `x` on tile 0) into the FIFO at wide `fifo`, one word per
/// wide row.
fn feed(seq: i64, x: i64, w: i64, fifo: i64, extra: Fields) -> Vec<u128> {
    encode_narrow_to_wide(&mul_feed_n2w_fields(seq, 1, x, fifo, w, 1, 1).merge(extra))
}

/// The reduction op: every FIFO row (one word of x, through the par TTU) times the
/// input word (x again through the in TTU), accumulated over all `w` words.
fn reduce_op_fields(
    w: i64,
    seq: i64,
    x: i64,
    out: i64,
    zp: i64,
    q: &el::Requant,
    fifo: i64,
) -> Fields {
    let depth = el::mul_fifo_depth(w);
    let r = w % depth;
    let mut d = fields(&[
        ("tile_mask", 1),
        ("seq", seq),
        ("loop0", depth - 1),
        ("loop1", (w + depth - 1) / depth - 1),
        ("out_base", out),
        ("out_mode7", 3),
        ("out_tflags", 3),
        ("par_mode7", 1),
        ("par_tflags", 0x40),
        ("par_fifo", depth),
        ("psum_tflags", 0x840),
        ("cfg0", 0x60),
        ("cfg1", 0x12d),
        ("sync0", 0x4000),
        ("sync1", 0x4000),
        ("cfg2", 6),
        ("w_zp", zp),
        ("dp_mode", 1),
        ("reduce_mask", 1),
        ("out_ch", 3),
        ("out_ch_last", 3),
        ("out_zp", q.out_zp),
        ("mult_bits", f64_bits(q.mult)),
        ("clamp_min_bits", f64_bits(q.clamp_min)),
        ("clamp_max_bits", f64_bits(q.clamp_max)),
    ])
    .merge(ttu_fine("out", &[4, 8, 8, 8], &[2, 1, 1, 1]))
    .merge(ttu_fine("par", &[4], &[depth, (w + depth - 1) / depth]))
    .merge(wide("par", fifo))
    .merge(ttu("psum_", &[], &[w], 8, "cnt"))
    .merge(ttu_fine("in", &[4, 4 * w, 4 * w], &[w, 1, 1]))
    .merge(fields(&[
        ("in_base", x),
        ("in_mode7", 3),
        ("in_tflags", 1),
        ("in_zp", zp),
    ]));
    if r != 0 {
        d = d.merge(fields(&[
            ("rsv174", 0x4000 | (r - 1)),
            ("rsv1230", (1 << 13) | (r - 1) | (depth - r) << 18),
        ]));
    }
    if w == 1 {
        d.put("psum_hmode", 1);
    }
    d
}

/// The L2_NORMALIZATION ops on tile 0 (x gathered at narrow `x`): the op fence,
/// the NLU's rsqrt table, x into the 4-row FIFO, op 1 (sum of squares -> NLU ->
/// the 16-bit r at narrow `z`), x again into the one-row FIFO, op 2 (y at narrow `y`).
#[expect(clippy::too_many_arguments, reason = "mirrors the compiler's stage")]
pub fn l2norm_stage(
    e: &mut Emitter,
    n: i64,
    x: i64,
    y: i64,
    z: i64,
    in_q: el::Q,
    out_q: el::Q,
    fifo: i64,
    fifo2: i64,
) {
    assert!(n % 4 == 0, "L2_NORMALIZATION of {n} bytes");
    let (s, zp, w) = (f32r(in_q.0), in_q.1, n / 4);
    let depth = el::mul_fifo_depth(w);
    let rounds = (w + depth - 1) / depth;
    let (sq, mult_ref) = l2norm_quant(s);
    let (mult, lo, hi, ozp) = if out_q == (1.0 / 128.0, 128) {
        (mult_ref, -128.0, 127.0, 128)
    } else {
        let (lo, hi) = el::out_clamps(out_q.0, out_q.1);
        (
            mul32(mul32(s, recip32(rsqrt_scale(s))), recip32(out_q.0)),
            lo,
            hi,
            out_q.1,
        )
    };
    e.sync(sc::sync_op_fence);
    let seq = e.seq;
    e.tile(el::encode_nlu_slots(1, seq, &rsqrt_slots(s)));
    let seq = e.seq;
    e.tile(feed(
        seq,
        x,
        w,
        fifo,
        fields(&[("sync_en1", 0), ("sync_val", 0x8000)]),
    ));
    let seq = e.seq;
    let sum = el::Requant {
        in_zp: 0,
        out_zp: 0,
        mult: sq,
        clamp_min: sq,
        clamp_max: f64::INFINITY,
    };
    e.tile(encode_op(
        &reduce_op_fields(w, seq, x, z, zp, &sum, fifo)
            .merge(fields(&[("cfg2", 7), ("rsv1946", 7)])),
    ));
    e.sync(|seq| {
        sc::sync(
            seq,
            &Sync {
                counters: tc(&["PARAMETERS"]),
                count: rounds,
                ..Sync::default()
            },
        )
    });
    let single = w == 1;
    // The 20-bit sync value at bit 12 of the record (en1 = its low bit).
    let v = 0x10000 + rounds;
    let seq = e.seq;
    e.tile(encode_narrow_to_wide(
        &fields(&[
            ("tile_mask", 1),
            ("seq", seq),
            ("narrow_addr", x / 4),
            ("narrow_lvl_mask", i64::from(!single)),
            ("wide_addr", fifo2),
            ("wide_rows", 1),
            ("wide_circ", i64::from(single)),
            ("sync_f1", i64::from(single)),
            ("sync_wait_lvl", i64::from(single)),
            ("sync_id", 1),
            ("sync_en1", v & 1),
            ("sync_val", v >> 1),
            ("sync_f45", 1),
            ("tail_f799", 1),
            ("tail_lvl", i64::from(single)),
        ])
        .merge(wn::ttu("n", &[1], &[w], 6))
        .merge(wn::ttu("w", &[i64::from(single)], &[w], 4)),
    ));
    let seq = e.seq;
    e.tile(encode_op(
        &fields(&[
            ("tile_mask", 1),
            ("seq", seq),
            ("loop1", w - 1),
            ("in_base", z),
            ("in_mode7", 1),
            ("in_tflags", 3),
            ("out_base", y),
            ("out_mode7", 3),
            ("out_tflags", 1),
            ("par_mode7", 1),
            ("par_tflags", 0x40),
            ("par_fifo", 1),
            ("psum_mode7", 1),
            ("psum_tflags", 0x840),
            ("cfg0", 0x67),
            ("cfg1", 0x2d | ((rounds + 1) % 4) << 8),
            ("sync0", 0x4000 | ((rounds + 1) / 4)),
            ("sync1", 0x4000),
            ("cfg2", 0xe),
            ("cfg3", 6),
            ("w_zp", zp),
            ("dp_mode", 1),
            ("out_ch", 3),
            ("out_ch_last", 3),
            ("out_zp", ozp),
            ("mult_bits", f64_bits(mult)),
            ("clamp_min_bits", f64_bits(lo)),
            ("clamp_max_bits", f64_bits(hi)),
        ])
        .merge(ttu_fine("in", &[2, 0, 8, 8], &[1, w, 1, 1]))
        .merge(ttu_fine("out", &[4, 4 * w, 4 * w], &[w, 1, 1]))
        .merge(ttu_fine("par", &[4, 0], &[1, w]))
        .merge(wide("par", fifo2))
        .merge(ttu_fine("psum", &[4, 0], &[1, w])),
    ));
    e.sync(|seq| {
        sc::sync(
            seq,
            &Sync {
                counters: tc(&["PARAMETERS"]),
                count: 2 * rounds,
                ..Sync::default()
            },
        )
    });
}
