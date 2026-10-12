//! Opcode 0x01 tile op (TensorOp) and 0x02 (pooling op), 17 words, reverse
//! engineered from edgetpu_compiler output:
//!   [0, 62)       header: predication, opcode, tile mask, dispatch sequence number
//!   [62, 174)     main loop nest loop0..loop7 (iteration count - 1, innermost first)
//!   [174, 557)    TTU in_   narrow-memory read (input activations)
//!   [557, 957)    TTU out_  narrow-memory write (outputs after the non-linear unit), + a partial last group
//!   [957, 1260)   TTU par_  wide-memory read (parameters)
//!   [1260, 1563)  TTU psum_ wide-memory read (partial sums)
//!   [1563, 2176)  datapath / output stage: flags, reduction mask, channel counts, zero points, f32 multiplier / clamps / offset
//! A TTU increment is byte granular (narrow) or 64-byte granular (wide):
//! `{p}_inc{k}` holds increment >> 2, its 2 low bits sit in the previous dim's
//! `{p}_mode{k-1}` slot (`{p}_hmode` for dim 0); `{p}_mode7` is a separate flag.

use super::{FieldSpec, Fields, Layout, cdiv, f32_bits, fields, round_up, spec, sspec, ttu};

use std::sync::LazyLock;

/// Address (base bits, + a 14th bit for wide memory), 2-bit header mode, 8 dims of
/// (inc s<sw>, cnt u<cw>, mode u2).
fn ttu_fields(p: &str, lo: usize, base: usize, sel: bool, sw: usize, cw: usize) -> Vec<FieldSpec> {
    let sel_bits = usize::from(sel);
    let mut out = vec![spec(&format!("{p}_base"), lo, base)];
    if sel {
        out.push(spec(&format!("{p}_sel"), lo + base, 1));
    }
    out.push(spec(&format!("{p}_hmode"), lo + base + sel_bits, 2));
    for k in 0..8 {
        let r = lo + base + sel_bits + 2 + (sw + cw + 2) * k;
        out.push(sspec(&format!("{p}_inc{k}"), r, sw));
        out.push(spec(&format!("{p}_cnt{k}"), r + sw, cw));
        out.push(spec(&format!("{p}_mode{k}"), r + sw + cw, 2));
    }
    out
}

pub static OP: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![spec("tile_mask", 12, 16), spec("seq", 46, 16)];
    f.extend((0..8).map(|k| spec(&format!("loop{k}"), 62 + 14 * k, 14)));
    f.extend(ttu_fields("in", 191, 18, false, 17, 18));
    f.extend([spec("in_tflags", 507, 8), spec("in_twait", 522, 14)]);
    f.extend(ttu_fields("out", 574, 18, false, 17, 18));
    f.extend([
        spec("out_tflags", 890, 3), sspec("out_last_inc", 901, 17), spec("out_last_cnt", 918, 18), spec("out_last_mode", 936, 2),
        spec("out_last_skip", 941, 16),
    ]);
    f.extend(ttu_fields("par", 957, 13, true, 13, 14));
    f.extend([spec("par_tflags", 1205, 8), spec("par_fifo", 1216, 14)]);
    f.extend(ttu_fields("psum", 1260, 13, true, 13, 14));
    f.extend([spec("psum_tflags", 1508, 15), spec("cfg0", 1563, 9), spec("cfg1", 1578, 10)]);
    f.extend((0..5).map(|j| spec(&format!("sync{j}"), 1588 + 16 * j, 16)));
    f.extend([
        spec("cfg2", 1843, 5), spec("cfg3", 1850, 4), spec("w_zp", 1854, 8), spec("in_zp", 1870, 8), spec("dp_mode", 1923, 3),
        spec("reduce_mask", 1926, 8), spec("out_ch_last", 1934, 6), spec("out_ch", 1940, 6), spec("mult_bits", 1949, 32),
        spec("clamp_max_bits", 1981, 32), spec("clamp_min_bits", 2013, 32), sspec("offset", 2045, 32), spec("out_zp", 2077, 8), spec("cfg4", 2093, 1),
    ]);
    Layout::new("op", &[0x01, 0x02], 17, f, Fields::new())
});

pub fn encode_op(f: &Fields) -> Vec<u128> { OP.encode(f) }

pub struct FcTile {
    pub n: usize,
    pub k: usize,
    pub t: usize,
    pub seq: i64,
    /// Narrow byte address of x.
    pub in_base: i64,
    /// Narrow byte address of the tile's first output `y[64*P*t]`.
    pub out_base: i64,
    /// Wide address of the tile's weights (64-byte units, after its 256-byte bias rows).
    pub par_base: i64,
    pub w_zp: i64,
    pub in_zp: i64,
    pub mult: f32,
    pub out_zp: i64,
    /// Default `[-out_zp, 255 - out_zp]`.
    pub clamp_min: Option<f32>,
    pub clamp_max: Option<f32>,
}

/// FULLY_CONNECTED: the op of tile t for `y[N] = W[N,K] x[K]` (uint8), bit-exact with
/// every compiler FC op with K <= 6144.
pub fn fc_tile_fields(a: &FcTile) -> Fields {
    let (kw, g) = (cdiv(a.k, 4), cdiv(a.n, 64));
    let p = cdiv(g, 16);
    let gt = p.min(g - p * a.t);
    assert!(gt >= 1, "tile has no outputs");
    assert!(kw > 1 || p == 1, "K<=4 with N>1024 uses a different schedule (loop1=1, psum TTU active); not modelled");
    let nt = (64 * p).min(a.n - 64 * p * a.t);
    let (ch, last) = (64.min(round_up(nt, 4)), round_up(nt - 64 * (gt - 1), 4));
    let w0 = (ch / 4) as i64;
    let (kw, gt, ch, last) = (kw as i64, gt as i64, ch as i64, last as i64);
    let mut d = fields(&[
        ("tile_mask", 1 << a.t), ("seq", a.seq), ("loop0", kw - 1), ("loop3", gt - 1), ("in_base", a.in_base), ("in_mode7", 3),
        ("in_tflags", 1), ("out_base", a.out_base), ("out_mode7", 3), ("out_tflags", 1), ("par_base", a.par_base), ("par_mode7", 1),
        ("par_tflags", 3), ("psum_mode7", 2), ("psum_hmode", i64::from(kw == 1)), ("psum_tflags", if gt > 1 { 0x8C0 } else { 0 }),
    ])
    .merge(ttu("in_", &[1, kw, kw, 0], &[kw, 1, 1, gt], 8, "cnt"))              // x, re-read once per pass
    .merge(ttu("out_", &[1, cdiv(a.n, 4) as i64, w0], &[w0, 1, gt], 8, "cnt"))  // row stride ceil(N/4) words, passes contiguous
    .merge(ttu("par_", &[1, 0, kw, kw], &[kw, 1, 1, gt], 8, "cnt"))             // kw rows of 256 B per pass
    .merge(ttu("psum_", &[0, 1, 0, 0], &[kw, 1, 1, gt], 8, "cnt"));            // not read, but the nest mirrors the main loop
    if last != ch {
        d = d.merge(fields(&[("out_last_cnt", last / 4 - 1), ("out_last_mode", 2), ("out_last_skip", w0 - last / 4)]));
    }
    d.merge(fields(&[
        ("cfg0", 0xA5), ("cfg1", 0x16C), ("sync0", 0x4000), ("sync1", 0x4000), ("cfg2", if kw == 1 { 6 } else { 7 }), ("cfg4", 1), ("dp_mode", 3),
        ("reduce_mask", 0b101), ("out_ch", ch - 1), ("out_ch_last", last - 1), ("w_zp", a.w_zp), ("in_zp", a.in_zp), ("out_zp", a.out_zp),
        ("mult_bits", f32_bits(a.mult)),
        ("clamp_max_bits", f32_bits(a.clamp_max.unwrap_or((255 - a.out_zp) as f32))),
        ("clamp_min_bits", f32_bits(a.clamp_min.unwrap_or((-a.out_zp) as f32))),
    ]))
}
