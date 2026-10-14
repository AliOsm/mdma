//! The attention core of a TinyStories-15M layer (6 heads of 48 dims, P
//! positions): scores = SUM over the channels of MUL(q broadcast over the
//! positions, K), the softmax on the scalar core, then SUM over the positions of
//! MUL(p broadcast over the channels, V). A `[1, 6, P, 48]` tensor lies over the
//! 16 tiles with tile row r holding heads `ROWS[r]` and column c positions
//! `cols(c)`.

use super::Emitter;
use super::conv2d::{self as c2, Conv2DGeom, MESH_W, emit_groups, half};
use crate::backend::coral::executable::Description;
use crate::backend::coral::isa::eltwise::{
    self as el, ProductQuant, Requant, maxpool_op_fields, maxpool_relay_n2w_fields,
    mul_feed_n2w_fields, mul_op1_fields, mul_op2_fields, requant_op_fields, tile_split,
};
use crate::backend::coral::isa::op::{encode_op, encode_op3, no_requant, ttu_fine, wide};
use crate::backend::coral::isa::scalar::{self as sc, INFEED, OUTFEED, Scsync, Sync, scm, tc};
use crate::backend::coral::isa::scalar_core::{self as s};
use crate::backend::coral::isa::wide_narrow::{
    self as wn, encode_narrow_to_wide, encode_wide_to_narrow,
};
use crate::backend::coral::isa::{Fields, f32r, f64_bits, fields, ring_mesh as rm, ttu};

pub const NH: usize = 6;
pub const DH: i64 = 48;
pub const DM: i64 = 288;
/// Words per (head, position).
pub const HW: i64 = DH / 4;
/// Heads per tile row.
pub const ROWS: [i64; 4] = [2, 1, 2, 1];
pub const HEAD0: [i64; 4] = [0, 2, 3, 5];
/// Bytes of scalar memory.
pub const SMEM_TOP: i64 = 0x4000;

/// Positions over the tile columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    pub p: i64,
    pub cols: [i64; 4],
}

impl Geom {
    pub fn new(p: i64) -> Self {
        let c = tile_split(p as usize);
        Self {
            p,
            cols: c.map(|v| v as i64),
        }
    }

    /// First position of tile column `c`.
    pub fn p0(&self, c: usize) -> i64 {
        self.cols[..c.min(4)].iter().sum()
    }

    pub fn smax(&self) -> i64 {
        self.cols.iter().copied().max().unwrap_or(0)
    }
}

/// `[1, 6, W, 48]` as an image for the conv input path.
pub fn g4_of(w: i64) -> Conv2DGeom {
    Conv2DGeom::new(NH as i64, w, DH)
}

/// The narrowToWide per tile that streams a `[rows][cols][48]` block at narrow
/// `x` into the MUL / SUM FIFO, tagged with its block shape.
fn feeds(g: &Geom, x: i64, fifo: i64) -> Vec<Option<Fields>> {
    (0..16)
        .map(|t| {
            let (nh, cols) = (ROWS[t / 4], g.cols[t % 4]);
            (cols != 0).then(|| {
                mul_feed_n2w_fields(0, 0, x, fifo, HW, cols, nh)
                    .set(c2::SHAPE[0], nh)
                    .set(c2::SHAPE[1], cols)
            })
        })
        .collect()
}

fn copies(e: &mut Emitter, g4: &Conv2DGeom, stg: i64, x: i64, ident: i64, last: bool) {
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            let skip = g4.cols[t % 4].ni == 0 || (!last && g4.chunks(t / 4).0 < 2);
            (!skip).then(|| c2::copy_op(g4, t / 4, t % 4, stg, x, ident, last))
        })
        .collect();
    emit_groups(e, &per, encode_op);
}

/// A `[1, 6, W, 48]` input (host bytes head-major, one DMA named `input`) into
/// each tile's block at narrow `x`; with `ident` the identity row first.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the compiler's input path"
)]
pub fn input4d(
    e: &mut Emitter,
    w: i64,
    stg: i64,
    x: i64,
    c: i64,
    ident_wide: i64,
    in_fifo: i64,
    ident: bool,
    input: &str,
) {
    let g4 = g4_of(w);
    let cmask: i64 = (0..4).filter(|c| g4.cols[*c].ni != 0).map(|c| 1 << c).sum();
    e.sync(sc::input_head);
    if ident {
        let seq = e.seq;
        e.tiles(el::ident_prologue(seq, 0xffff, c, ident_wide, 0));
    }
    e.sync(sc::sync_wn_fence);
    copies(e, &g4, stg, x, ident_wide, false);
    e.scalar_with_base(
        sc::input_dma(g4.s(), 0),
        Description::InputActivation,
        Some(input),
        false,
    );
    let fenced: [bool; 4] = std::array::from_fn(|r| c2::ring_passes(&g4, r).0 >= 3);
    if fenced.iter().any(|f| *f) {
        let tiles: i64 = (0..4)
            .filter(|r| fenced[*r])
            .map(|r| cmask << (4 * r))
            .sum();
        e.sync(|seq| {
            sc::sync(
                seq,
                &Sync {
                    tiles,
                    counters: tc(&["WIDE_TO_NARROW"]),
                    units: 0,
                    ..Sync::default()
                },
            )
        });
    }
    for part in [false, true] {
        let per: Vec<Option<Fields>> = (0..16)
            .map(|t| {
                (fenced[t / 4] == part && g4.cols[t % 4].ni != 0)
                    .then(|| c2::w2n_input(&g4, t / 4, t % 4, stg, in_fifo))
            })
            .collect();
        emit_groups(e, &per, encode_wide_to_narrow);
    }
    for r in 0..4 {
        let seq = e.seq;
        let rc = c2::rc_input(&g4, r, seq, in_fifo).set("tile_mask", cmask << (4 * r));
        e.tile(rm::encode_ring_consumer(&rc));
        let (lo, n) = c2::row_stream(&g4, r);
        let (a, b) = (lo / 8, (lo + n + 7) / 8);
        e.scalar(c2::av_infeed(8 * a, 8 * (b - a), cmask << (4 * r)), 0);
    }
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    copies(e, &g4, stg, x, ident_wide, true);
}

/// Operand a's walk of a MUL: (in_base, byte strides, counts), innermost first.
pub type Walk = (i64, Vec<i64>, Vec<i64>);

/// MUL of two `[6, P, 48]` tensors: step 1 per tile (operand a through the in TTU,
/// `a_in(t)`), the FIFO feed of operand b (block at `x_b`), step 2 per tile into
/// `y` with the output row stride `out_rows(t)` words.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the compiler's MUL stage"
)]
pub fn mul_stage(
    e: &mut Emitter,
    g: &Geom,
    a_in: &dyn Fn(usize) -> Walk,
    x_b: i64,
    m: i64,
    y: i64,
    fifo: i64,
    ident: i64,
    q: &ProductQuant,
    out_rows: &dyn Fn(usize) -> i64,
) {
    e.sync(sc::sync_reset17);
    e.sync(sc::sync_reset_n2w);
    e.sync(sc::sync_reset_par);
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            let (nh, cols) = (ROWS[t / 4], g.cols[t % 4]);
            (cols != 0).then(|| {
                let (in_base, strides, counts) = a_in(t);
                mul_op1_fields(
                    HW,
                    cols,
                    nh,
                    [HW, HW * cols, HW * cols * nh],
                    0,
                    0,
                    in_base,
                    m,
                    fifo,
                    q,
                )
                .merge(ttu_fine("in", &strides, &counts))
            })
        })
        .collect();
    emit_groups(e, &per, encode_op);
    emit_groups(e, &feeds(g, x_b, fifo), encode_narrow_to_wide);
    e.sync(sc::sync_wn_fence);
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            let (nh, cols) = (ROWS[t / 4], g.cols[t % 4]);
            (cols != 0).then(|| {
                let ro = out_rows(t);
                mul_op2_fields(HW, cols, nh, [HW, ro, ro * nh], 0, 0, m, y, ident)
            })
        })
        .collect();
    emit_groups(e, &per, encode_op);
    e.sync(sc::sync_reset_n2w);
    e.sync(sc::sync_reset_par);
    e.sync(sc::sync_reset17);
}

/// SUM over the positions on the column-0 tiles: the transposing relay of the
/// gathered block into wide memory, then the pooling-class reduction op.
fn possum_stage(e: &mut Emitter, g: &Geom, y: i64, o: i64, wide_addr: i64, q: &Requant) {
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    let relay = |nh: i64| {
        let f = maxpool_relay_n2w_fields(0, 0, y, wide_addr, HW, HW * g.p, g.p, nh, DH);
        if nh * ((g.p + 3) / 4) == 1 {
            f.set("wide_lvl_mask", 0)
        } else {
            f
        }
    };
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| (t % 4 == 0).then(|| relay(ROWS[t / 4])))
        .collect();
    emit_groups(e, &per, encode_narrow_to_wide);
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            (t % 4 == 0).then(|| {
                let mut f = maxpool_op_fields(
                    1,
                    g.p,
                    1,
                    1,
                    ROWS[t / 4],
                    1,
                    DH,
                    [HW, HW],
                    0,
                    0,
                    o,
                    wide_addr,
                    0,
                    q.clamp_min,
                    q.clamp_max,
                );
                f.remove("opcode");
                f.remove("in_zp");
                f.remove("offset");
                f.merge(fields(&[
                    ("w_zp", q.in_zp),
                    ("out_zp", q.out_zp),
                    ("mult_bits", f64_bits(q.mult)),
                ]))
            })
        })
        .collect();
    emit_groups(e, &per, encode_op3);
}

/// After the MUL: the gather onto the column-0 tiles and the SUM over the
/// positions as edgetpu_compiler does it for every P (P = 1: a requantize op).
pub fn possum_any(
    e: &mut Emitter,
    g: &Geom,
    y: i64,
    o: i64,
    wide_addr: i64,
    ident: i64,
    q: &Requant,
) {
    if g.p == 1 {
        e.sync(sc::sync_reset17);
        e.sync(sc::sync_reset17);
        let per: Vec<Option<Fields>> = (0..16)
            .map(|t| {
                (t % 4 == 0)
                    .then(|| requant_op_fields(ROWS[t / 4], HW, HW, HW, 0, 0, y, o, ident, q, 0))
            })
            .collect();
        emit_groups(e, &per, encode_op);
        return;
    }
    gather_west(e, g, y);
    for _ in 0..if g.p >= 4 { 1 } else { 2 } {
        e.sync(sc::sync_reset17);
    }
    possum_stage(e, g, y, o, wide_addr, q);
}

/// SUM / requantize quantization `y = clamp((x - in_zp) mult) + out_zp`.
pub fn sum_quant(in_q: el::Q, out_q: el::Q) -> Requant {
    el::requant_quant(in_q, out_q)
}

/// A reformat copy through the identity row at the top of wide memory: rows x n
/// positions of W words; `in_s` / `out_s` the strides of the in TTU's dims 2, 4, 6
/// and the out TTU's dims 1..3 (words).
pub fn chain_copy(
    src: i64,
    dst: i64,
    w: i64,
    n: i64,
    rows: i64,
    in_s: [i64; 3],
    out_s: [i64; 3],
) -> Fields {
    fields(&[
        ("loop1", w - 1),
        ("loop2", n - 1),
        ("loop3", rows - 1),
        ("in_base", src),
        ("in_mode7", 1),
        ("in_tflags", 0x15),
        ("out_base", dst),
        ("out_mode7", 3),
        ("out_tflags", 3),
        ("par_mode7", 1),
        ("psum_hmode", 1),
        ("psum_tflags", 0x840),
        ("cfg2", 6),
        ("dp_mode", 4),
        ("reduce_mask", 1),
        ("out_ch_last", 3),
        ("out_ch", 3),
    ])
    .merge(ttu(
        "in_",
        &[1, 0, in_s[0], 0, in_s[1], 0, in_s[2]],
        &[w, 1, n, 1, rows],
        8,
        "cnt",
    ))
    .merge(ttu(
        "out_",
        &[1, out_s[0], out_s[1], out_s[2]],
        &[w, n, rows],
        8,
        "cnt",
    ))
    .merge(ttu("par_", &[1], &[1, w, 1, n, rows], 8, "cnt"))
    .merge(ttu("psum_", &[], &[1, w, 1, n, rows], 8, "cnt"))
    .merge(no_requant())
    .merge(wide("par", super::WIDE_TOP - 4))
}

/// SUM over the 48 channels of every (head, position): the packed products stream
/// through the 4-row FIFO, one opcode-3 op writes one word per score at `s_out`.
pub fn chansum_stage(e: &mut Emitter, g: &Geom, y: i64, s_out: i64, fifo: i64, q: &Requant) {
    e.sync(sc::sync_reset17);
    e.sync(sc::sync_reset_par);
    emit_groups(e, &feeds(g, y, fifo), encode_narrow_to_wide);
    let depth = el::mul_fifo_depth(HW);
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            let (nh, cols) = (ROWS[t / 4], g.cols[t % 4]);
            (cols != 0).then(|| {
                fields(&[
                    ("loop0", depth - 1),
                    ("loop1", (HW + depth - 1) / depth - 1),
                    ("loop2", cols - 1),
                    ("loop3", nh - 1),
                    ("out_base", s_out),
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
                    ("w_zp", q.in_zp),
                    ("dp_mode", 1),
                    ("reduce_mask", 1),
                    ("out_ch", 3),
                    ("out_ch_last", 3),
                    ("out_zp", q.out_zp),
                    ("mult_bits", f64_bits(q.mult)),
                    ("clamp_min_bits", f64_bits(q.clamp_min)),
                    ("clamp_max_bits", f64_bits(q.clamp_max)),
                ])
                .merge(ttu_fine(
                    "out",
                    &[4, 4, 4 * cols, 4 * cols * nh],
                    &[1, cols, nh],
                ))
                .merge(ttu_fine(
                    "par",
                    &[4],
                    &[depth, (HW + depth - 1) / depth, cols, nh],
                ))
                .merge(wide("par", fifo))
                .merge(ttu("psum_", &[], &[HW, cols, nh], 8, "cnt"))
            })
        })
        .collect();
    emit_groups(e, &per, encode_op3);
    e.sync(|seq| sc::sync_counters(seq, tc(&["NARROW_TO_WIDE", "PARAMETERS"])));
}

/// The exp2 argument's factor: f32(f32(beta) * log2 e).
pub fn exp2_scale(beta: f64) -> f64 {
    f32r(f32r(beta) * std::f64::consts::LOG2_E)
}

/// The softmax on the scalar core over word layouts: row r's `n` score words at
/// word `src_w + r*m` (the score in the low byte); p goes to word
/// `dst_w + r*m + j` as 4 copies of its byte.
#[expect(clippy::too_many_arguments, reason = "mirrors the compiler's softmax")]
pub fn softmax_words(
    rows: i64,
    n: i64,
    src_w: i64,
    m: i64,
    dst_w: i64,
    zp: i64,
    s_in: f64,
    beta: f64,
    s_out: f64,
    zp_out: i64,
) -> Vec<u128> {
    let k = exp2_scale(beta) as f32;
    let s_in = s_in as f32;
    let deq = |x: i64| {
        let mut w = Vec::new();
        if zp != 0 {
            w.push(s::alu_imm(s::SUB, x, x, zp));
        }
        w.extend([s::i2f(x, x), s::fimm(s::FMUL, x, x, s_in)]);
        w
    };
    // The loaded register is first used 5 bundles later, as in edgetpu_compiler's softmax.
    let score = |i: i64, x: i64| {
        let mut w = vec![s::ld(x, i)];
        w.extend([s::nop(); 4]);
        w.push(s::alu_imm(s::AND, x, x, 0xff));
        w
    };
    let mut w = vec![s::movi(4, src_w), s::movi(5, dst_w)];
    let top = w.len() as i64;
    w.extend([
        s::mov(6, 4),
        s::mov(7, 5),
        s::movi(8, 0xff80_0000),
        s::mov(10, 6),
    ]);
    let mut body = score(10, 9);
    body.extend(deq(9));
    body.extend([s::alu_reg(s::FMAX, 8, 8, 9), s::alu_imm(s::ADD, 10, 10, 1)]);
    w.push(s::repeat(n, body.len()));
    w.extend(body);
    w.extend([s::movi(9, 0), s::mov(11, 6)]);
    let mut body = score(11, 10);
    body.extend(deq(10));
    body.extend([
        s::alu_reg(s::FSUB, 10, 10, 8),
        s::fimm(s::FMUL, 10, 10, k),
        s::sfu(s::EXP2, 10, 10),
        s::alu_imm(s::ADD, 11, 11, 1),
        s::nop(),
        s::alu_reg(s::FADD, 9, 9, 10),
    ]);
    w.push(s::repeat(n, body.len()));
    w.extend(body);
    w.extend([s::mov(17, 6), s::mov(13, 7), s::sfu(s::RECIP, 9, 9)]);
    let mut body = score(17, 10);
    body.extend(deq(10));
    body.extend([
        s::alu_reg(s::FSUB, 10, 10, 8),
        s::fimm(s::FMUL, 10, 10, k),
        s::sfu(s::EXP2, 10, 10),
        s::alu_imm(s::ADD, 17, 17, 1),
        s::nop(),
        s::alu_reg(s::FMUL, 10, 10, 9),
        s::fimm(s::FMUL, 10, 10, el::recip32(s_out) as f32),
    ]);
    if zp_out != 0 {
        body.extend([
            s::f2i(10, 10),
            s::alu_imm(s::ADD, 10, 10, zp_out),
            s::i2f(10, 10),
        ]);
    }
    body.extend([
        s::fimm(s::FMIN, 10, 10, 255.0),
        s::fimm(s::FMAX, 10, 10, 0.0),
        s::f2i(10, 10),
        s::alu_imm(s::SHL, 11, 10, 8),
        s::alu_reg(s::OR, 11, 11, 10),
        s::alu_imm(s::SHL, 12, 11, 16),
        s::alu_reg(s::OR, 11, 11, 12),
        s::st(13, 11),
        s::alu_imm(s::ADD, 13, 13, 1),
    ]);
    w.push(s::repeat(n, body.len()));
    w.extend(body);
    w.extend([
        s::alu_imm(s::ADD, 4, 4, m),
        s::alu_imm(s::ADD, 5, 5, m),
        s::alu_imm(s::LT, 0, 4, src_w + rows * m),
    ]);
    let back = top - w.len() as i64;
    w.push(s::branch(back));
    w
}

/// Every tile row's packed products onto its column-0 tile (meshBus 0x16, west).
pub fn gather_west(e: &mut Emitter, g: &Geom, y: i64) {
    let per: Vec<Option<Fields>> = (0..16)
        .map(|t| {
            let (r, c) = (t / 4, t % 4);
            let nh = ROWS[r];
            let n_own = g.p - g.p0(c);
            let n_in = if c < 3 { g.p - g.p0(c + 1) } else { 0 };
            let mut f = Fields::new();
            if c > 0 && n_own != 0 {
                f = f.merge(half("o_", y, &[(1, HW), (HW, n_own), (HW * n_own, nh)]));
            }
            if n_in != 0 {
                f = f.merge(half(
                    "i_",
                    y + DH * g.cols[c],
                    &[(1, HW), (HW, n_in), (HW * n_own, nh)],
                ));
            }
            if c > 0 && n_own != 0 && n_in != 0 {
                let levels = i64::from(f.get("o_sdims").unwrap_or(0).count_ones());
                f = f.merge(fields(&[
                    ("s0_id", c2::rec_in(MESH_W)),
                    ("s0_val", 4 + levels),
                    ("s0_en_a", 1),
                    ("s0_en_b", 1),
                ]));
            }
            (f != Fields::new()).then_some(f)
        })
        .collect();
    for (mask, f) in c2::group(&per) {
        let seq = e.seq;
        e.tile(rm::encode_mesh(
            &f.set("opcode", i64::from(MESH_W))
                .set("tile_mask", mask)
                .set("seq", seq),
        ));
    }
}

/// Every tile's scores (ROWS[r] x cols(c) words at narrow `s_in`) into scalar
/// memory, row h at word `base_w + h*m`.
pub fn tiles_to_smem(e: &mut Emitter, g: &Geom, s_in: i64, base_w: i64, m: i64, fifo: i64) {
    e.sync(sc::sync_reset17);
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    let mut k = 0;
    for t in 0..16 {
        let (r, c) = (t / 4, t % 4);
        let (nh, cols) = (ROWS[r], g.cols[c]);
        if cols == 0 {
            continue;
        }
        let pk = (4 * nh * cols + 255) / 256;
        assert!(pk <= 2, "scores of one tile take {pk} packets");
        let seq = e.seq;
        e.tile(encode_narrow_to_wide(
            &fields(&[
                ("tile_mask", 1 << t),
                ("seq", seq),
                ("narrow_addr", s_in / 4),
                ("narrow_lvl_mask", 3),
                ("wide_addr", fifo),
                ("tail_lvl", 2),
                ("sync_en1", 0),
                ("sync_en2", 0),
                ("wide_lvl_mask", 1),
            ])
            .merge(wn::ttu("n", &[1, cols], &[cols, nh], 6))
            .merge(wn::ttu("w", &[1], &[pk], 4)),
        ));
        let seq = e.seq;
        e.tile(rm::encode_ring_producer(
            &fields(&[
                ("tile_mask", 1 << t),
                ("seq", seq),
                ("addr", fifo),
                ("sdims", 1),
                ("mode", 1),
                ("pcfg", if pk > 1 { 12 } else { 4 }),
                ("r0_id", (i64::from(pk > 1) << 5) | 0x11),
                ("r0_val", k),
                ("r0_en_a", 1),
                ("r0_en_b", 1),
                ("r1_id", 13),
                ("r1_val", 1),
                ("r1_en_a", 1),
                ("r1_en_b", 1),
                ("dest", 1 << rm::SCALAR_CORE),
            ])
            .merge(ttu("", &[1], &[pk], 4, "cnt")),
        ));
        e.scalar(
            OUTFEED.encode(&fields(&[
                ("base", base_w + HEAD0[r] * m + g.p0(c)),
                ("d0_stride", 1),
                ("d0_limit", cols - 1),
                ("d1_stride", (m - (cols - 1)) & 0xffff),
                ("d1_limit", nh - 1),
                ("d2_stride", -(cols - 1 + (nh - 1) * m) & 0xffff),
                ("d3_stride", 3),
                ("k173", 1),
                ("rsv175", 1),
                ("f222", 0x3f),
            ])),
            0,
        );
        k += 1;
    }
    e.scalar(
        sc::scsync(&Scsync {
            smask: scm(&["RING_OUTFEED"]),
            ..Scsync::default()
        }),
        0,
    );
    let seq = e.seq;
    e.scalar(sc::output_wait(seq, k), 2);
}

/// `rows x row_words` words of scalar memory from word `src_w` into narrow `x` of
/// tile 0 (a wideToNarrow and a ringConsumer on the receiver, one scalar-memory
/// infeed); then a drain and a reset17. The general two-level walk.
pub fn smem_to_tiles(e: &mut Emitter, x: i64, src_w: i64, row_words: i64, rows: i64, fifo: i64) {
    let tiles = 1;
    let n = rows * row_words;
    let c = (4 * n + 255) / 256;
    let nd = fields(&[("narrow_lvl_mask", 3), ("tail_en870", 0), ("tail_f871", 1)]).merge(wn::ttu(
        "n",
        &[1, row_words],
        &[row_words, rows],
        6,
    ));
    let seq = e.seq;
    if c == 1 {
        e.tile(encode_wide_to_narrow(
            &fields(&[
                ("tile_mask", tiles),
                ("seq", seq),
                ("wide_addr", fifo),
                ("wide_circ", 1),
                ("wide_rows", 1),
                ("narrow_addr", x / 4),
                ("sync_wait_lvl", 1),
                ("sync_id", 14),
                ("sync_val", 0x8000),
            ])
            .merge(wn::ttu("w", &[1], &[1], 4))
            .merge(nd),
        ));
        let seq = e.seq;
        e.tile(rm::encode_ring_consumer(
            &fields(&[
                ("tile_mask", tiles),
                ("seq", seq),
                ("addr", fifo),
                ("cbuf", 1),
                ("slots", 1),
                ("mode", 3),
                ("s_id", 43),
                ("s_val", -64),
                ("s_cnt", 1),
                ("s_en_b", 1),
                ("s_y", 1),
            ])
            .merge(ttu("", &[1], &[1], 4, "cnt")),
        ));
    } else {
        e.tile(encode_wide_to_narrow(
            &fields(&[
                ("tile_mask", tiles),
                ("seq", seq),
                ("wide_addr", fifo),
                ("wide_rows", 1),
                ("narrow_addr", x / 4),
                ("sync_id", 14),
                ("sync_val", 0x8000),
            ])
            .merge(wn::ttu("w", &[0], &[c], 4))
            .merge(nd),
        ));
        let seq = e.seq;
        e.tile(rm::encode_ring_consumer(
            &fields(&[
                ("tile_mask", tiles),
                ("seq", seq),
                ("addr", fifo),
                ("slots", 1),
                ("mode", 1),
                ("s_id", 11),
                ("s_val", -64),
                ("s_cnt", 1),
                ("s_en_b", 1),
                ("s_y", 1),
            ])
            .merge(ttu("", &[0], &[c], 4, "cnt")),
        ));
    }
    e.scalar(
        INFEED.encode(&fields(&[
            ("rsv13", 1),
            ("buf_off", src_w),
            ("d0_stride", 1),
            ("d0_limit", n - 1),
            ("d1_stride", -(n - 1) & 0x1f_ffff),
            ("d2_stride", -(n - 1) & 0x1f_ffff),
            ("k157", 1),
            ("tiles", tiles),
            ("k434", 15),
            ("f438", 3),
        ])),
        0,
    );
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    e.sync(sc::sync_reset17);
}

/// The attention core's narrow buffers (bytes, every tile), in address order.
pub fn narrow_sizes(g: &Geom) -> Vec<(&'static str, i64)> {
    let (p, sm) = (g.p, g.smax());
    vec![
        ("Stg", 2 * (DH * p).max(256)),
        ("C", 16),
        ("Q", 2 * DH),
        ("R", 3 * 64),
        ("XK", 96 * sm),
        ("XV", 96 * sm),
        ("Ms", 384 * sm),
        ("Ys", 96 * sm),
        ("S", 8 * sm),
        ("Pn", 24 * p),
        ("Mv", 384 * sm),
        ("Yv", DH * (p + (p + 3) / 4 * 4)),
        ("O", 2 * DH),
    ]
}

/// `[P][288]` rows (position-major, the KV cache) as `[6][P][48]` head-major bytes.
pub fn heads(rows: &[u8], p: usize) -> Vec<u8> {
    let (nh, dh) = (NH, DH as usize);
    let mut out = vec![0u8; rows.len()];
    for pos in 0..p {
        for h in 0..nh {
            let src = &rows[pos * nh * dh + h * dh..pos * nh * dh + (h + 1) * dh];
            out[(h * p + pos) * dh..(h * p + pos + 1) * dh].copy_from_slice(src);
        }
    }
    out
}
