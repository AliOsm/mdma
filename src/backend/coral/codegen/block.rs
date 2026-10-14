//! Stages the whole-model program shares with edgetpu_compiler's attention block
//! and FFN programs: the 1-D input scatter, the gather of the attention heads onto
//! tile 0, the KV row copies, single-tile outputs, the FULLY_CONNECTED
//! quantization, and the bias load and parameter broadcast of streamed matmuls.

use super::attention::{DH, Geom, HEAD0, HW, ROWS, chain_copy};
use super::conv2d::emit_groups;
use super::fc::{FcGeom, rcons_input, relay, rprod_output};
use super::{Emitter, WIDE_OUT_FIFO};
use crate::backend::coral::executable::Description;
use crate::backend::coral::isa::op::encode_op;
use crate::backend::coral::isa::scalar as sc;
use crate::backend::coral::isa::wide_narrow::{self as wn, encode_wide_to_narrow};
use crate::backend::coral::isa::{fields, ring_mesh as rm, round_up, ttu};

/// A `[k]` input in edgetpu_compiler's 1-D layout (tile t holds bytes
/// `[64t, 64t+64)` at narrow `x + 64t`): host DMA, ring multicast, each tile its chunk.
pub fn scatter_input(e: &mut Emitter, x: i64, k: usize, input: &str) {
    let g = FcGeom::new(64, k);
    e.sync(sc::input_head);
    e.sync(sc::sync_wn_fence);
    e.scalar_with_base(
        sc::input_dma(g.s() as i64, 0),
        Description::InputActivation,
        Some(input),
        false,
    );
    for t in 0..g.t_in() {
        let seq = e.seq;
        e.tile(wn::w2n_input(g.k, t, x / 4, seq, Some(1 << t)));
    }
    for j in 0..g.t_in().div_ceil(4) {
        let ts: i64 = (4 * j..(4 * j + 4).min(g.t_in())).map(|t| 1 << t).sum();
        let seq = e.seq;
        e.tile(rcons_input(&g, j, ts, seq));
        let (b, s) = (g.b() as i64, g.s() as i64);
        e.scalar(
            sc::av_infeed(4 * j as i64 * b, (4 * b).min(s - 4 * j as i64 * b), ts),
            0,
        );
    }
    e.sync(|seq| sc::sync_drain(seq, 0xffff));
    e.sync(sc::sync_reset17);
}

/// The attention output (tile row r: `ROWS[r]` heads x 48 bytes at narrow `o` on
/// its column-0 tile) to tile 0 as `o[288]`: the north gather with the head rows
/// as pieces.
pub fn gather_north(e: &mut Emitter, o: i64, r: i64) {
    e.sync(sc::sync_reset17);
    let (mut cum, mut last) = (0, 0xffff);
    for k in 1..4usize {
        let n = HW * ROWS[k];
        let seq = e.seq;
        e.tile(super::fc::mesh(
            0x17,
            1 << (4 * k),
            seq,
            Some((o, n)),
            None,
            0,
        ));
        let relays: i64 = (1..k).map(|j| 1 << (4 * j)).sum();
        if k >= 2 {
            let seq = e.seq;
            e.tile(relay(0x17, relays, seq, r, n, cum));
        }
        let seq = e.seq;
        e.tile(super::fc::mesh(
            0x17,
            1,
            seq,
            None,
            Some((o + DH * HEAD0[k], n)),
            0,
        ));
        if k >= 2 {
            cum += (n + 3) / 4;
            last = 0xffff & !relays;
            e.sync(|seq| sc::sync_mesh(seq, 0x17, last, cum));
        }
    }
    e.sync(|seq| sc::sync_drain(seq, last));
    e.sync(sc::sync_reset_mesh);
}

/// The new row (6 heads x 48 bytes at narrow `src`, on every tile) into position
/// P-1 of the `[rows][cols][48]` block at `x`: one copy through the identity row
/// on each tile of the last tile column that holds positions.
pub fn slot_copies(e: &mut Emitter, g: &Geom, src: i64, x: i64) {
    let cl = (0..4).rfind(|c| g.cols[*c] != 0).unwrap_or(0);
    let (j, s) = (g.p - 1 - g.p0(cl), g.cols[cl]);
    let per: Vec<_> = (0..16)
        .map(|t| {
            let rows = ROWS[t / 4];
            (t % 4 == cl).then(|| {
                chain_copy(
                    src + DH * HEAD0[t / 4],
                    x + DH * j,
                    HW,
                    1,
                    rows,
                    [HW, HW, HW * rows],
                    [HW, HW * s, HW * s * rows],
                )
            })
        })
        .collect();
    emit_groups(e, &per, encode_op);
}

/// `n` bytes at narrow `y` of tile 0 to the host output `output`.
pub fn output_tile0(e: &mut Emitter, y: i64, n: usize, output: &str) {
    e.sync(sc::signal_fence);
    e.scalar_with_base(
        sc::output_dma_head(0),
        Description::OutputActivation,
        Some(output),
        false,
    );
    let seq = e.seq;
    e.tile(wn::n2w_output(n, 1, y / 4, WIDE_OUT_FIFO, seq, None));
    let bytes = round_up(n, 8) as i64;
    e.scalar(sc::output_dma_tail(bytes), 0);
    e.scalar(sc::outfeed(bytes), 0);
    let seq = e.seq;
    e.tile(rprod_output(n, 1, 0, seq, WIDE_OUT_FIFO, false));
    let seq = e.seq;
    e.scalar(sc::output_wait(seq, 1), 2);
}

/// Mode-2 load of the bias block at wide `bias`; `b` = weight FIFO fills per group.
pub fn bias_load(seq: i64, tiles: i64, bias: i64, b: i64) -> Vec<u128> {
    encode_wide_to_narrow(
        &fields(&[
            ("seq", seq),
            ("tile_mask", tiles),
            ("wide_addr", bias),
            ("wide_lvl_mask", 1),
            ("wide_circ", 1),
            ("wide_rows", 1),
            ("narrow_addr", 0x40),
            ("narrow_lvl_mask", 3),
            ("mode", 2),
            ("size64", 2),
            ("sync_f2", 1),
            ("sync_id", 15),
            ("sync_wait_lvl", 1),
            ("sync_val", (b << 15) & 0x7ffff),
            ("rsv643", b >> 4),
            ("sync_dec_lvl", 2),
            ("sync_dec", -1),
            ("sync_dec_mode", 3),
            ("tail_f867", 1),
        ])
        .merge(wn::ttu("w", &[1, 0], &[1, 1], 4))
        .merge(wn::ttu("n", &[1, 64, 0], &[32, 1, 1], 6)),
    )
}

/// Parameter broadcast: tile `tile` streams `rows` cached rows from wide `off` to
/// ringConsumer1 of the tiles in `dest`; `first` = rows sent before.
pub fn rprod_param(seq: i64, tile: i64, off: i64, rows: i64, first: i64, dest: i64) -> Vec<u128> {
    rm::encode_ring_producer(
        &fields(&[
            ("tile_mask", 1 << tile),
            ("seq", seq),
            ("addr", off),
            ("sdims", 1),
            ("pcfg", 4),
            ("r0_id", (1 << 5) | 0x12),
            ("r0_val", first),
            ("r0_en_a", 1),
            ("r0_en_b", 1),
            ("to_c1", 1),
            ("dest", dest),
        ])
        .merge(ttu("", &[1], &[rows], 4, "cnt")),
    )
}
