//! A whole TinyStories-15M transformer (6 layers of dim 288, FFN 768, 6 heads of
//! 48) at batch 1 as ONE program per position P: per layer the residual x (uint8)
//! -> L2_NORMALIZATION (the RMSNorm, its weight and sqrt(288) folded into the next
//! matmuls) -> q, k, v (q and k rows in the order SIGMA) -> RoPE (MUL, MUL, ADD with
//! swap(q) read as a half swap of every 16-byte group) -> attention over the host's
//! KV rows and this token's k', v -> o -> ADD (the residual) -> L2_NORMALIZATION ->
//! the FFN (w1, LOGISTIC, MUL, w3, MUL, w2) -> ADD. Byte-identical to tinycoral's
//! `layer.gen_model` in the tile-local layout; every matmul either sits on its
//! compute tiles ("spread") or streams each call from parameter tiles over the ring.

use super::CachingPart;
use super::attention::{
    self as at, DH, DM, Geom, HEAD0, HW, ROWS, SMEM_TOP, input4d, mul_stage as core_mul_stage,
};
use super::block::{
    bias_load, gather_north, output_tile0, rprod_param, scatter_input, slot_copies,
};
use super::conv2d::emit_groups;
use super::eltops::{ROW_FIFO, ROW_FIFO2, l2norm_stage};
use super::fc::{self, FcGeom, Place, caching_part};
use super::{Emitter, WIDE_TOP, caching_program, split_bitstreams};
use crate::backend::coral::executable::{
    Bitstream, Description, Direction, Executable, ExecutableType, Hint, Layer,
};
use crate::backend::coral::isa::eltwise::{
    self as el, ProductQuant, Q, TileGeom, add_op_fields, add_quant, add_weight_fills,
    add_weight_n2w_fields, encode_nlu, ident_prologue, logistic_op_fields, logistic_quant,
    mul_feed_n2w_fields, mul_op1_fields, mul_op2_fields, product_quant, tile_geometry,
};
use crate::backend::coral::isa::op::{FcTile, encode_op, fc_tile_fields, ttu_fine, wide};
use crate::backend::coral::isa::scalar as sc;
use crate::backend::coral::isa::wide_narrow::encode_narrow_to_wide;
use crate::backend::coral::isa::{Fields, cdiv, fields, ring_mesh as rm, ttu};
use crate::backend::coral::{Error, Result};
use std::collections::HashMap;

pub const HD: usize = 768;
pub const LAYERS: usize = 6;
/// Positions the scalar memory holds scores and probabilities for.
pub const MAX_POSITIONS: usize = 256;
/// The 4x4 identity row, for the whole program.
const IDENT: i64 = WIDE_TOP - 4;
/// The FFN broadcasts of its input to tiles 1..11 and of m to tiles 1..4.
const HFIFO: i64 = 0x1f70 - 4;
const MFIFO: i64 = 0x1f70;
/// Streamed FULLY_CONNECTED: the 32-row weight FIFO and its bias rows.
const STREAM_SLOTS: i64 = 32;
const STREAM_FIFO: i64 = 8064;
const STREAM_BIAS: i64 = 8056;

/// The seven matmuls of a layer, in parameter-blob order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mat {
    Q,
    K,
    V,
    Wo,
    W1,
    W3,
    W2,
}

pub const MATS: [Mat; 7] = [Mat::Q, Mat::K, Mat::V, Mat::Wo, Mat::W1, Mat::W3, Mat::W2];

impl Mat {
    pub fn geom(self) -> FcGeom {
        match self {
            Self::Q | Self::K | Self::V | Self::Wo => FcGeom::new(DM as usize, DM as usize),
            Self::W1 | Self::W3 => FcGeom::new(HD, DM as usize),
            Self::W2 => FcGeom::new(DM as usize, HD),
        }
    }

    /// (input, weights, output) quantization keys.
    pub fn keys(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Q => ("x", "wqkv", "qf"),
            Self::K => ("x", "wqkv", "kf"),
            Self::V => ("x", "wqkv", "v"),
            Self::Wo => ("att", "wo", "o"),
            Self::W1 => ("hn", "w1", "h1"),
            Self::W3 => ("hn", "w3", "h3"),
            Self::W2 => ("m", "w2", "y"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Q => "q",
            Self::K => "k",
            Self::V => "v",
            Self::Wo => "wo",
            Self::W1 => "w1",
            Self::W3 => "w3",
            Self::W2 => "w2",
        }
    }

    /// 64-byte units of one compute tile's share.
    fn per_tile(self) -> i64 {
        let g = self.geom();
        (g.param_bytes() / 64 / g.t()) as i64
    }
}

/// A layer's quantization: (scale, zero point) per tensor, and the attention's beta.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerQuant {
    pub q: HashMap<String, Q>,
    pub beta: f64,
}

impl LayerQuant {
    pub fn get(&self, key: &str) -> Q {
        match key {
            // swap(q)*sin' and swap(k)*sin' share the plain products' quantization.
            "qs" => self.get("qc"),
            "ks" => self.get("kc"),
            _ => *self
                .q
                .get(key)
                .unwrap_or_else(|| panic!("layer quantization has no {key:?}")),
        }
    }
}

/// Where a matmul's weights live: on its compute tiles at one offset (64-byte
/// units), or per 64-output group on (tile, offset), streamed every call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Spread(i64),
    Stream(Vec<(i64, i64)>),
}

pub type LayerPlan = HashMap<Mat, Placement>;

/// The narrow buffers (bytes, every tile), in address order.
fn narrow_sizes(g: &Geom, layers: usize) -> Vec<(&'static str, i64)> {
    let core: HashMap<&str, i64> = at::narrow_sizes(g).into_iter().collect();
    let mut v = vec![
        ("Stg", core["Stg"]),
        ("C", 16),
        ("Yo", 320),
        ("Y2", 320),
        ("Xc", 320),
        ("Xs", 320),
        ("R", 192),
        ("Z", 16),
        ("XN", 384),
        ("HN", 384),
        ("Yq", 320),
        ("Yk", 320),
        ("Mr", 256),
        ("QC", 2 * DM),
        ("KC", 2 * DM),
        ("Wm", 64),
        ("Q", 3 * DM * layers as i64),
    ];
    for k in ["XK", "XV", "Ms", "Ys", "S", "Pn", "Mv", "Yv"] {
        v.push((k, core[k]));
    }
    v.extend([
        ("O", 384),
        ("H1", HD as i64),
        ("G", HD as i64),
        ("Af", HD as i64),
        ("H3", HD as i64),
        ("Mm", HD as i64),
        ("H", 320),
        ("XG", 320),
        ("XR", 320),
    ]);
    v
}

/// The memory plan: narrow buffers in order, 64-byte aligned; wide buffers (64-byte
/// units) from the top; the scalar memory's score and probability words.
#[derive(Clone, Debug)]
pub struct Alloc(HashMap<&'static str, i64>);

impl Alloc {
    fn get(&self, key: &str) -> i64 {
        *self
            .0
            .get(key)
            .unwrap_or_else(|| panic!("no buffer {key:?}"))
    }
}

pub fn model_alloc(p: i64, layers: usize) -> Alloc {
    let g = Geom::new(p);
    let mut a = HashMap::new();
    let mut at = 0;
    for (name, size) in narrow_sizes(&g, layers) {
        a.insert(name, at);
        at += (size + 63) / 64 * 64;
    }
    a.insert("narrow_end", at);
    let mut wt = WIDE_TOP;
    for (name, size) in [
        ("const", 4),
        ("outfifo", 4),
        ("unused", 8),
        ("in_kv", 8 * at::g4_of(p).c_in()),
        ("mulfifo", 32),
        ("sumfifo", 16),
        ("smfifo", 8),
        ("pfifo", 4),
        ("bfifo", 4),
        ("relay", 4 * 2 * ((p + 3) / 4)),
        ("addw", 16),
        ("qkvfifo", 4),
        ("ofifo", 4),
        ("xfifo", 4),
    ] {
        wt -= size;
        a.insert(name, wt);
    }
    a.insert("l2fifo", ROW_FIFO);
    a.insert("l2fifo2", ROW_FIFO2);
    a.insert("hfifo", HFIFO);
    a.insert("mfifo", MFIFO);
    a.insert("wide_bottom", wt.min(STREAM_BIAS).min(HFIFO).min(ROW_FIFO2));
    a.insert("M", p);
    let p_smem = SMEM_TOP / 4 - 6 * p;
    a.insert("p_smem_w", p_smem);
    a.insert("s_smem_w", p_smem - 6 * p);
    debug_assert!(at <= 192 * 1024 && p_smem - 6 * p >= 0);
    Alloc(a)
}

/// The lowest wide unit the execution programs write on any tile, P = 1..256.
pub fn model_limit() -> i64 {
    [1, MAX_POSITIONS as i64]
        .iter()
        .map(|&p| model_alloc(p, 1).get("wide_bottom"))
        .min()
        .unwrap_or(0)
}

/// The placement of every matmul of `layers` layers: w1 / w3 first (12 tiles),
/// spread while that fits below the limit, else streamed from the tiles with the
/// most room, group by group.
pub fn plan(layers: usize) -> Result<Vec<LayerPlan>> {
    let mut lim = model_limit();
    lim -= lim % 4;
    let mut fill = [0i64; 16];
    let mut out: Vec<LayerPlan> = (0..layers).map(|_| HashMap::new()).collect();
    let order = (0..layers)
        .flat_map(|l| [(l, Mat::W1)])
        .chain((0..layers).map(|l| (l, Mat::W3)))
        .chain((0..layers).flat_map(|l| {
            [Mat::Q, Mat::K, Mat::V, Mat::Wo, Mat::W2]
                .into_iter()
                .map(move |m| (l, m))
        }));
    let mut streamed = Vec::new();
    for (l, m) in order {
        let (t, u) = (m.geom().t(), m.per_tile());
        let o = fill[..t].iter().copied().max().unwrap_or(0);
        if o + u <= lim {
            out[l].insert(m, Placement::Spread(o));
            fill[..t].fill(o + u);
        } else {
            streamed.push((l, m));
        }
    }
    for (l, m) in streamed {
        let (g, u) = (m.geom(), m.per_tile());
        let mut pieces = Vec::new();
        for _ in 0..g.g() {
            let t = (0..16)
                .min_by_key(|&t| (fill[t], std::cmp::Reverse(t)))
                .unwrap_or(0);
            if fill[t] + u > lim {
                return Err(Error::Codegen(format!(
                    "the parameters of {layers} layers do not fit below {lim} units"
                )));
            }
            pieces.push((t as i64, fill[t]));
            fill[t] += u;
        }
        out[l].insert(m, Placement::Stream(pieces));
    }
    Ok(out)
}

/// The parameter blob's PARAMETER_CACHING: per matmul (in blob order) its
/// ringConsumers, then its infeeds.
pub fn model_caching(
    pl: &[LayerPlan],
) -> (
    Vec<u8>,
    Vec<crate::backend::coral::executable::FieldOffset>,
    usize,
) {
    let mut parts = Vec::new();
    let mut base = 0i64;
    for d in pl {
        for m in MATS {
            let g = m.geom();
            match &d[&m] {
                Placement::Spread(off) => {
                    parts.push(caching_part(&g, &Place::default(), *off, base));
                }
                Placement::Stream(pieces) => {
                    let rows = (g.group_bytes() / 256) as i64;
                    parts.push(
                        pieces
                            .iter()
                            .enumerate()
                            .map(|(i, &(t, o))| CachingPart {
                                tiles: 1 << t,
                                consumer: fields(&[
                                    ("tile_mask", 1 << t),
                                    ("addr", 2 + o),
                                    ("sdims", 1),
                                ])
                                .merge(ttu(
                                    "",
                                    &[1],
                                    &[rows],
                                    4,
                                    "cnt",
                                )),
                                offset: base + (i * g.group_bytes()) as i64,
                                bytes: g.group_bytes() as i64,
                                lanes: 4,
                            })
                            .collect(),
                    );
                }
            }
            base += g.param_bytes() as i64;
        }
    }
    let (bytes, relocs) = caching_program(base, &parts);
    (bytes, relocs, base as usize)
}

// *** stages ***

/// An n-byte vector in the compiler's 1-D layout: per tile (mask, geometry, offset).
fn parts_1d(n: usize) -> Vec<(i64, TileGeom, i64)> {
    (0..cdiv(n, 64))
        .map(|t| (1 << t, tile_geometry(&[1, n], t), 64 * t as i64))
        .collect()
}

fn chunk_tiles(n: usize) -> i64 {
    (1 << cdiv(n, 64)) - 1
}

/// The tile-local layout: (mask, geometry) per run of tiles with equal geometry.
fn parts_local(n: usize) -> Vec<(i64, TileGeom)> {
    let mut out: Vec<(i64, TileGeom)> = Vec::new();
    for (m, tg, _) in parts_1d(n) {
        match out.last_mut() {
            Some((mask, g)) if *g == tg => *mask |= m,
            _ => out.push((m, tg)),
        }
    }
    out
}

fn fc_tile(
    g: &FcGeom,
    t: usize,
    seq: i64,
    x: i64,
    y: i64,
    par_base: i64,
    q: &ProductQuant,
) -> Fields {
    fc_tile_fields(&FcTile {
        n: g.n,
        k: g.k,
        t,
        seq,
        in_base: x,
        out_base: y,
        par_base,
        w_zp: q.b_zp,
        in_zp: q.a_zp,
        mult: q.mult as f32,
        out_zp: q.out_zp,
        clamp_min: Some(q.clamp_min as f32),
        clamp_max: Some(q.clamp_max as f32),
    })
}

/// The compute fence, the bias loads, one FC op per tile group (every tile writes
/// its outputs at `y`), reset17.
fn fc_spread(e: &mut Emitter, g: &FcGeom, x: i64, y: i64, off: i64, q: &ProductQuant) {
    e.sync(sc::signal_fence);
    let mut counts: Vec<usize> = (0..g.t()).map(|t| g.gt(t)).collect();
    counts.sort_unstable();
    counts.dedup();
    for n in counts.into_iter().rev() {
        let tiles: i64 = (0..g.t()).filter(|t| g.gt(*t) == n).map(|t| 1 << t).sum();
        let seq = e.seq;
        e.tile(crate::backend::coral::isa::wide_narrow::w2n_bias(
            tiles, off, n as i64, seq,
        ));
    }
    let per: Vec<Option<Fields>> = (0..g.t())
        .map(|t| {
            let mut f = fc_tile(g, t, 0, x, y, g.bias_rows(t) as i64 + off, q);
            f.remove("tile_mask");
            Some(f)
        })
        .collect();
    emit_groups(e, &per, encode_op);
    e.sync(sc::sync_reset17);
}

/// (fills, rows of the last fill) of a streamed matmul's weight FIFO.
fn fills(kw: i64) -> (i64, i64) {
    let b = (kw + STREAM_SLOTS - 1) / STREAM_SLOTS;
    (b, kw - STREAM_SLOTS * (b - 1))
}

fn stream_op(g: &FcGeom, t: usize, seq: i64, x: i64, y: i64, q: &ProductQuant) -> Result<Fields> {
    let (kw, d) = (g.kw() as i64, STREAM_SLOTS);
    let (b, r) = fills(kw);
    if r != d && r != 8 {
        return Err(Error::Codegen(format!(
            "streamed FC with a partial last FIFO fill of {r} rows"
        )));
    }
    let mut f = fc_tile(g, t, seq, x, y + (64 * g.p() * t) as i64, 0, q)
        .merge(fields(&[("loop0", d - 1), ("loop2", b - 1), ("loop3", 0)]))
        .merge(ttu("in_", &[1, kw, d], &[d, 1, b], 8, "cnt"))
        .merge(ttu("par_", &[1, 0, 0], &[d, 1, b], 8, "cnt"))
        .merge(ttu("psum_", &[0, 1, 0], &[d, 1, b], 8, "cnt"))
        .merge(wide("par", STREAM_FIFO))
        .merge(fields(&[
            ("par_tflags", 0x80),
            ("par_fifo", d),
            ("cfg1", 0x14F),
            ("sync2", 0xB6),
            ("sync3", 0x80),
            ("sync4", 0x80),
            ("psum_hmode", 0),
            ("psum_tflags", 0),
        ]));
    if r != d {
        f = f.merge(fields(&[
            ("rsv174", 0x8000 | (r - 1)),
            ("in_twait", 0x2000),
            ("rsv536", (d - r) << 22 | 1 << 18 | 3),
            ("par_fifo", d | 0x2000),
            ("rsv1230", (d - r) << 18 | 1 << 14 | 3),
            ("rsv1523", (r - 1) << 9 | 1 << 24),
        ]));
    }
    Ok(f)
}

/// Compute fence, drain, per compute tile its weight-FIFO ringConsumer1, the bias
/// load, one op per tile, then group t streams from `pieces[t]` to tile t.
fn fc_streamed(
    e: &mut Emitter,
    g: &FcGeom,
    x: i64,
    y: i64,
    pieces: &[(i64, i64)],
    q: &ProductQuant,
) -> Result<()> {
    let (d, per) = (STREAM_SLOTS, 1 + g.kw() as i64);
    let (b, r) = fills(g.kw() as i64);
    assert!(
        g.p() == 1 && pieces.len() == g.g(),
        "streamed matmul geometry"
    );
    e.sync(sc::signal_fence);
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    for t in 0..g.t() {
        let mut f = fields(&[
            ("opcode", 0x12),
            ("tile_mask", 1 << t),
            ("seq", e.seq),
            ("addr", STREAM_FIFO),
            ("aux_en0", 1),
            ("aux_en1", 1),
            ("s_en_a", 1),
            ("s_en_b", 1),
            ("s_y", 1),
            ("sdims", 1),
            ("cbuf", 1),
            ("mode", 3),
            ("s_id", (1 << 5) | 1),
            ("slots", d),
            ("aux_addr0", STREAM_BIAS + 2),
            ("aux_addr1", STREAM_BIAS + 4),
            ("s_val", -1),
        ])
        .merge(ttu("", &[1, 0, 0, 0], &[d, b, 1, 1], 4, "cnt"));
        if r != d {
            f = f.merge(fields(&[("grp", r - 1), ("gstride", 4 * (d - r) + 1)]));
        }
        e.tile(rm::encode_ring_consumer(&f));
    }
    let seq = e.seq;
    e.tile(bias_load(seq, (1 << g.t()) - 1, STREAM_BIAS, b));
    for t in 0..g.t() {
        let seq = e.seq;
        let f = stream_op(g, t, seq, x, y, q)?.set("out_base", y);
        e.tile(encode_op(&f));
    }
    for (t, &(pt, po)) in pieces.iter().enumerate() {
        let seq = e.seq;
        e.tile(rprod_param(seq, pt, po, per, per * t as i64, 1 << t));
    }
    e.sync(sc::sync_reset17);
    let rows = per * g.g() as i64;
    e.sync(|seq| sc::broadcast_wait(seq, rows));
    Ok(())
}

fn fc_stage(
    e: &mut Emitter,
    g: &FcGeom,
    x: i64,
    y: i64,
    pl: &Placement,
    q: &ProductQuant,
) -> Result<()> {
    match pl {
        Placement::Spread(off) => {
            fc_spread(e, g, x, y, *off, q);
            Ok(())
        }
        Placement::Stream(pieces) => fc_streamed(e, g, x, y, pieces, q),
    }
}

/// MUL step 1's in TTU reading a chunk of `w` words in SWAPS order (the halves of
/// every 16-byte group exchanged), from base + 8.
fn swap_walk(w: i64) -> Fields {
    ttu_fine("in", &[1, -8, 16], &[8, 2, 4 * w / 16])
}

/// out = MUL(a, b) of two n-byte vectors (a and out tile-local, b tile-local or in
/// the 1-D layout): step 1, the FIFO feeds of b, step 2.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the compiler's MUL stage"
)]
fn mul_stage(
    e: &mut Emitter,
    n: usize,
    a: i64,
    b: i64,
    tmp: i64,
    out: i64,
    fifo: i64,
    q: &ProductQuant,
    swap: bool,
    b_global: bool,
) {
    e.sync(sc::sync_reset17);
    e.sync(sc::sync_reset_n2w);
    e.sync(sc::sync_reset_par);
    for (m, tg) in parts_local(n) {
        let seq = e.seq;
        let mut f = mul_op1_fields(tg.w, tg.cols, tg.rows, tg.r, m, seq, a, tmp, fifo, q);
        if swap {
            f = f.merge(swap_walk(tg.w)).set("in_base", a + 8);
        }
        e.tile(encode_op(&f));
    }
    let feeds: Vec<(i64, TileGeom, i64)> = if b_global {
        parts_1d(n)
            .into_iter()
            .map(|(m, tg, o)| (m, tg, b + o))
            .collect()
    } else {
        parts_local(n)
            .into_iter()
            .map(|(m, tg)| (m, tg, b))
            .collect()
    };
    for (m, tg, bb) in feeds {
        let seq = e.seq;
        e.tile(encode_narrow_to_wide(&mul_feed_n2w_fields(
            seq, m, bb, fifo, tg.w, tg.cols, tg.rows,
        )));
    }
    e.sync(sc::sync_wn_fence);
    for (m, tg) in parts_local(n) {
        let seq = e.seq;
        e.tile(encode_op(&mul_op2_fields(
            tg.w, tg.cols, tg.rows, tg.r, m, seq, tmp, out, IDENT,
        )));
    }
    e.sync(sc::sync_reset_n2w);
    e.sync(sc::sync_reset_par);
    e.sync(sc::sync_reset17);
}

/// dst = LOGISTIC(src): the identity row, the op fence, the spline, one op per
/// tile group.
fn logistic_stage(e: &mut Emitter, n: usize, src: i64, dst: i64, in_q: Q, out_q: Q, a: &Alloc) {
    e.sync(sc::sync_reset17);
    let seq = e.seq;
    e.tiles(ident_prologue(seq, 0xffff, a.get("C"), a.get("const"), 0));
    e.sync(sc::sync_op_fence);
    let seq = e.seq;
    e.tile(encode_nlu(chunk_tiles(n), seq));
    let q = logistic_quant(in_q, out_q.1);
    for (m, tg) in parts_local(n) {
        let seq = e.seq;
        e.tile(encode_op(&logistic_op_fields(
            tg.p, tg.w, tg.r[0], tg.r[0], m, seq, src, dst, IDENT, &q, tg.flat,
        )));
    }
    e.sync(sc::sync_reset17);
}

/// out = ADD(a, b): the 16-bit weight matrix by 10 mesh fills and a narrowToWide,
/// one op per tile group (or per tile when operand 2 is in the 1-D layout).
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the compiler's ADD stage"
)]
fn add_stage(
    e: &mut Emitter,
    n: usize,
    a: i64,
    b: i64,
    out: i64,
    wn: i64,
    ww: i64,
    q: &el::AddQuant,
    b_global: bool,
) {
    e.sync(sc::sync_reset17);
    e.sync(sc::sync_reset17);
    let tiles = chunk_tiles(n);
    let seq = e.seq;
    e.tiles(add_weight_fills(seq, tiles, wn, q.w1, q.w2));
    let seq = e.seq;
    e.tile(encode_narrow_to_wide(&add_weight_n2w_fields(
        seq, tiles, wn, ww,
    )));
    assert!(b > a && (b - a) % 4 == 0, "ADD operands at {a} and {b}");
    let per: Vec<(i64, TileGeom, i64)> = if b_global {
        parts_1d(n)
            .into_iter()
            .enumerate()
            .map(|(t, (m, tg, _))| (m, tg, 64 * t as i64))
            .collect()
    } else {
        parts_local(n)
            .into_iter()
            .map(|(m, tg)| (m, tg, 0))
            .collect()
    };
    for (m, tg, bo) in per {
        let seq = e.seq;
        e.tile(encode_op(&add_op_fields(
            tg.w,
            tg.cols,
            tg.rows,
            tg.r,
            (b + bo - a) / 4,
            tg.r,
            m,
            seq,
            a,
            out,
            ww,
            q,
        )));
    }
    e.sync(sc::sync_reset17);
}

/// The 1-D (`local` = false) or tile-local n-byte vector onto tile 0 at `x..x+n`.
fn gather(e: &mut Emitter, n: usize, x: i64, r: i64, local: bool) {
    fc::gather(e, &FcGeom::new(n, n), x, r, &|t| 1 << t, local);
}

/// The n-byte vector at narrow x of tile 0 to the tiles `dest`.
fn bcast(e: &mut Emitter, n: usize, x: i64, dest: i64, fifo: i64) {
    e.scalar(sc::scsync_nop(), 0);
    e.sync(sc::sync_reset17);
    fc::broadcast(e, &FcGeom::new(64, n), x, 1, dest, fifo);
    e.sync(sc::sync_reset17);
}

/// The 4x4 identity row at wide IDENT.
fn ident(e: &mut Emitter, a: &Alloc) {
    let seq = e.seq;
    e.tiles(ident_prologue(seq, 0xffff, a.get("C"), a.get("const"), 0));
    e.sync(sc::sync_reset17);
}

/// The FC quantization of each matmul of a layer.
fn fc_quants(q: &LayerQuant) -> HashMap<Mat, ProductQuant> {
    MATS.iter()
        .map(|&m| {
            let (i, w, o) = m.keys();
            (m, product_quant(q.get(i), q.get(w), q.get(o)))
        })
        .collect()
}

/// Whether the identity row must be loaded again before the next stage that reads
/// it: an L2 norm leaves it unusable on tile 0, a streamed FC on its compute tiles.
struct IdentState {
    dirty: bool,
}

/// Layer l: the residual x (1-D layout at XG for layer 0, tile-local at XR after)
/// -> x_out (tile-local at XR).
fn layer_stages(
    e: &mut Emitter,
    l: usize,
    g: &Geom,
    a: &Alloc,
    q: &LayerQuant,
    pl: &LayerPlan,
    st: &mut IdentState,
) -> Result<()> {
    let p = g.p;
    let first = l == 0;
    let mq = fc_quants(q);
    let x0 = if first { a.get("XG") } else { a.get("XR") };
    let qb = a.get("Q") + 3 * DM * l as i64;
    let use_ident = |e: &mut Emitter, st: &mut IdentState| {
        if st.dirty {
            e.sync(sc::sync_reset17);
            ident(e, a);
            st.dirty = false;
        }
    };
    let fc_run =
        |e: &mut Emitter, st: &mut IdentState, g: FcGeom, x: i64, y: i64, m: Mat| -> Result<()> {
            fc_stage(e, &g, x, y, &pl[&m], &mq[&m])?;
            if matches!(pl[&m], Placement::Stream(_)) {
                st.dirty = true;
            }
            Ok(())
        };
    let l2 = |e: &mut Emitter, st: &mut IdentState, x: i64, y: i64, in_q: Q, out_q: Q| {
        l2norm_stage(
            e,
            DM,
            x,
            y,
            a.get("Z"),
            in_q,
            out_q,
            a.get("l2fifo"),
            a.get("l2fifo2"),
        );
        st.dirty = true;
    };
    let g288 = FcGeom::new(DM as usize, DM as usize);
    let g768 = FcGeom::new(HD, DM as usize);
    let g2 = FcGeom::new(DM as usize, HD);
    // The attention RMSNorm: x onto tile 0, L2_NORMALIZATION at XN, broadcast to tiles 1..4.
    gather(e, DM as usize, x0, a.get("R"), !first);
    l2(e, st, x0, a.get("XN"), q.get("res"), q.get("x"));
    use_ident(e, st);
    bcast(e, DM as usize, a.get("XN"), 0x1e, a.get("xfifo"));
    // This layer's K and V cache rows (their input path copies through the identity row).
    if p > 1 {
        use_ident(e, st);
        for (block, kv) in [("XK", "k"), ("XV", "v")] {
            input4d(
                e,
                p,
                a.get("Stg"),
                a.get(block),
                a.get("C"),
                a.get("const"),
                a.get("in_kv"),
                false,
                &format!("{kv}{l}"),
            );
            e.sync(sc::sync_reset17);
        }
    }
    // q, k, v; RoPE (swap(q) read as q[SWAPS]).
    fc_run(e, st, g288, a.get("XN"), a.get("Yq"), Mat::Q)?;
    fc_run(e, st, g288, a.get("XN"), a.get("Yk"), Mat::K)?;
    fc_run(e, st, g288, a.get("XN"), qb + 2 * DM, Mat::V)?;
    use_ident(e, st);
    let rope = |e: &mut Emitter,
                src: i64,
                prod: i64,
                f: &str,
                c: &str,
                s: &str,
                out_key: &str,
                out: i64| {
        mul_stage(
            e,
            DM as usize,
            src,
            a.get("Xc"),
            a.get("Mr"),
            prod,
            a.get("mulfifo"),
            &product_quant(q.get(f), q.get("cos"), q.get(c)),
            false,
            true,
        );
        mul_stage(
            e,
            DM as usize,
            src,
            a.get("Xs"),
            a.get("Mr"),
            prod + DM,
            a.get("mulfifo"),
            &product_quant(q.get(f), q.get("sin"), q.get(s)),
            true,
            true,
        );
        add_stage(
            e,
            DM as usize,
            prod,
            prod + DM,
            out,
            a.get("Wm"),
            a.get("addw"),
            &add_quant(q.get(c), q.get(s), q.get(out_key)),
            false,
        );
    };
    rope(e, a.get("Yq"), a.get("QC"), "qf", "qc", "qs", "q", qb);
    rope(e, a.get("Yk"), a.get("KC"), "kf", "kc", "ks", "k", qb + DM);
    // q' | k' | v onto tile 0, to every tile that holds positions; k' and v into row P-1.
    for k in 0..3 {
        gather(e, DM as usize, qb + DM * k, a.get("R"), true);
    }
    e.scalar(sc::scsync_nop(), 0);
    e.sync(sc::sync_reset17);
    let dest: i64 = (1..16).filter(|t| g.cols[t % 4] != 0).map(|t| 1 << t).sum();
    if dest != 0 {
        fc::broadcast(
            e,
            &FcGeom::new(64, 3 * DM as usize),
            qb,
            1,
            dest,
            a.get("qkvfifo"),
        );
    }
    e.sync(sc::sync_reset17);
    use_ident(e, st);
    slot_copies(e, g, qb + DM, a.get("XK"));
    slot_copies(e, g, qb + 2 * DM, a.get("XV"));
    e.sync(sc::sync_reset17);
    // The attention core.
    let core = |k: &str| -> Q { q.get(k) };
    let q_bcast = |t: usize| -> at::Walk {
        let rows = ROWS[t / 4];
        (
            qb + DH * HEAD0[t / 4],
            vec![1, 0, DH, DH * rows],
            vec![DH, g.cols[t % 4], rows],
        )
    };
    core_mul_stage(
        e,
        g,
        &q_bcast,
        a.get("XK"),
        a.get("Ms"),
        a.get("Ys"),
        a.get("mulfifo"),
        a.get("const"),
        &product_quant(core("q"), core("k"), core("qk")),
        &|t| HW * g.cols[t % 4],
    );
    at::chansum_stage(
        e,
        g,
        a.get("Ys"),
        a.get("S"),
        a.get("sumfifo"),
        &at::sum_quant(core("qk"), core("s")),
    );
    at::tiles_to_smem(
        e,
        g,
        a.get("S"),
        a.get("s_smem_w"),
        a.get("M"),
        a.get("smfifo"),
    );
    let (s_q, p_q) = (core("s"), core("p"));
    e.scalar(
        at::softmax_words(
            at::NH as i64,
            p,
            a.get("s_smem_w"),
            a.get("M"),
            a.get("p_smem_w"),
            s_q.1,
            crate::backend::coral::isa::f32r(s_q.0),
            q.beta,
            p_q.0,
            p_q.1,
        ),
        0,
    );
    at::smem_to_tiles(
        e,
        a.get("Pn"),
        a.get("p_smem_w"),
        p,
        at::NH as i64,
        a.get("pfifo"),
    );
    if dest != 0 {
        e.scalar(sc::scsync_nop(), 0);
        e.sync(sc::sync_reset17);
        fc::broadcast(
            e,
            &FcGeom::new(64, 24 * p as usize),
            a.get("Pn"),
            1,
            dest,
            a.get("bfifo"),
        );
        e.sync(sc::sync_reset17);
    }
    let pn = a.get("Pn");
    let p_bcast = |t: usize| -> at::Walk {
        let (r, c) = (t / 4, t % 4);
        (
            pn + 4 * (p * HEAD0[r] + g.p0(c)),
            vec![0, 4, 4 * p, 4 * p * ROWS[r]],
            vec![DH, g.cols[c], ROWS[r]],
        )
    };
    core_mul_stage(
        e,
        g,
        &p_bcast,
        a.get("XV"),
        a.get("Mv"),
        a.get("Yv"),
        a.get("mulfifo"),
        a.get("const"),
        &product_quant(core("p"), core("v"), core("pv")),
        &|t| HW * (p - g.p0(t % 4)),
    );
    at::possum_any(
        e,
        g,
        a.get("Yv"),
        a.get("O"),
        a.get("relay"),
        a.get("const"),
        &at::sum_quant(core("pv"), core("att")),
    );
    e.sync(sc::sync_reset17);
    gather_north(e, a.get("O"), a.get("R"));
    bcast(e, DM as usize, a.get("O"), 0x1e, a.get("ofifo"));
    // o = wo(att), h = o + x.
    fc_run(e, st, g288, a.get("O"), a.get("Yo"), Mat::Wo)?;
    add_stage(
        e,
        DM as usize,
        a.get("Yo"),
        x0,
        a.get("H"),
        a.get("Wm"),
        a.get("addw"),
        &add_quant(q.get("o"), q.get("res"), q.get("mid")),
        first,
    );
    // The FFN RMSNorm: h onto tile 0, L2_NORMALIZATION at HN, broadcast to tiles 1..11.
    gather(e, DM as usize, a.get("H"), a.get("R"), true);
    l2(e, st, a.get("H"), a.get("HN"), q.get("mid"), q.get("hn"));
    use_ident(e, st);
    bcast(e, DM as usize, a.get("HN"), 0xffe, a.get("hfifo"));
    // The FFN: h1, g = LOGISTIC(h1), a = h1 g, h3, m = a h3; m onto tile 0, to tiles 1..4; y = w2 m.
    fc_run(e, st, g768, a.get("HN"), a.get("H1"), Mat::W1)?;
    logistic_stage(e, HD, a.get("H1"), a.get("G"), q.get("h1"), q.get("g"), a);
    st.dirty = false;
    use_ident(e, st);
    mul_stage(
        e,
        HD,
        a.get("H1"),
        a.get("G"),
        a.get("Mr"),
        a.get("Af"),
        a.get("mulfifo"),
        &product_quant(q.get("h1"), q.get("g"), q.get("a")),
        false,
        false,
    );
    fc_run(e, st, g768, a.get("HN"), a.get("H3"), Mat::W3)?;
    use_ident(e, st);
    mul_stage(
        e,
        HD,
        a.get("Af"),
        a.get("H3"),
        a.get("Mr"),
        a.get("Mm"),
        a.get("mulfifo"),
        &product_quant(q.get("a"), q.get("h3"), q.get("m")),
        false,
        false,
    );
    gather(e, HD, a.get("Mm"), a.get("R"), true);
    bcast(e, HD, a.get("Mm"), 0x1e, a.get("mfifo"));
    fc_run(e, st, g2, a.get("Mm"), a.get("Y2"), Mat::W2)?;
    // x_out = y + h at XR.
    add_stage(
        e,
        DM as usize,
        a.get("Y2"),
        a.get("H"),
        a.get("XR"),
        a.get("Wm"),
        a.get("addw"),
        &add_quant(q.get("y"), q.get("mid"), q.get("out")),
        false,
    );
    Ok(())
}

/// The PARAMETER_CACHING executable of a plan's parameter blob.
pub fn model_caching_executable(pl: &[LayerPlan]) -> Executable {
    let (bytes, relocs, param_bytes) = model_caching(pl);
    Executable {
        name: "stories15m:caching".into(),
        kind: ExecutableType::ParameterCaching,
        batch_size: 1,
        scratch_size: 0,
        bitstreams: vec![Bitstream {
            data: bytes,
            relocs,
        }],
        parameters: Vec::new(),
        hints: vec![
            Hint::Instruction { chunk: 0 },
            Hint::Dma {
                direction: Direction::Infeed,
                desc: Description::Parameter,
                name: None,
                offset: 0,
                size: param_bytes,
            },
            Hint::Interrupt { id: 0 },
        ],
        inputs: Vec::new(),
        outputs: Vec::new(),
        caching_token: 0,
    }
}

/// The programs of one position count: the caching program (the same for every P)
/// and the execution program, split into bitstreams.
pub struct ModelPrograms {
    pub caching: Executable,
    pub execution: Executable,
}

/// The host contract: inputs in DMA order (x, cos, sin, then per layer k, v for
/// P >= 2), outputs (x_out, then per layer k'|v).
pub fn model_io(p: usize, layers: usize) -> (Vec<Layer>, Vec<Layer>) {
    let dm = DM as usize;
    let mut inputs = vec![
        Layer {
            name: "x".into(),
            size_bytes: dm,
        },
        Layer {
            name: "cos".into(),
            size_bytes: dm,
        },
        Layer {
            name: "sin".into(),
            size_bytes: dm,
        },
    ];
    if p > 1 {
        for l in 0..layers {
            for kv in ["k", "v"] {
                inputs.push(Layer {
                    name: format!("{kv}{l}"),
                    size_bytes: dm * p,
                });
            }
        }
    }
    let mut outputs = vec![Layer {
        name: "out".into(),
        size_bytes: dm,
    }];
    outputs.extend((0..layers).map(|l| Layer {
        name: format!("kv{l}"),
        size_bytes: 2 * dm,
    }));
    (inputs, outputs)
}

/// The EXECUTION_ONLY executable of `layers` layers at P positions from its
/// bitstreams.
pub fn model_execution(p: usize, layers: usize, bitstreams: Vec<Bitstream>) -> Executable {
    let (inputs, outputs) = model_io(p, layers);
    let mut hints: Vec<Hint> = (0..bitstreams.len())
        .map(|chunk| Hint::Instruction { chunk })
        .collect();
    hints.push(Hint::Interrupt { id: 0 });
    Executable {
        name: format!("stories15m_p{p}:execution"),
        kind: ExecutableType::ExecutionOnly,
        batch_size: 1,
        scratch_size: 0,
        bitstreams,
        parameters: Vec::new(),
        hints,
        inputs,
        outputs,
        caching_token: 0,
    }
}

/// The caching and execution programs of `quants.len()` layers at P positions (the
/// new token at P-1). `quants[l]["out"]` must equal `quants[l+1]["res"]`.
pub fn gen_model(p: usize, quants: &[LayerQuant], pl: &[LayerPlan]) -> Result<ModelPrograms> {
    if !(1..=MAX_POSITIONS).contains(&p) {
        return Err(Error::Codegen(format!(
            "P = {p} is outside 1..={MAX_POSITIONS}"
        )));
    }
    let layers = quants.len();
    if pl.len() != layers {
        return Err(Error::Codegen(format!(
            "{} layer plans for {layers} layers",
            pl.len()
        )));
    }
    for l in 1..layers {
        if quants[l].get("res") != quants[l - 1].get("out") {
            return Err(Error::Codegen(format!(
                "layer {l}: the residual quantization does not chain"
            )));
        }
    }
    let (g, a) = (Geom::new(p as i64), model_alloc(p as i64, layers));
    let mut e = Emitter::new();
    e.scalar(sc::exe_prologue(), 1);
    scatter_input(&mut e, a.get("XG"), DM as usize, "x");
    scatter_input(&mut e, a.get("Xc"), DM as usize, "cos");
    scatter_input(&mut e, a.get("Xs"), DM as usize, "sin");
    ident(&mut e, &a);
    let mut st = IdentState { dirty: false };
    for l in 0..layers {
        layer_stages(&mut e, l, &g, &a, &quants[l], &pl[l], &mut st)?;
    }
    gather(&mut e, DM as usize, a.get("XR"), a.get("R"), true);
    e.sync(sc::sync_reset17);
    output_tile0(&mut e, a.get("XR"), DM as usize, "out");
    for l in 0..layers {
        output_tile0(
            &mut e,
            a.get("Q") + 3 * DM * l as i64 + DM,
            2 * DM as usize,
            &format!("kv{l}"),
        );
    }
    e.sync(|seq| sc::epilogue(seq, false));
    let bitstreams = split_bitstreams(&e);
    let execution = model_execution(p, layers, bitstreams);
    let caching = model_caching_executable(pl);
    Ok(ModelPrograms { caching, execution })
}

/// The quantization as JSON: per layer `{tensor: [scale, zero point], beta}`.
pub fn quants_json(quants: &[LayerQuant]) -> serde_json::Value {
    serde_json::Value::Array(
        quants
            .iter()
            .map(|q| {
                // Sorted, so equal quantizations serialize equally.
                let mut keys: Vec<&String> = q.q.keys().collect();
                keys.sort();
                let mut m: serde_json::Map<String, serde_json::Value> = keys
                    .into_iter()
                    .map(|k| (k.clone(), serde_json::json!([q.q[k].0, q.q[k].1])))
                    .collect();
                m.insert("beta".into(), serde_json::json!(q.beta));
                serde_json::Value::Object(m)
            })
            .collect(),
    )
}

pub fn quants_from_json(v: &serde_json::Value) -> Option<Vec<LayerQuant>> {
    v.as_array()?
        .iter()
        .map(|d| {
            let mut q = HashMap::new();
            let mut beta = None;
            for (k, x) in d.as_object()? {
                if k == "beta" {
                    beta = x.as_f64();
                } else {
                    let a = x.as_array()?;
                    q.insert(k.clone(), (a.first()?.as_f64()?, a.get(1)?.as_i64()?));
                }
            }
            Some(LayerQuant { q, beta: beta? })
        })
        .collect()
}
