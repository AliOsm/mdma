//! Tile-local DMAs, 7 words each:
//!   0x14 wideToNarrow  wide memory / wide ring FIFO -> narrow memory (or -> the bias "scaling" store, mode 2)
//!   0x13 narrowToWide  narrow memory -> wide memory / wide ring FIFO
//! TTU units: narrow 4-byte words; wide one 256-byte row per step (wide addresses
//! are in 64-byte units).

use super::{FieldSpec, Fields, Layout, cdiv, fields, spec, sspec, ttu as ttu_generic};
use std::sync::LazyLock;

fn ttu_fields(p: &str, lo: usize, levels: usize) -> Vec<FieldSpec> {
    (0..levels)
        .flat_map(|d| {
            [
                sspec(&format!("{p}_inc{d}"), lo + 33 * d, 17),
                spec(&format!("{p}_lim{d}"), lo + 33 * d + 17, 16),
            ]
        })
        .collect()
}

/// 88-bit sync block; only wideToNarrow uses the decrement fields.
fn sync_fields(lo: usize, w2n: bool) -> Vec<FieldSpec> {
    let mut f = vec![
        spec("sync_en0", lo, 1),
        spec("sync_f1", lo + 1, 1),
        spec("sync_f2", lo + 2, 1),
        spec("sync_id", lo + 4, 5),
        spec("sync_wait_lvl", lo + 9, 2),
        spec("sync_en1", lo + 12, 1),
        spec("sync_val", lo + 13, 19),
        spec("sync_en2", lo + 44, 1),
        spec("sync_f45", lo + 45, 1),
    ];
    if w2n {
        f.extend([
            spec("sync_dec_lvl", lo + 51, 2),
            sspec("sync_dec", lo + 54, 17),
            spec("sync_dec_mode", lo + 86, 2),
        ]);
    }
    f
}

/// The defaults are the bits set in every compiled instance.
pub static W2N: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![
        spec("tile_mask", 12, 16),
        spec("seq", 46, 14),
        spec("wide_addr", 60, 14),
    ];
    f.extend(ttu_fields("w", 78, 4));
    f.extend([
        spec("wide_lvl_mask", 210, 4),
        spec("wide_circ", 214, 1),
        spec("wide_rows", 216, 11),
        spec("narrow_addr", 267, 16),
    ]);
    f.extend(ttu_fields("n", 283, 6));
    f.extend([
        spec("narrow_lvl_mask", 481, 6),
        spec("mode", 487, 2),
        spec("size64", 494, 13), // bytes read from the wide side / 64 (min 4)
        spec("head_words_m1", 507, 16),
        spec("head_en", 523, 1),
        spec("head_tail16", 530, 4),
        spec("skip_en0", 542, 1),
        spec("skip_en1", 544, 1),
        spec("skip_base", 546, 16),
        spec("skip_m1", 562, 16),
        spec("skip_end", 578, 16),
    ]);
    f.extend(sync_fields(611, true));
    f.extend([
        spec("tail_f867", 867, 1),
        spec("tail_en870", 870, 1),
        spec("tail_f871", 871, 1),
    ]);
    Layout::new(
        "wideToNarrow",
        &[0x14],
        7,
        f,
        fields(&[
            ("n_inc0", 1),
            ("sync_en0", 1),
            ("sync_en1", 1),
            ("sync_en2", 1),
            ("tail_en870", 1),
        ]),
    )
});

pub static N2W: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![
        spec("tile_mask", 12, 16),
        spec("seq", 46, 14),
        spec("narrow_addr", 62, 16),
    ];
    f.extend(ttu_fields("n", 78, 6));
    f.extend([spec("narrow_lvl_mask", 276, 6), spec("wide_addr", 335, 14)]);
    f.extend(ttu_fields("w", 353, 4));
    f.extend([
        spec("wide_lvl_mask", 485, 4),
        spec("wide_circ", 489, 1),
        spec("wide_rows", 491, 11),
    ]);
    f.extend(sync_fields(543, false));
    f.extend([
        // 168 reserved bits, named in pieces an i64 field value can fill (ADD's weight move sets 83 of them).
        spec("rsv631", 631, 62),
        spec("rsv693", 693, 62),
        spec("rsv755", 755, 44),
        spec("tail_f799", 799, 1),
        spec("tail_lvl", 801, 2),
    ]);
    Layout::new(
        "narrowToWide",
        &[0x13],
        7,
        f,
        fields(&[
            ("n_inc0", 1),
            ("sync_en0", 1),
            ("sync_en1", 1),
            ("sync_en2", 1),
        ]),
    )
});

pub fn encode_wide_to_narrow(f: &Fields) -> Vec<u128> {
    W2N.encode(f)
}

pub fn encode_narrow_to_wide(f: &Fields) -> Vec<u128> {
    N2W.encode(f)
}

pub fn ttu(p: &str, strides: &[i64], counts: &[i64], levels: usize) -> Fields {
    ttu_generic(&format!("{p}_"), strides, counts, levels, "lim")
}

// *** FULLY_CONNECTED / RELU recipes (edgetpu_compiler templates). Narrow addresses in 4-byte words, wide in 64-byte units ***

/// Chunk `c` of an `s`-byte input (P = 64*ceil(S/1024) bytes per chunk, on tile c
/// unless `tiles`) from the ring-consumer-0 FIFO (2*ceil(S/256) rows below wide
/// address 0x2080) to narrow X + its offset.
pub fn w2n_input(s: usize, c: usize, x: i64, seq: i64, tiles: Option<i64>) -> Vec<u128> {
    let p = 64 * cdiv(s, 1024);
    let b = p.min(s - c * p);
    let (row, o) = (((c % 4) * p) / 256, ((c % 4) * p) % 256);
    let mut f = fields(&[
        ("seq", seq),
        ("tile_mask", tiles.unwrap_or(1 << c)),
        (
            "wide_addr",
            0x2080 - 8 * cdiv(s, 256) as i64 + 4 * row as i64,
        ),
        ("narrow_addr", x + (c * p / 4) as i64),
        ("wide_lvl_mask", 1),
        ("narrow_lvl_mask", 1),
        ("mode", 1),
        ("size64", 4.max(cdiv(b, 64)) as i64),
        ("sync_id", 14),
        ("sync_wait_lvl", 2),
        ("sync_val", 0x8000),
        ("sync_dec_lvl", 2),
        ("sync_dec", -1),
        ("sync_dec_mode", 3),
    ])
    .merge(ttu("w", &[1], &[cdiv(o + b, 256) as i64], 4))
    .merge(ttu("n", &[1], &[cdiv(b, 4) as i64], 6));
    if o > 0 {
        let o = o as i64;
        f = f.merge(fields(&[
            ("skip_en0", 1),
            ("skip_en1", 1),
            ("skip_m1", o / 4 - 1),
            ("skip_base", x),
            ("skip_end", x + o / 4 - 1),
        ]));
    }
    encode_wide_to_narrow(&f)
}

/// Receive the `k`-byte vector broadcast over the ring (through the wide FIFO) into narrow `x`.
pub fn w2n_ring_recv(k: usize, tile_mask: i64, x: i64, seq: i64, fifo: i64) -> Vec<u128> {
    let r = cdiv(k, 256) as i64;
    let one = i64::from(r == 1);
    let f = fields(&[
        ("seq", seq),
        ("tile_mask", tile_mask),
        ("wide_addr", fifo),
        ("wide_rows", 1),
        ("wide_circ", one),
        ("narrow_addr", x),
        ("narrow_lvl_mask", i64::from(k > 4)),
        ("sync_id", 14),
        ("sync_wait_lvl", one),
        ("sync_val", 0x8000),
    ])
    .merge(ttu("w", &[one], &[r], 4))
    .merge(ttu("n", &[1], &[cdiv(k, 4) as i64], 6));
    encode_wide_to_narrow(&f)
}

/// Mode-2 load of the per-64-output bias block(s) at wide `param_base` (the relocation field).
pub fn w2n_bias(tile_mask: i64, param_base: i64, groups: i64, seq: i64) -> Vec<u128> {
    let g = groups;
    let f = fields(&[
        ("seq", seq),
        ("tile_mask", tile_mask),
        ("wide_addr", param_base),
        ("narrow_addr", 0x40),
        ("narrow_lvl_mask", 3),
        ("mode", 2),
        ("size64", 2),
        ("wide_lvl_mask", if g > 1 { 3 } else { 1 }),
        ("sync_f2", 1),
        ("tail_f867", 1),
        ("sync_id", 0),
        ("sync_wait_lvl", 2),
        ("sync_val", 0xffff),
        ("sync_f45", 1),
    ])
    .merge(if g > 1 {
        ttu("w", &[1, 1], &[1, g], 4)
    } else {
        ttu("w", &[1], &[1], 4)
    })
    .merge(ttu("n", &[1, 64, 0], &[32, 1, g], 6));
    encode_wide_to_narrow(&f)
}

/// `nbytes` from narrow memory to a wide ring FIFO (0x2078: the output FIFO drained
/// by ringProducer/outfeed; 0x1f70: FC ring send to other tiles); `dims` = the
/// narrow walk `[(stride, count)]` in words, innermost first (default: contiguous).
pub fn n2w_output(
    nbytes: usize,
    tile_mask: i64,
    narrow_addr: i64,
    wide_addr: i64,
    seq: i64,
    dims: Option<&[(i64, i64)]>,
) -> Vec<u128> {
    let r = cdiv(nbytes, 256) as i64;
    let contiguous = [(1, cdiv(nbytes, 4) as i64)];
    let dims = dims.unwrap_or(&contiguous);
    let one = i64::from(r == 1);
    let strides: Vec<i64> = dims.iter().map(|(s, _)| *s).collect();
    let counts: Vec<i64> = dims.iter().map(|(_, c)| *c).collect();
    let f = fields(&[
        ("seq", seq),
        ("tile_mask", tile_mask),
        ("narrow_addr", narrow_addr),
        ("wide_addr", wide_addr),
        ("wide_rows", 1),
        (
            "narrow_lvl_mask",
            dims.iter()
                .enumerate()
                .filter(|(_, (_, c))| *c > 1)
                .map(|(k, _)| 1i64 << k)
                .sum(),
        ),
        ("wide_circ", one),
        ("sync_f1", one),
        ("sync_id", 16),
        ("sync_wait_lvl", one),
        ("sync_val", 0xffff),
        ("sync_f45", 1),
        ("tail_lvl", dims.len() as i64),
    ])
    .merge(ttu("n", &strides, &counts, 6))
    .merge(ttu("w", &[one], &[r], 4));
    encode_narrow_to_wide(&f)
}
