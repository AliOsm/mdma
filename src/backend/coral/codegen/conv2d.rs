//! The part of edgetpu_compiler's CONV_2D layout the attention core reuses: the
//! image geometry split over the 4x4 tiles and the input path that brings each
//! tile's block of a `[1, H, W, C]` input from one host DMA into narrow memory
//! (ring stream per tile row, wideToNarrow chunks into two staging slots, copy
//! ops through the identity row), and the mesh-move halves.

use super::Emitter;
use crate::backend::coral::isa::eltwise::tile_split;
use crate::backend::coral::isa::op::{no_requant, wide};
use crate::backend::coral::isa::scalar::{self as sc, INFEED, tile_counter};
use crate::backend::coral::isa::wide_narrow as wn;
use crate::backend::coral::isa::{Fields, fields, ttu};

pub const MESH_N: u8 = 0x17;
pub const MESH_S: u8 = 0x15;
pub const MESH_W: u8 = 0x16;
pub const MESH_E: u8 = 0x18;

/// The receiving side's sync record id of a mesh move in direction `op`.
pub fn rec_in(op: u8) -> i64 {
    let counter = match op {
        MESH_N => "MESH_SOUTH_IN",
        MESH_S => "MESH_NORTH_IN",
        MESH_W => "MESH_EAST_IN",
        _ => "MESH_WEST_IN",
    };
    (tile_counter(counter) << 1) as i64
}

/// One tile row (column): its own input rows `[i0, i0+ni)`, its outputs
/// `[o0, o0+no)`, the input window `[w0, w0+nw)` they need, and `[lo, hi)`, the
/// image rows it holds once the halos arrived. The tile stores `[b0, b0+nb)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub i0: i64,
    pub ni: i64,
    pub o0: i64,
    pub no: i64,
    pub w0: i64,
    pub nw: i64,
    pub lo: i64,
    pub hi: i64,
}

impl Span {
    pub fn b0(&self) -> i64 {
        if self.no != 0 {
            self.w0.min(self.lo)
        } else {
            self.lo
        }
    }

    pub fn nb(&self) -> i64 {
        let end = if self.no != 0 {
            (self.w0 + self.nw).max(self.hi)
        } else {
            self.hi
        };
        end - self.b0()
    }

    /// Own rows start this far into the block.
    pub fn d(&self) -> i64 {
        self.i0 - self.b0()
    }
}

/// The 4 tile spans of one image axis.
pub fn axis(d: i64, od: i64, k: i64, s: i64, pad: i64) -> [Span; 4] {
    let ins = tile_split(d as usize);
    let outs = tile_split(od as usize);
    let (mut i0, mut o0) = (0, 0);
    let mut sp = [Span {
        i0: 0,
        ni: 0,
        o0: 0,
        no: 0,
        w0: 0,
        nw: 0,
        lo: 0,
        hi: 0,
    }; 4];
    for p in 0..4 {
        let (ni, no) = (ins[p] as i64, outs[p] as i64);
        sp[p] = Span {
            i0,
            ni,
            o0,
            no,
            w0: o0 * s - pad,
            nw: if no != 0 { (no - 1) * s + k } else { 0 },
            lo: 0,
            hi: 0,
        };
        i0 += ni;
        o0 += no;
    }
    let mut hi = [0; 4];
    for p in 0..4 {
        let x = sp[p];
        let window = if x.no != 0 { (x.w0 + x.nw).min(d) } else { 0 };
        let prev = if p > 0 { hi[p - 1] } else { 0 };
        hi[p] = (x.i0 + x.ni).max(window).max(prev);
    }
    let mut lo = [0; 4];
    for p in (0..4).rev() {
        let x = sp[p];
        let window = if x.no != 0 { x.w0.max(0) } else { d };
        let next = if p < 3 { lo[p + 1] } else { d };
        lo[p] = x.i0.min(window).min(next);
    }
    for p in 0..4 {
        sp[p].lo = lo[p];
        sp[p].hi = hi[p];
    }
    sp
}

/// A VALID 1x1-kernel image `[1, H, W, C]` as the conv input path sees it.
#[derive(Clone, Debug)]
pub struct Conv2DGeom {
    pub h: i64,
    pub w: i64,
    pub cin: i64,
    pub rows: [Span; 4],
    pub cols: [Span; 4],
}

impl Conv2DGeom {
    pub fn new(h: i64, w: i64, cin: i64) -> Self {
        Self {
            h,
            w,
            cin,
            rows: axis(h, h, 1, 1, 0),
            cols: axis(w, w, 1, 1, 0),
        }
    }

    /// Bytes per window position (channels padded to a word).
    pub fn c4(&self) -> i64 {
        (self.cin + 3) / 4 * 4
    }

    pub fn cw(&self) -> i64 {
        self.c4() / 4
    }

    /// Bytes of one image row in the input stream.
    pub fn r(&self) -> i64 {
        self.w * self.cin
    }

    /// Input DMA bytes.
    pub fn s(&self) -> i64 {
        8 * ((self.h * self.r() + 7) / 8)
    }

    /// Input ring FIFO slots.
    pub fn c_in(&self) -> i64 {
        (self.r() + 255) / 256
    }

    fn rows_per_word(&self) -> i64 {
        4 / gcd(self.r(), 4)
    }

    /// Image rows per input chunk of row group `r`.
    pub fn m(&self, r: usize) -> i64 {
        self.rows_per_word().min(self.rows[r].ni)
    }

    /// Words per input chunk.
    pub fn l(&self, r: usize) -> i64 {
        (self.m(r) * self.r() + 3) / 4
    }

    /// Words of a full chunk.
    pub fn lf(&self) -> i64 {
        self.rows_per_word() * self.r() / 4
    }

    /// The narrow staging slot (two of them): one full chunk.
    pub fn slot(&self) -> i64 {
        (4 * self.lf()).max(256)
    }

    /// (chunks, rows of the last chunk) of tile row `r`'s own image rows.
    pub fn chunks(&self, r: usize) -> (i64, i64) {
        let m = self.m(r);
        let n = (self.rows[r].ni + m - 1) / m;
        (n, self.rows[r].ni - m * (n - 1))
    }
}

const fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs()
}

/// Tiles whose instructions are identical share one (OR-ed tile mask), in the
/// order of their lowest tile.
pub fn group(per_tile: &[Option<Fields>]) -> Vec<(i64, Fields)> {
    let mut groups: Vec<(i64, Fields)> = Vec::new();
    for (t, f) in per_tile.iter().enumerate() {
        let Some(f) = f else { continue };
        match groups.iter_mut().find(|(_, g)| g == f) {
            Some((mask, _)) => *mask |= 1 << t,
            None => groups.push((1 << t, f.clone())),
        }
    }
    groups
}

/// Drops the (stride, count) levels of count 1.
pub fn dims(levels: &[(i64, i64)]) -> Vec<(i64, i64)> {
    levels.iter().copied().filter(|(_, c)| *c > 1).collect()
}

/// Byte offset of tile (r, c)'s first position in its row group's ring stream.
fn stream_off(g: &Conv2DGeom, r: usize, c: usize) -> i64 {
    g.rows[r].i0 * g.r() % 8 + g.cols[c].i0 * g.cin
}

/// (chunks, in_base, out_base, words % 4, sync fields) of a copy op.
fn copy_common(
    g: &Conv2DGeom,
    r: usize,
    c: usize,
    stg: i64,
    x: i64,
    last: bool,
) -> (i64, i64, i64, i64, Fields) {
    let (rs, cs) = (g.rows[r], g.cols[c]);
    let (n, rl) = g.chunks(r);
    let ph = stream_off(g, r, c) % 4;
    let (m, l) = (g.m(r), g.l(r));
    let nch = if last { 1 } else { n - 1 };
    let wrow = rs.d() + if last { m * (n - 1) } else { 0 };
    let in_base = if last {
        stg + ((n - 1) % 2) * g.slot()
    } else {
        stg
    } + ph;
    let out_base = x + (wrow * cs.nb() + cs.d()) * g.c4();
    let words = if last {
        (n - 1) * l + ((rl - 1) * g.r() + ph + cs.ni * g.cin + 3) / 4
    } else {
        l
    };
    let sync = fields(&[
        ("sync0", ((g.lf() % 4) << 14) | (words / 4)),
        ("sync1", 0x4000 + g.lf() / 4),
        ("rsv515", (2 * g.slot()) & 127),
        ("in_twait", (2 * g.slot()) >> 7),
    ]);
    (nch, in_base, out_base, words % 4, sync)
}

/// C % 4 == 0 (one image row per chunk): the tile's positions, word by word,
/// through the identity row.
pub fn copy_op(
    g: &Conv2DGeom,
    r: usize,
    c: usize,
    stg: i64,
    x: i64,
    ident: i64,
    last: bool,
) -> Fields {
    let (cs, cw) = (g.cols[c], g.cw());
    let (nch, in_base, out_base, rem, sync) = copy_common(g, r, c, stg, x, last);
    fields(&[
        ("loop1", cw - 1),
        ("loop2", cs.ni - 1),
        ("loop3", nch - 1),
        ("in_base", in_base),
        ("in_mode7", 3),
        ("in_tflags", 0x80),
        ("out_base", out_base),
        ("out_mode7", 3),
        ("out_tflags", 3),
    ])
    .merge(ttu("in_", &[1, cw, 0], &[cw, cs.ni, nch], 8, "cnt"))
    .merge(ttu(
        "out_",
        &[1, 1, cw, cs.nb() * cw],
        &[1, cw, cs.ni, nch],
        8,
        "cnt",
    ))
    .merge(sync)
    .merge(wide("par", ident))
    .merge(ttu("par_", &[], &[cw, 1, cs.ni, nch], 8, "cnt"))
    .merge(ttu("psum_", &[], &[cw, 1, cs.ni, nch], 8, "cnt"))
    .merge(fields(&[
        ("par_hmode", i64::from(cw == 1)),
        ("psum_hmode", i64::from(cw == 1)),
        ("psum_tflags", 0x800),
        ("cfg0", 7),
        ("cfg1", 0x6B | rem << 8),
        ("cfg2", 6),
        ("dp_mode", 4),
        ("out_ch", 3),
        ("out_ch_last", 3),
    ]))
    .merge(no_requant())
}

/// (first byte, bytes) of row group `r`'s input stream.
pub fn row_stream(g: &Conv2DGeom, r: usize) -> (i64, i64) {
    (g.rows[r].i0 * g.r(), g.rows[r].ni * g.r())
}

/// (FIFO passes, packets, grp, gstride) of row group `r`'s stream through the
/// `c_in`-slot ring FIFO.
pub fn ring_passes(g: &Conv2DGeom, r: usize) -> (i64, i64, i64, i64) {
    let (lo, n) = row_stream(g, r);
    let packets = (8 * ((lo + n + 7) / 8 - lo / 8) + 255) / 256;
    let refills = (packets + g.c_in() - 1) / g.c_in();
    let p_last = packets - g.c_in() * (refills - 1);
    let (grp, gstride) = if p_last < g.c_in() {
        (p_last - 1, 4 * (g.c_in() - p_last) + 1)
    } else {
        (0, 0)
    };
    (refills, packets, grp, gstride)
}

/// Tile (r, c)'s wideToNarrow of its part of the row group's stream into the
/// staging slots.
pub fn w2n_input(g: &Conv2DGeom, r: usize, c: usize, stg: i64, in_fifo: i64) -> Fields {
    let (refills, packets, grp, gstride) = ring_passes(g, r);
    let sk = stream_off(g, r, c) / 4;
    let (mut l, rem, n) = (g.l(r), 64 * packets - sk, g.chunks(r).0);
    let (nch, head) = if rem <= l {
        l = rem;
        (1, None)
    } else if (rem + l - 1) / l > n + 1 {
        let nch = n + 1;
        let mut head = rem - (nch - 1) * l;
        if head % l == 0 {
            head -= l;
        }
        (nch, Some(head))
    } else {
        let nch = (rem + l - 1) / l;
        let head = rem - (nch - 1) * l;
        (nch, (head != l).then_some(head))
    };
    let staging = (4 * g.l(r)).max(256) / 2;
    let mut f = fields(&[
        ("wide_addr", in_fifo),
        ("wide_lvl_mask", 1),
        ("wide_circ", 1),
        ("wide_rows", g.c_in()),
        ("rsv227", (grp << 6) | (gstride << 22)),
        ("narrow_addr", stg / 4),
        ("narrow_lvl_mask", 1),
        ("mode", 1),
        ("sync_id", 14),
        ("sync_wait_lvl", 1),
        ("sync_val", 0x8000),
        ("sync_dec_lvl", 1),
        ("sync_dec", -1),
        ("sync_dec_mode", 3),
        ("tail_f871", 1),
        ("rsv489", staging & 31),
        ("size64", staging >> 5),
    ])
    .merge(wn::ttu("w", &[1, 0], &[g.c_in(), refills], 4))
    .merge(wn::ttu("n", &[1, 0], &[l, nch], 6));
    if let Some(head) = head {
        let t = (4 * (l - head)) & 0x3ffff;
        f = f.merge(fields(&[
            ("head_words_m1", head - 1),
            ("head_en", 1),
            ("rsv524", t & 63),
            ("head_tail16", (t >> 6) & 15),
            ("rsv534", t >> 10),
        ]));
    }
    if sk != 0 {
        f = f.merge(fields(&[
            ("skip_en0", 1),
            ("skip_en1", 1),
            ("skip_base", stg / 4),
            ("skip_m1", sk - 1),
            ("skip_end", stg / 4 + sk - 1),
        ]));
    }
    let _ = packets;
    f
}

/// The ringConsumer of row group `r`'s stream.
pub fn rc_input(g: &Conv2DGeom, r: usize, seq: i64, in_fifo: i64) -> Fields {
    let (refills, _, grp, gstride) = ring_passes(g, r);
    let f = if g.c_in() > 1 {
        fields(&[
            ("sdims", 1),
            ("cbuf", 1),
            ("mode", 3),
            ("s_id", (1 << 5) | 0xb),
        ])
        .merge(ttu("", &[1, 0], &[g.c_in(), refills], 4, "cnt"))
    } else if refills == 1 {
        fields(&[("cbuf", 1), ("mode", 3), ("s_id", (1 << 5) | 0xb)]).merge(ttu(
            "",
            &[1],
            &[1],
            4,
            "cnt",
        ))
    } else {
        fields(&[("mode", 1), ("s_id", 11)]).merge(ttu("", &[0], &[refills], 4, "cnt"))
    };
    fields(&[
        ("tile_mask", 0xf << (4 * r)),
        ("seq", seq),
        ("addr", in_fifo),
        ("slots", g.c_in()),
        ("grp", grp),
        ("gstride", gstride),
        ("s_val", -64 * g.c_in()),
        ("s_cnt", g.c_in()),
        ("s_en_b", 1),
        ("s_y", 1),
    ])
    .merge(f)
}

/// `scalar::av_infeed` with the staging-buffer offset wrapped at 2^15 units also
/// for single-pass transfers, and a last pass that fills the whole buffer encoded
/// as `count_m1 = k203 = 0`.
pub fn av_infeed(offset: i64, nbytes: i64, tiles: i64) -> Vec<u128> {
    let mut f = INFEED.decode(&sc::av_infeed(offset, nbytes, tiles));
    f.put("buf_off", offset / 8 % (1 << 15));
    if nbytes / 8 > 1 && nbytes / 8 % sc::AV_BUF == 0 {
        f.put("count_m1", 0);
        f.put("k203", 0);
    }
    INFEED.encode(&f)
}

/// One half (`o_` / `i_`) of a mesh move: `[(1, words of a position), (stride,
/// positions), (stride, rows)]` with count-1 levels dropped. A level moves at most
/// 16 words: wider positions go in 16-word pieces over an outermost level.
pub fn half(p: &str, addr: i64, levels: &[(i64, i64)]) -> Fields {
    let cw = levels[0].1;
    let pieces = (cw + 15) / 16;
    let mut all = vec![(1, cw.min(16))];
    all.extend_from_slice(&levels[1..]);
    let mut lv = dims(&all);
    if pieces > 1 {
        lv.push((16, pieces));
    }
    let strides: Vec<i64> = lv.iter().map(|l| l.0).collect();
    let counts: Vec<i64> = lv.iter().map(|l| l.1).collect();
    let mut f = fields(&[
        (&format!("{p}addr"), addr),
        (&format!("{p}sdims"), (1 << lv.len()) - 1),
    ])
    .merge(ttu(p, &strides, &counts, 4, "cnt"));
    if pieces > 1 && cw % 16 != 0 {
        f.put(&format!("{p}grp"), cw % 16 - 1);
        let key = if p == "o_" { "rsv243" } else { "rsv448" };
        f.put(key, (4 * (16 - cw % 16) + lv.len() as i64 - 1) << 6);
    }
    if p == "i_" {
        f.put("in_mode", 2 * (lv.len() as i64 - i64::from(pieces > 1)) + 1);
    }
    f
}

/// Fields that keep the tiles of different block shapes apart in `group` (the
/// compiler emits one instruction per shape even when two encode the same); never
/// encoded.
pub const SHAPE: [&str; 2] = ["shape_rows", "shape_cols"];

/// Emits one instruction per group of identical per-tile fields.
pub fn emit_groups(e: &mut Emitter, per: &[Option<Fields>], encode: fn(&Fields) -> Vec<u128>) {
    for (mask, mut f) in group(per) {
        for key in SHAPE {
            f.remove(key);
        }
        let seq = e.seq;
        e.tile(encode(&f.set("tile_mask", mask).set("seq", seq)));
    }
}
