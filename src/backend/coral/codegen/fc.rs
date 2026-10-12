//! FULLY_CONNECTED code generation, `y[N] = W[N,K] x[K]` (uint8), byte-identical to
//! edgetpu_compiler for `tile_shift = 0`, plus
//! the host side: parameter blob layout, input padding and the bit-exact reference.

use super::{CachingPart, Emitter, Quant, WIDE_OUT_FIFO, WIDE_TOP, caching_program};
use crate::backend::coral::executable::{
    Bitstream, Description, Direction, Executable, ExecutableType, Hint, Layer,
};
use crate::backend::coral::isa::scalar::{self as sc, mesh_counters, tile_counter};
use crate::backend::coral::isa::{
    Fields, cdiv, cdiv_i64, fields, op, ring_mesh as rm, round_up, ttu, wide_narrow as wn,
};
use crate::backend::coral::{Error, Result};

/// 64-byte units: ring broadcast of x from the gather tile to the other compute tiles.
pub const WIDE_FWD_FIFO: i64 = 0x1f70;
/// Parameter bytes per tile that stay below every program's own wide-memory buffers.
pub const WIDE_BYTES: usize = 496_640;
/// Ring sync-record ids `op<<5 | tile SyncCounter`: NARROW_TO_WIDE/op1, WIDE_TO_NARROW/op1, WIDE_TO_NARROW/op0.
const R_N2W: i64 = (1 << 5) | 0x0d;
const R_W2N_1: i64 = (1 << 5) | 0x0b;
const R_W2N_0: i64 = 0x0b;

/// Tile geometry of `y[N] = W[N,K] x[K]` as edgetpu_compiler lays it out.
/// `bias = false`: no bias rows (the compiler's form for N=1 with an all-zero bias).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FcGeom {
    pub n: usize,
    pub k: usize,
    pub bias: bool,
}

impl FcGeom {
    pub const fn new(n: usize, k: usize) -> Self {
        Self { n, k, bias: true }
    }

    /// 64-output groups.
    pub const fn g(&self) -> usize {
        cdiv(self.n, 64)
    }
    /// Groups ("passes") per compute tile; the last tile may have fewer.
    pub const fn p(&self) -> usize {
        cdiv(self.g(), 16)
    }
    /// Compute tiles.
    pub const fn t(&self) -> usize {
        cdiv(self.g(), self.p())
    }
    /// Groups of compute tile t.
    pub fn gt(&self, t: usize) -> usize {
        self.p().min(self.g() - self.p() * t)
    }
    /// Outputs of compute tile t.
    pub fn n_out(&self, t: usize) -> usize {
        (64 * self.p()).min(self.n - 64 * self.p() * t)
    }
    /// Input DMA bytes.
    pub const fn s(&self) -> usize {
        8 * cdiv(self.k, 8)
    }
    /// Output DMA bytes.
    pub const fn out_bytes(&self) -> usize {
        8 * cdiv(self.n, 8)
    }
    /// Input bytes per input tile.
    pub const fn b(&self) -> usize {
        64 * cdiv(self.s(), 1024)
    }
    /// Input tiles (x arrives in pieces of b bytes).
    pub const fn t_in(&self) -> usize {
        cdiv(self.s(), self.b())
    }
    /// Bytes of x on input tile t.
    pub fn piece(&self, t: usize) -> usize {
        self.b().min(self.k - self.b() * t)
    }
    /// 4-byte input words = reduction steps.
    pub const fn kw(&self) -> usize {
        cdiv(self.k, 4)
    }
    /// Outputs per parameter group.
    pub const fn group_out(&self) -> usize {
        if self.n <= 64 {
            round_up(self.n, 16)
        } else {
            64
        }
    }
    /// int32 bias + `[K/4][g][4]` weights.
    pub const fn group_bytes(&self) -> usize {
        4 * self.group_out() * (self.bias as usize) + self.group_out() * 4 * self.kw()
    }
    pub const fn param_bytes(&self) -> usize {
        self.g() * self.group_bytes()
    }
    /// 64-byte units of bias in front of tile t's weights.
    pub fn bias_rows(&self, t: usize) -> usize {
        4 * self.gt(t) * usize::from(self.bias)
    }
}

/// edgetpu_compiler's narrow placement (fitted on ~1450 compiles) -> byte addresses
/// `(X, Y, R)` of x, y and the 32-byte mesh relay buffer R (only when x is gathered
/// from several input tiles). Sizes are rounded up to 4 bytes; with
/// `M = 256*ceil(K/256) + 512`:
///   x first  `[x | y | R]`          unless K <= 188 or N4 + 32 >= M      (N4 = N rounded up to 4)
///   y first  `[y | R | .. | x]`     x at max(M, N4 + |R|)  when N4 < M
///            `[y | x | R]`          when N4 >= M
/// A one-word y (N <= 4) goes after R instead of before it.
pub fn narrow_layout(n: usize, k: usize) -> (i64, i64, i64) {
    let m = 256 * cdiv(k, 256) + 512;
    let rsz = if FcGeom::new(n, k).t_in() > 1 { 32 } else { 0 };
    let n4 = round_up(n, 4);
    let y_r = |base: usize| {
        if n <= 4 {
            (base + rsz, base)
        } else {
            (base, base + n4)
        }
    };
    let (x, (y, r)) = if !(k <= 188 || n4 + 32 >= m) {
        (0, y_r(round_up(k, 4)))
    } else if n4 < m {
        (m.max(n4 + rsz), y_r(0))
    } else {
        (n4, (0, n4 + round_up(k, 4)))
    };
    (x as i64, y as i64, r as i64)
}

// *** ring DMAs (ring_mesh.md sections 2-3) ***

/// ringConsumer0 of input row group j (tiles 4j..4j+3), fed by one infeed that
/// multicasts its 256-byte packets. c = packets of the whole input, p = packets of
/// this group; the consumer skips the other groups' 4*(c-p) 64-byte lanes.
fn rcons_input(g: &FcGeom, j: usize, tiles: i64, seq: i64) -> Vec<u128> {
    let c = cdiv(g.s(), 256) as i64;
    let p = cdiv((4 * g.b()).min(g.s() - 4 * g.b() * j), 256) as i64;
    rm::encode_ring_consumer(
        &fields(&[
            ("tile_mask", tiles),
            ("seq", seq),
            ("addr", WIDE_TOP - 8 * c),
            ("sdims", i64::from(c > 1)),
            ("cbuf", 1),
            ("slots", c),
            ("grp", p - 1),
            ("gstride", if p > 1 { 4 * (c - p) + 1 } else { 0 }),
            ("mode", 3),
            ("s_id", R_W2N_1),
            ("s_val", -64 * c),
            ("s_cnt", c),
            ("s_en_b", 1),
            ("s_y", 1),
        ])
        .merge(ttu("", &[1], &[if p == 1 { 1 } else { c }], 4, "cnt")),
    )
}

/// ringConsumer0 on the compute tiles that receive x from the gather tile (single-slot FIFO, c packets).
fn rcons_forward(g: &FcGeom, tiles: i64, seq: i64, fifo: i64) -> Vec<u128> {
    let c = cdiv(g.k, 256) as i64;
    let f = if c == 1 {
        fields(&[("cbuf", 1), ("mode", 3), ("s_id", R_W2N_1)]).merge(ttu("", &[1], &[1], 4, "cnt"))
    } else {
        fields(&[("cbuf", 0), ("mode", 1), ("s_id", R_W2N_0)]).merge(ttu("", &[0], &[c], 4, "cnt"))
    };
    rm::encode_ring_consumer(
        &fields(&[
            ("tile_mask", tiles),
            ("seq", seq),
            ("addr", fifo),
            ("slots", 1),
            ("s_val", -64),
            ("s_cnt", 1),
            ("s_en_b", 1),
            ("s_y", 1),
        ])
        .merge(f),
    )
}

/// ringProducer on the gather tile: multicast x (c packets through the single-slot FIFO) to `dest`.
fn rprod_forward(g: &FcGeom, tile: i64, dest: i64, seq: i64, fifo: i64) -> Vec<u128> {
    rm::encode_ring_producer(
        &fields(&[
            ("tile_mask", tile),
            ("seq", seq),
            ("addr", fifo),
            ("sdims", 1),
            ("cbuf", 1),
            ("slots", 1),
            ("mode", 1),
            ("r0_id", R_N2W),
            ("r0_val", 1),
            ("r0_en_a", 1),
            ("r0_en_b", 1),
            ("dest", dest),
        ])
        .merge(ttu("", &[1, 0], &[1, cdiv(g.k, 256) as i64], 4, "cnt")),
    )
}

/// ringProducer: one tile's outputs to the scalar core (outfeed); k = output
/// ordinal in a RING_PRODUCER_A record (serializes the tiles; op 2, op 1 for a
/// scalar-memory outfeed).
pub fn rprod_output(
    nbytes: usize,
    tile: i64,
    k: i64,
    seq: i64,
    fifo: i64,
    smem: bool,
) -> Vec<u128> {
    rm::encode_ring_producer(
        &fields(&[
            ("tile_mask", tile),
            ("seq", seq),
            ("addr", fifo),
            ("sdims", 1),
            ("cbuf", 1),
            ("slots", 1),
            ("mode", 3),
            ("pcfg", if smem { 12 } else { 20 }),
            ("r0_id", (if smem { 1 } else { 2 } << 5) | 0x11),
            ("r0_val", k),
            ("r0_en_a", 1),
            ("r0_en_b", 1),
            ("r1_id", R_N2W),
            ("r1_val", 1),
            ("r1_en_a", 1),
            ("r1_en_b", 1),
            ("dest", 1 << rm::SCALAR_CORE),
        ])
        .merge(ttu("", &[1, 0], &[1, cdiv(nbytes, 256) as i64], 4, "cnt")),
    )
}

/// Caching: the ringConsumer that writes tile t's n = gt(t) parameter groups (kw
/// 256-byte rows each) to wide memory. addr (2 + the 4n bias rows) and
/// aux_addr0/1 (bias rows 0 .. 4(n-1)) are the relocation fields (+off); no aux
/// without bias.
fn rcons_caching(g: &FcGeom, t: usize, tile: i64, off: i64) -> Fields {
    let (n, kq) = (g.gt(t) as i64, g.kw() as i64);
    let aux = if g.bias {
        fields(&[
            ("aux_en0", 1),
            ("aux_addr0", off),
            ("aux_addr1", 4 * (n - 1) + off),
            ("aux_en1", 1),
        ])
    } else {
        Fields::new()
    };
    fields(&[
        ("tile_mask", tile),
        ("addr", 2 + g.bias_rows(t) as i64 + off),
        ("sdims", if n == 1 { 1 } else { 3 }),
    ])
    .merge(aux)
    .merge(if n > 1 {
        ttu("", &[1, kq], &[kq, n], 4, "cnt")
    } else {
        ttu("", &[1], &[kq], 4, "cnt")
    })
}

// *** mesh DMAs (ring_mesh.md section 4) ***

/// A mesh half: (narrow byte address, words).
type Half = (i64, i64);

/// meshBus `op` with an outbound half (send to the neighbour in op's direction)
/// and/or an inbound half (receive from the opposite neighbour).
fn mesh(
    op: u8,
    tiles: i64,
    seq: i64,
    out: Option<Half>,
    inn: Option<Half>,
    mode: i64,
) -> Vec<u128> {
    let mut f = fields(&[
        ("opcode", i64::from(op)),
        ("tile_mask", tiles),
        ("seq", seq),
    ]);
    for (p, half, m) in [("o_", out, "out_mode"), ("i_", inn, "in_mode")] {
        if let Some((addr, words)) = half {
            f = f
                .merge(fields(&[
                    (&format!("{p}addr"), addr),
                    (&format!("{p}sdims"), i64::from(words > 1)),
                    (m, mode),
                ]))
                .merge(ttu(p, &[1], &[words], 4, "cnt"));
        }
    }
    rm::encode_mesh(&f)
}

/// Mesh relay: forward `nwords` from the neighbour behind to the neighbour ahead in
/// 4-word packets through the 4-slot buffer at narrow `r` (both halves), sync
/// records (IN, 5 + 4c), (OUT, -3 + 4c) for the phase count c. A partial last
/// packet of r words (1..3) sets grp = r-1 and the 16-bit field at bit 249 (+205
/// inbound) to 4*(4-r)+1; a single word moves as a 1-word packet.
pub fn relay(op: u8, tiles: i64, seq: i64, r: i64, nwords: i64, c: i64) -> Vec<u128> {
    let (packets, rest) = (cdiv_i64(nwords, 4), nwords % 4);
    let (inn, out) = mesh_counters(op);
    let (rin, rout) = (
        (tile_counter(inn) << 1) as i64,
        (tile_counter(out) << 1) as i64,
    );
    let mut f = fields(&[
        ("opcode", i64::from(op)),
        ("tile_mask", tiles),
        ("seq", seq),
        ("out_mode", 3),
        ("in_mode", 3),
        ("s0_id", rin),
        ("s0_val", 5 + 4 * c),
        ("s0_en_a", 1),
        ("s0_en_b", 1),
        ("s1_id", rout),
        ("s1_val", -3 + 4 * c),
        ("s1_en_a", 1),
        ("s1_en_b", 1),
        ("s2_id", 1),
    ]);
    for p in ["o_", "i_"] {
        f = f
            .merge(fields(&[
                (&format!("{p}addr"), r),
                (&format!("{p}sdims"), 1),
                (&format!("{p}cbuf"), 1),
                (&format!("{p}slots"), 4),
            ]))
            .merge(ttu(
                p,
                &[1, 0],
                &[if nwords == 1 { 1 } else { 4 }, packets],
                4,
                "cnt",
            ));
    }
    if rest != 0 && nwords > 1 {
        let tail = (4 * (4 - rest) + 1) << 6;
        f = f.merge(fields(&[
            ("o_grp", rest - 1),
            ("i_grp", rest - 1),
            ("rsv243", tail),
            ("rsv448", tail),
        ]));
    }
    rm::encode_mesh(&f)
}

/// Bring the input pieces (input tile t holds `x[b*t : b*t+piece(t)]` at X+b*t) to
/// input tile 0: west within each row of 4 tiles, then north along column 0.
/// Group k of a phase moves the data of position k, positions 1..k-1 relay it.
/// Every west group ends with a mesh fence, north groups once they have relays.
/// The west fence counts are +1.
fn gather(e: &mut Emitter, g: &FcGeom, x: i64, r: i64, ibit: &dyn Fn(usize) -> i64) {
    let rows: Vec<Vec<usize>> = (0..cdiv(g.t_in(), 4))
        .map(|row| (4 * row..(4 * row + 4).min(g.t_in())).collect())
        .collect();
    let mask = |ts: &[usize]| ts.iter().map(|t| ibit(*t)).sum::<i64>();
    let words = |nbytes: usize| cdiv(nbytes, 4) as i64;
    e.sync(sc::sync_reset17);
    let (mut cum, mut last) = (0i64, 0xffffi64);
    for k in 1..4.min(g.t_in()) {
        let rs: Vec<&Vec<usize>> = rows.iter().filter(|row| row.len() > k).collect();
        for row in &rs {
            let seq = e.seq;
            e.tile(mesh(
                0x16,
                mask(&[row[k]]),
                seq,
                Some((x + (g.b() * row[k]) as i64, words(g.piece(row[k])))),
                None,
                3,
            ));
        }
        // Relays forwarding the same number of words share one instruction.
        let mut classes: Vec<(i64, Vec<&Vec<usize>>)> = Vec::new();
        if k >= 2 {
            for row in &rs {
                let w = words(g.piece(row[k]));
                match classes.last_mut() {
                    Some((cw, rows)) if *cw == w => rows.push(row),
                    _ => classes.push((w, vec![row])),
                }
            }
            for (w, crs) in &classes {
                let tiles: Vec<usize> = crs
                    .iter()
                    .flat_map(|row| row[1..k].iter().copied())
                    .collect();
                let seq = e.seq;
                e.tile(relay(0x16, mask(&tiles), seq, r, *w, cum + 1));
            }
        }
        for row in &rs {
            let seq = e.seq;
            e.tile(mesh(
                0x16,
                mask(&[row[0]]),
                seq,
                None,
                Some((x + (g.b() * row[k]) as i64, words(g.piece(row[k])))),
                3,
            ));
        }
        if k >= 2 {
            // The fence excludes the relays that forward as many packets as the first relay instruction.
            let pk = cdiv_i64(classes[0].0, 4);
            cum += pk;
            let excluded: Vec<usize> = classes
                .iter()
                .filter(|(w, _)| cdiv_i64(*w, 4) == pk)
                .flat_map(|(_, crs)| crs.iter().flat_map(|row| row[1..k].iter().copied()))
                .collect();
            last = 0xffff & !mask(&excluded);
        }
        e.sync(|seq| sc::sync_mesh(seq, 0x16, last, cum + 1));
    }
    e.sync(|seq| sc::sync_drain(seq, last));
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    // North: the row gathered on tile 4k moves to tile 0, relayed by tiles 4, 8, ..., 4(k-1).
    cum = 0;
    for (k, row) in rows.iter().enumerate().skip(1) {
        let n = words(row.iter().map(|t| g.piece(*t)).sum());
        let seq = e.seq;
        e.tile(mesh(
            0x17,
            mask(&[4 * k]),
            seq,
            Some((x + (g.b() * 4 * k) as i64, n)),
            None,
            0,
        ));
        if k >= 2 {
            let relays: Vec<usize> = (1..k).map(|j| 4 * j).collect();
            let seq = e.seq;
            e.tile(relay(0x17, mask(&relays), seq, r, n, cum));
        }
        let seq = e.seq;
        e.tile(mesh(
            0x17,
            mask(&[0]),
            seq,
            None,
            Some((x + (g.b() * 4 * k) as i64, n)),
            0,
        ));
        if k >= 2 {
            cum += cdiv_i64(n, 4);
            let relays: Vec<usize> = (1..k).map(|j| 4 * j).collect();
            last = 0xffff & !mask(&relays);
            e.sync(|seq| sc::sync_mesh(seq, 0x17, last, cum));
        }
    }
    if rows.len() >= 3 {
        e.sync(|seq| sc::sync_drain(seq, last));
    }
    e.sync(sc::sync_reset_mesh);
}

// *** tile placement ***

/// `move_gather = true` (pure translation): the program moves from tiles [0, n) to
/// [s, s+n) iff no used tile passes tile 15 and every westward chain stays inside
/// one mesh row (a row of w >= 2 input tiles needs s%4 + w <= 4); northward hops
/// stay neighbours for any s. `move_gather = false`: only the compute tiles move (x
/// is still gathered on [0, T_in) and broadcast over the ring), so any s + T <= 16 works.
pub fn shift_ok(n: usize, k: usize, s: usize, move_gather: bool) -> bool {
    let g = FcGeom::new(n, k);
    if !move_gather {
        return s + g.t() <= 16;
    }
    if s + g.t().max(g.t_in()) > 16 {
        return false;
    }
    (0..cdiv(g.t_in(), 4))
        .filter(|r| g.t_in() - 4 * r >= 2)
        .all(|r| s % 4 + 4.min(g.t_in() - 4 * r) <= 4)
}

pub fn legal_shifts(n: usize, k: usize, move_gather: bool) -> Vec<usize> {
    (0..16)
        .filter(|s| shift_ok(n, k, *s, move_gather))
        .collect()
}

/// Physical tiles: compute tile t -> t + s; input/gather tile t -> t + s (move_gather) or t.
#[derive(Clone, Copy, Debug, Default)]
pub struct Place {
    pub s: usize,
    pub move_gather: bool,
}

impl Place {
    pub fn c(&self, t: usize) -> i64 {
        1 << (t + self.s)
    }

    pub fn i(&self, t: usize) -> i64 {
        1 << (t + if self.move_gather { self.s } else { 0 })
    }

    pub fn cspan(&self, a: usize, b: usize) -> i64 {
        (a..b).map(|t| self.c(t)).sum()
    }
}

// *** program blocks ***

/// Per compute tile its caching ringConsumer and infeed (its groups, from blob byte `base`).
pub fn caching_part(g: &FcGeom, pl: &Place, off: i64, base: i64) -> Vec<CachingPart> {
    (0..g.t())
        .map(|t| CachingPart {
            tiles: pl.c(t),
            consumer: rcons_caching(g, t, pl.c(t), off),
            offset: base + (t * g.p() * g.group_bytes()) as i64,
            bytes: (g.gt(t) * g.group_bytes()) as i64,
            lanes: (g.group_out() / 16) as i64,
        })
        .collect()
}

/// x (`g.k` bytes at narrow X on the tiles `src`) -> the tiles `dest` over the ring, through the wide FIFO.
fn broadcast(e: &mut Emitter, g: &FcGeom, x: i64, src: i64, dest: i64, fifo: i64) {
    let seq = e.seq;
    e.tile(wn::w2n_ring_recv(g.k, dest, x / 4, seq, fifo));
    let seq = e.seq;
    e.tile(rcons_forward(g, dest, seq, fifo));
    let seq = e.seq;
    e.tile(rprod_forward(g, src, dest, seq, fifo));
    let seq = e.seq;
    e.tile(wn::n2w_output(g.k, src, x / 4, fifo, seq, None));
}

/// One input vector: host DMA, ring scatter to the input tiles, mesh gather to
/// input tile 0, ring broadcast to the other compute tiles (scalar.md 8.3).
/// X = narrow address of the vector on every tile.
fn input_block(e: &mut Emitter, g: &FcGeom, pl: &Place, x: i64, r: i64) {
    e.sync(sc::input_head);
    e.sync(sc::sync_wn_fence);
    e.scalar_with_base(
        sc::input_dma(g.s() as i64, 0),
        Description::InputActivation,
        Some("x"),
        false,
    );
    for t in 0..g.t_in() {
        let seq = e.seq;
        e.tile(wn::w2n_input(g.k, t, x / 4, seq, Some(pl.i(t))));
    }
    for j in 0..cdiv(g.t_in(), 4) {
        let ts: i64 = (4 * j..(4 * j + 4).min(g.t_in())).map(|t| pl.i(t)).sum();
        let seq = e.seq;
        e.tile(rcons_input(g, j, ts, seq));
        e.scalar(
            sc::av_infeed(
                (4 * j * g.b()) as i64,
                (4 * g.b()).min(g.s() - 4 * j * g.b()) as i64,
                ts,
            ),
            0,
        );
    }
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    e.sync(sc::sync_reset17);
    if g.t_in() >= 2 {
        gather(e, g, x, r, &|t| pl.i(t));
    }
    e.scalar(sc::scsync_nop(), 0);
    e.sync(sc::sync_reset17);
    let dest = pl.cspan(0, g.t()) & !pl.i(0);
    if dest != 0 {
        broadcast(e, g, x, pl.i(0), dest, WIDE_FWD_FIFO);
    }
    e.sync(sc::sync_reset17);
}

/// Compute fence, bias loads (one per distinct group count), the ops, reset; `tail`: + the completion fence.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the generator's block signature"
)]
fn ops_block(
    e: &mut Emitter,
    g: &FcGeom,
    pl: &Place,
    x: i64,
    y: i64,
    off: i64,
    quant: &Quant,
    tail: bool,
) {
    e.sync(sc::signal_fence);
    if g.bias {
        let mut counts: Vec<usize> = (0..g.t()).map(|t| g.gt(t)).collect();
        counts.sort_unstable();
        counts.dedup();
        for n in counts.into_iter().rev() {
            let tiles: i64 = (0..g.t()).filter(|t| g.gt(*t) == n).map(|t| pl.c(t)).sum();
            let seq = e.seq;
            e.tile(wn::w2n_bias(tiles, off, n as i64, seq));
        }
    }
    for t in 0..g.t() {
        let mut f = op::fc_tile_fields(&op::FcTile {
            n: g.n,
            k: g.k,
            t,
            seq: e.seq,
            in_base: x,
            out_base: y + (64 * g.p() * t) as i64,
            par_base: g.bias_rows(t) as i64 + off,
            w_zp: i64::from(quant.w_zp),
            in_zp: i64::from(quant.in_zp),
            mult: quant.mult,
            out_zp: i64::from(quant.out_zp),
            clamp_min: Some(quant.clamp_min()),
            clamp_max: Some(quant.clamp_max()),
        })
        .set("tile_mask", pl.c(t));
        if !g.bias {
            // The bias ("scaling") path is off.
            f = f.merge(fields(&[
                ("cfg1", 0),
                ("sync0", 0),
                ("sync1", 0),
                ("cfg4", 0),
            ]));
        }
        e.tile(op::encode_op(&f));
    }
    e.sync(sc::sync_reset17);
    if tail {
        e.sync(sc::signal_fence);
    }
}

/// One output vector: narrowToWide per compute tile, host DMA, (outfeed, ringProducer) per tile in ordinal order.
fn output_block(e: &mut Emitter, g: &FcGeom, pl: &Place, y: i64) {
    e.scalar_with_base(
        sc::output_dma_head(0),
        Description::OutputActivation,
        Some("y"),
        false,
    );
    for t in 0..g.t() {
        let seq = e.seq;
        e.tile(wn::n2w_output(
            g.n_out(t),
            pl.c(t),
            (y + (64 * g.p() * t) as i64) / 4,
            WIDE_OUT_FIFO,
            seq,
            None,
        ));
    }
    e.scalar(sc::output_dma_tail(g.out_bytes() as i64), 0);
    for t in 0..g.t() {
        e.scalar(sc::outfeed((8 * cdiv(g.n_out(t), 8)) as i64), 0);
        let seq = e.seq;
        e.tile(rprod_output(
            g.n_out(t),
            pl.c(t),
            t as i64,
            seq,
            WIDE_OUT_FIFO,
            false,
        ));
    }
    let seq = e.seq;
    e.scalar(sc::output_wait(seq, g.t() as i64), 2);
}

/// What to generate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FcSpec {
    pub n: usize,
    pub k: usize,
    /// Byte offset of the parameter region in every tile's wide memory (a multiple
    /// of 256: on the device other offsets hang the caching program).
    pub param_offset: usize,
    /// Place the compute tiles on [s, s+T).
    pub tile_shift: usize,
    /// Translate the whole program; otherwise keep the input scatter/gather on
    /// [0, T_in) and broadcast x to the shifted tiles (a structure edgetpu_compiler never emits).
    pub move_gather: bool,
    pub quant: Quant,
    /// No bias rows or load (the compiler's form for N=1 with an all-zero bias).
    pub bias: bool,
}

impl FcSpec {
    pub const fn new(n: usize, k: usize) -> Self {
        Self {
            n,
            k,
            param_offset: 0,
            tile_shift: 0,
            move_gather: true,
            quant: Quant::REFERENCE,
            bias: true,
        }
    }
}

/// The two programs of one FULLY_CONNECTED layer.
#[derive(Debug)]
pub struct FcPrograms {
    pub geom: FcGeom,
    pub caching: Executable,
    pub execution: Executable,
}

/// `(PARAMETER_CACHING, EXECUTION_ONLY)` for `y[N] = W[N,K] x[K]` (uint8),
/// byte-identical to edgetpu_compiler for `tile_shift = 0`.
pub fn build_fc(spec: &FcSpec) -> Result<FcPrograms> {
    let fail = |what: String| Error::Codegen(format!("fc {}x{}: {what}", spec.n, spec.k));
    if !spec.param_offset.is_multiple_of(256) {
        return Err(fail(format!(
            "param_offset {} is not a multiple of 256",
            spec.param_offset
        )));
    }
    if spec.n == 0 || spec.k == 0 {
        return Err(fail("empty matrix".into()));
    }
    let g = FcGeom {
        n: spec.n,
        k: spec.k,
        bias: spec.bias,
    };
    if g.t() > 16 || g.t_in() > 16 {
        return Err(fail("needs more than 16 tiles".into()));
    }
    if spec.k <= 4 && g.p() > 1 {
        return Err(fail(
            "K <= 4 with N > 1024: the compiler uses another op schedule".into(),
        ));
    }
    if !shift_ok(spec.n, spec.k, spec.tile_shift, spec.move_gather) {
        return Err(fail(format!(
            "tile_shift {} is not legal (move_gather {}); legal: {:?}",
            spec.tile_shift,
            spec.move_gather,
            legal_shifts(spec.n, spec.k, spec.move_gather)
        )));
    }
    let pl = Place {
        s: spec.tile_shift,
        move_gather: spec.move_gather,
    };
    let off = (spec.param_offset / 64) as i64;
    let (x, y, r) = narrow_layout(spec.n, spec.k);
    let mut e = Emitter::new();
    e.scalar(sc::exe_prologue(), 1);
    input_block(&mut e, &g, &pl, x, r);
    ops_block(&mut e, &g, &pl, x, y, off, &spec.quant, true);
    output_block(&mut e, &g, &pl, y);
    e.sync(|seq| sc::epilogue(seq, false));
    let (exe_bytes, exe_relocs) = e.program();
    let (caching_bytes, caching_relocs) =
        caching_program(g.param_bytes() as i64, &[caching_part(&g, &pl, off, 0)]);
    let name = format!("fc_{}x{}", spec.n, spec.k);
    let caching = Executable {
        name: format!("{name}:caching"),
        kind: ExecutableType::ParameterCaching,
        batch_size: 1,
        scratch_size: 0,
        bitstreams: vec![Bitstream {
            data: caching_bytes,
            relocs: caching_relocs,
        }],
        parameters: Vec::new(),
        hints: vec![
            Hint::Instruction { chunk: 0 },
            Hint::Dma {
                direction: Direction::Infeed,
                desc: Description::Parameter,
                name: None,
                offset: 0,
                size: g.param_bytes(),
            },
            Hint::Interrupt { id: 0 },
        ],
        inputs: Vec::new(),
        outputs: Vec::new(),
        caching_token: 0,
    };
    let execution = Executable {
        name: format!("{name}:execution"),
        kind: ExecutableType::ExecutionOnly,
        batch_size: 1,
        scratch_size: 0,
        bitstreams: vec![Bitstream {
            data: exe_bytes,
            relocs: exe_relocs,
        }],
        parameters: Vec::new(),
        hints: vec![
            Hint::Instruction { chunk: 0 },
            Hint::Dma {
                direction: Direction::Infeed,
                desc: Description::InputActivation,
                name: Some("x".into()),
                offset: 0,
                size: g.s(),
            },
            Hint::Dma {
                direction: Direction::Outfeed,
                desc: Description::OutputActivation,
                name: Some("y".into()),
                offset: 0,
                size: g.out_bytes(),
            },
            Hint::Interrupt { id: 0 },
        ],
        inputs: vec![Layer {
            name: "x".into(),
            size_bytes: g.s(),
        }],
        outputs: vec![Layer {
            name: "y".into(),
            size_bytes: g.out_bytes(),
        }],
        caching_token: 0,
    };
    Ok(FcPrograms {
        geom: g,
        caching,
        execution,
    })
}

// *** the host side of one matmul ***

/// One matmul's parameters as edgetpu_compiler lays them out (FULLY_CONNECTED and
/// 1x1 conv alike): per group of `group` outputs an int32 bias row, then the
/// weights as `[K4/4][group][4]` uint8. `w` is `[N, K]` row-major; K is padded to a
/// multiple of 4 with `w_zp`, the padded outputs are 0.
pub fn param_blob(
    w: &[u8],
    n: usize,
    k: usize,
    w_zp: u8,
    bias: Option<&[i32]>,
    group: usize,
) -> Vec<u8> {
    assert_eq!(w.len(), n * k, "weights are [N, K]");
    let (np, k4) = (round_up(n, group), round_up(k, 4));
    let mut out = Vec::with_capacity(np * 4 + np * k4);
    for g in (0..np).step_by(group) {
        for row in g..g + group {
            let b = bias.and_then(|b| b.get(row)).copied().unwrap_or(0);
            out.extend_from_slice(&if row < n { b } else { 0 }.to_le_bytes());
        }
        for word in 0..k4 / 4 {
            for row in g..g + group {
                for j in 0..4 {
                    let col = 4 * word + j;
                    out.push(if row >= n {
                        0
                    } else if col >= k {
                        w_zp
                    } else {
                        w[row * k + col]
                    });
                }
            }
        }
    }
    out
}

/// What the Edge TPU computes for `y = requant(x @ W.T + b)`: int32 accumulate,
/// float32 rescale, clamp, round half to even (measured with exact .5 ties on the
/// device), add the zero point. `x` is `[K]`, `w` is `[N, K]`.
pub fn fc_reference(q: &Quant, x: &[u8], w: &[u8], bias: &[i32], n: usize, k: usize) -> Vec<u8> {
    assert!(
        x.len() >= k && w.len() == n * k && bias.len() == n,
        "fc_reference: shapes"
    );
    (0..n)
        .map(|row| {
            let acc: i64 = (0..k)
                .map(|col| {
                    (i64::from(x[col]) - i64::from(q.in_zp))
                        * (i64::from(w[row * k + col]) - i64::from(q.w_zp))
                })
                .sum::<i64>()
                + i64::from(bias[row]);
            let y = (acc as f32 * q.mult).clamp(q.clamp_min(), q.clamp_max());
            (y.round_ties_even() + f32::from(q.out_zp)) as u8
        })
        .collect()
}

/// A FULLY_CONNECTED layer on the chip: N < 64 runs the 64-output program with padded
/// rows (the compiler's smaller groups read wrong weights at >= 256 KiB).
#[derive(Clone, Copy, Debug)]
pub struct FcProgram {
    pub n: usize,
    pub k: usize,
    /// Outputs the program computes.
    pub rows: usize,
}

impl FcProgram {
    pub const fn new(n: usize, k: usize) -> Self {
        Self {
            n,
            k,
            rows: if n < 64 { 64 } else { n },
        }
    }

    pub const fn geom(&self) -> FcGeom {
        FcGeom::new(self.rows, self.k)
    }

    /// Parameter bytes per tile, 256-byte aligned regions.
    pub fn tile_bytes(&self) -> usize {
        let g = self.geom();
        256 * cdiv(g.p() * g.group_bytes(), 256)
    }

    /// Whether the weights fit the tiles' wide memory from `param_offset`.
    pub fn fits(&self, param_offset: usize) -> bool {
        param_offset + self.tile_bytes() <= WIDE_BYTES
    }

    pub fn params(&self, w: &[u8], bias: &[i32], w_zp: u8) -> Vec<u8> {
        param_blob(w, self.n, self.k, w_zp, Some(bias), 64)
    }

    /// The compute tiles move to [shift, shift+T); the input gather stays on tiles 0..
    pub fn programs(&self, shift: usize, param_offset: usize, quant: Quant) -> Result<FcPrograms> {
        if !self.fits(param_offset) {
            return Err(Error::Codegen(format!(
                "fc {}x{}: {} parameter bytes per tile at offset {param_offset} exceed the {WIDE_BYTES} available",
                self.n,
                self.k,
                self.tile_bytes()
            )));
        }
        build_fc(&FcSpec {
            n: self.rows,
            k: self.k,
            param_offset,
            tile_shift: shift,
            move_gather: false,
            quant,
            bias: true,
        })
    }

    /// x padded to the input DMA size with the input zero point.
    pub fn input_bytes(&self, x: &[u8], in_zp: u8) -> Vec<u8> {
        assert_eq!(x.len(), self.k, "input is [K]");
        let mut out = x.to_vec();
        out.resize(self.geom().s(), in_zp);
        out
    }

    /// The N outputs from the program's output buffer.
    pub fn output<'a>(&self, out: &'a [u8]) -> &'a [u8] {
        &out[..self.n]
    }
}

#[cfg(test)]
mod tests;
