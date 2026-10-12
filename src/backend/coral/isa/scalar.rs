//! Scalar-core instructions and the program skeleton: 0x3e start, 0x3f end, 0x21 halt, 0x1a sync (tile fence),
//! 0x24 scalar sync, 0x25 avDataPop / parameterPop, 0x26 infeed, 0x27 outfeed, and
//! the scalar ALU (0x20) words that build host DMA descriptors. Fields named after
//! a bit number (`k157`, ...) are constant within a stream class; their meaning is
//! unknown.

use super::{Fields, Layout, cdiv_i64, fields, neg, spec, ttu as ttu_generic, words_to_bytes};
use std::sync::LazyLock;

pub const TAG_INPUT: i64 = 1;
pub const TAG_PARAMETERS: i64 = 2;
pub const TAG_OUTPUT: i64 = 3;
pub const TAG_INT0: i64 = 4;

/// Staging buffers in units: 16384 x 8 B activations, 1024 x 64 B parameters.
pub const AV_BUF: i64 = 1 << 14;
pub const PAR_BUF: i64 = 1 << 10;

pub const TILE_COUNTERS: [&str; 19] = [
    "AVDATA",
    "PARAMETERS",
    "PARTIAL_SUMS",
    "MESH_NORTH_IN",
    "MESH_EAST_IN",
    "MESH_SOUTH_IN",
    "MESH_WEST_IN",
    "MESH_NORTH_OUT",
    "MESH_EAST_OUT",
    "MESH_SOUTH_OUT",
    "MESH_WEST_OUT",
    "WIDE_TO_NARROW",
    "WIDE_TO_SCALING",
    "NARROW_TO_WIDE",
    "RING_READ_A",
    "RING_READ_B",
    "RING_WRITE",
    "RING_PRODUCER_A",
    "RING_PRODUCER_B",
];
pub const SCALAR_COUNTERS: [&str; 9] = [
    "AVDATA_POP",
    "PARAMETER_POP",
    "AVDATA_INFEED",
    "PARAMETER_INFEED",
    "SCALAR_INFEED",
    "PRODUCER_A",
    "PRODUCER_B",
    "RING_OUTFEED",
    "SCALAR_PIPELINE",
];

pub fn tile_counter(name: &str) -> usize {
    TILE_COUNTERS
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| panic!("unknown tile counter {name}"))
}

pub fn tc(names: &[&str]) -> i64 {
    names.iter().map(|n| 1i64 << tile_counter(n)).sum()
}

pub fn scm(names: &[&str]) -> i64 {
    names
        .iter()
        .map(|n| {
            1i64 << SCALAR_COUNTERS
                .iter()
                .position(|c| c == n)
                .unwrap_or_else(|| panic!("unknown scalar counter {n}"))
        })
        .sum()
}

/// The counters of a mesh move (meshBus opcode): the receiving tile's IN side, the
/// sending tile's OUT side.
pub fn mesh_counters(op: u8) -> (&'static str, &'static str) {
    match op {
        0x15 => ("MESH_NORTH_IN", "MESH_SOUTH_OUT"),
        0x16 => ("MESH_EAST_IN", "MESH_WEST_OUT"),
        0x17 => ("MESH_SOUTH_IN", "MESH_NORTH_OUT"),
        0x18 => ("MESH_WEST_IN", "MESH_EAST_OUT"),
        _ => panic!("{op:#x} is not a meshBus opcode"),
    }
}

/// 128 * (words between start and end, exclusive).
pub static START: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "start",
        &[0x3e],
        1,
        vec![spec("length_bits", 19, 32)],
        Fields::new(),
    )
});
pub static END: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "end",
        &[0x3f],
        1,
        vec![spec("length_bits", 19, 32)],
        Fields::new(),
    )
});
/// 4; 1 ends a bitstream that continues in the next one.
pub static HALT: LazyLock<Layout> =
    LazyLock::new(|| Layout::new("halt", &[0x21], 1, vec![spec("code", 14, 5)], Fields::new()));
/// Tile fence: dispatched to the tiles, takes a slot in the tile-instruction sequence.
pub static SYNC: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "sync",
        &[0x1a],
        1,
        vec![
            spec("tiles", 12, 16),
            spec("seq", 28, 15),
            spec("b43", 43, 1),
            spec("counters", 44, 19),
            spec("count", 63, 16),
            spec("units", 79, 10),
            spec("signal", 107, 1),
            spec("f109", 109, 2),
        ],
        Fields::new(),
    )
});
/// Scalar fence / scalar sync-counter op (not dispatched to the tiles).
pub static SCSYNC: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![
        spec("tiles", 12, 16),
        spec("smask", 32, 9),
        spec("value", 43, 13),
        spec("tokwait", 57, 1),
        spec("tmask", 73, 16),
    ];
    f.extend((0..9).map(|k| spec(&format!("thr{k}"), 93 + 16 * k, 16)));
    f.extend([spec("wait", 237, 9), spec("sunits", 246, 5)]);
    Layout::new("scsync", &[0x24], 2, f, Fields::new())
});
/// avDataPop (stream=1) / parameterPop (stream=0): host stream -> circular staging buffer.
pub static POP: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "pop",
        &[0x25],
        2,
        vec![
            spec("stream", 12, 1),
            spec("d0_stride", 28, 16),
            spec("d0_limit", 44, 15),
            spec("d1_stride", 59, 16),
            spec("d1_limit", 75, 15),
            spec("d2_stride", 90, 16),
            spec("d2_limit", 106, 15),
            spec("k121", 121, 1),
            spec("k124", 124, 1),
            spec("par136", 136, 1),
            spec("av140", 140, 1),
            spec("count_m1", 142, 14),
            spec("k157", 157, 1),
            spec("count_neg", 159, 14),
            spec("k174", 174, 1),
            spec("par177", 177, 1),
            spec("k178", 178, 1),
            spec("b183", 183, 1),
            spec("rows_neg", 184, 15),
            spec("k199", 199, 1),
            spec("k215", 215, 1),
        ],
        Fields::new(),
    )
});
/// Staging buffer -> ring bus -> the tiles in `tiles` (received by a ringConsumer).
pub static INFEED: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "infeed",
        &[0x26],
        4,
        vec![
            spec("stream", 12, 1),
            spec("buf_off", 14, 20),
            spec("d0_stride", 34, 21),
            spec("d0_limit", 55, 20),
            spec("d1_stride", 75, 21),
            spec("d1_limit", 96, 20),
            spec("d2_stride", 116, 21),
            spec("d2_limit", 137, 20),
            spec("k157", 157, 1),
            spec("k160", 160, 1),
            spec("par172", 172, 1),
            spec("av176", 176, 1),
            spec("count_m1", 183, 14),
            spec("k203", 203, 1),
            spec("count_neg", 205, 14),
            spec("k311", 311, 1),
            spec("par314", 314, 1),
            spec("pop_wait", 320, 16),
            spec("k336", 336, 1),
            spec("k352", 352, 1),
            spec("k392", 392, 1),
            spec("tiles", 409, 16),
            spec("k434", 434, 4),
            spec("f438", 438, 2),
        ],
        Fields::new(),
    )
});
/// Ring (from a ringProducer) -> host stream (DMA tag 3), or -> scalar memory.
pub static OUTFEED: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "outfeed",
        &[0x27],
        2,
        vec![
            spec("base", 12, 15),
            spec("d0_stride", 27, 16),
            spec("d0_limit", 43, 15),
            spec("d1_stride", 58, 16),
            spec("d1_limit", 74, 15),
            spec("d2_stride", 89, 16),
            spec("d2_limit", 105, 15),
            spec("d3_stride", 120, 16),
            spec("k173", 173, 2),
            spec("f222", 222, 7),
        ],
        Fields::new(),
    )
});
/// A scalar-core word as geohot's edgetpuxray decompiler sees it; bits 6..11
/// "branch" and bit 11 "enable_scalar" are the opcode.
pub static WORD: LazyLock<Layout> = LazyLock::new(|| {
    Layout::new(
        "word",
        &[],
        1,
        vec![
            spec("gate", 0, 1),
            spec("pred_reg", 1, 3),
            spec("yes_pred", 4, 1),
            spec("unk_0", 5, 1),
            spec("branch", 6, 5),
            spec("enable_scalar", 11, 1),
            spec("enable_vector", 12, 2),
            spec("vs_reg_v1", 14, 5),
            spec("imm_size", 19, 12),
            spec("v_op", 31, 5),
            spec("v_offset", 36, 8),
            spec("v_cmd", 44, 5),
            spec("vs_reg", 49, 5),
            spec("s_op", 54, 6),
            spec("s_x", 60, 5),
            spec("s_y", 65, 5),
            spec("imm_scalar", 70, 32),
            spec("v_op_2", 102, 3),
            spec("vs_reg_w", 105, 5),
            spec("unk_3", 110, 18),
        ],
        Fields::new(),
    )
});

/// Bit of the 32-bit immediate in an ALU word: where the runtime links addresses.
pub const IMM_BIT: usize = 70;

// *** fences ***

pub struct Sync {
    pub tiles: i64,
    pub counters: i64,
    pub count: i64,
    pub units: i64,
    pub signal: i64,
    pub b43: i64,
    pub f109: i64,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            tiles: 0xffff,
            counters: 0,
            count: 0,
            units: 0x3ff,
            signal: 0,
            b43: 0,
            f109: 0,
        }
    }
}

pub fn sync(seq: i64, s: &Sync) -> Vec<u128> {
    SYNC.encode(&fields(&[
        ("seq", seq),
        ("tiles", s.tiles),
        ("counters", s.counters),
        ("count", s.count),
        ("units", s.units),
        ("signal", s.signal),
        ("b43", s.b43),
        ("f109", s.f109),
    ]))
}

/// 2nd word of every program.
pub fn sync_init(caching: bool) -> Vec<u128> {
    sync(
        0,
        &Sync {
            counters: (1 << 19) - 1,
            signal: 1,
            b43: i64::from(caching),
            ..Sync::default()
        },
    )
}
/// Drain all units, then a token -> scsync_fence.
pub fn sync_signal(seq: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            signal: 1,
            ..Sync::default()
        },
    )
}
pub fn sync_final(seq: i64, caching: bool) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            signal: 1,
            f109: 3,
            b43: i64::from(caching),
            ..Sync::default()
        },
    )
}
/// Drain all units, no token.
pub fn sync_drain(seq: i64, tiles: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            tiles,
            ..Sync::default()
        },
    )
}
/// Starts each input's DMA.
pub fn sync_wn_fence(seq: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            units: 1 << 5,
            ..Sync::default()
        },
    )
}
/// All counters but RING_PRODUCER_A/B.
pub fn sync_reset17(seq: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            counters: (1 << 17) - 1,
            ..Sync::default()
        },
    )
}
/// The 8 mesh counters.
pub fn sync_reset_mesh(seq: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            counters: tc(&TILE_COUNTERS[3..11]),
            ..Sync::default()
        },
    )
}
/// After the outfeeds.
pub fn sync_rpa(seq: i64) -> Vec<u128> {
    sync(
        seq,
        &Sync {
            counters: tc(&["RING_PRODUCER_A"]),
            signal: 1,
            ..Sync::default()
        },
    )
}
/// Fence between the groups of a mesh phase (meshBus opcode `op`); `count` is the
/// phase's running packet count.
pub fn sync_mesh(seq: i64, op: u8, tiles: i64, count: i64) -> Vec<u128> {
    let (inn, out) = mesh_counters(op);
    sync(
        seq,
        &Sync {
            tiles,
            counters: tc(&[inn]) | tc(&[out]),
            count,
            units: 1 << (op - 0x0f),
            ..Sync::default()
        },
    )
}

pub struct Scsync {
    pub smask: i64,
    pub value: i64,
    pub tokwait: bool,
    /// `(counter index, threshold)`: sets `thr<k>` and the wait bit k.
    pub thresholds: Vec<(usize, i64)>,
    pub sunits: i64,
}

impl Default for Scsync {
    fn default() -> Self {
        Self {
            smask: 0,
            value: 0,
            tokwait: false,
            thresholds: Vec::new(),
            sunits: 0b01100,
        }
    }
}

pub fn scsync(s: &Scsync) -> Vec<u128> {
    let wait: i64 = s.thresholds.iter().map(|(k, _)| 1i64 << k).sum();
    let mut f = fields(&[
        ("smask", s.smask),
        ("value", s.value),
        ("sunits", s.sunits),
        ("wait", wait),
    ])
    .set("tiles", if s.tokwait || wait != 0 { 0xffff } else { 0 });
    if s.tokwait {
        f = f.set("tokwait", 1).set("tmask", 0xffff);
    }
    for (k, v) in &s.thresholds {
        f.put(&format!("thr{k}"), *v);
    }
    SCSYNC.encode(&f)
}

/// Follows a sync(signal=1).
pub fn scsync_fence() -> Vec<u128> {
    scsync(&Scsync {
        tokwait: true,
        ..Scsync::default()
    })
}
/// 2nd instruction of every program.
pub fn scsync_init() -> Vec<u128> {
    scsync(&Scsync {
        smask: 0x1ff,
        tokwait: true,
        sunits: 0b11111,
        ..Scsync::default()
    })
}
/// Before each input.
pub fn scsync_set_av() -> Vec<u128> {
    scsync(&Scsync {
        smask: scm(&["AVDATA_POP", "AVDATA_INFEED"]),
        ..Scsync::default()
    })
}
/// Right before avDataPop.
pub fn scsync_av_credit() -> Vec<u128> {
    scsync(&Scsync {
        smask: scm(&["AVDATA_INFEED"]),
        value: 0x1fff,
        ..Scsync::default()
    })
}
pub fn scsync_set_avpop() -> Vec<u128> {
    scsync(&Scsync {
        smask: scm(&["AVDATA_POP"]),
        ..Scsync::default()
    })
}
/// Scalar-unit fence only.
pub fn scsync_nop() -> Vec<u128> {
    scsync(&Scsync::default())
}
/// n = outfeeds of the output.
pub fn scsync_wait_pa(n: i64) -> Vec<u128> {
    scsync(&Scsync {
        smask: scm(&["PRODUCER_A"]),
        thresholds: vec![(5, n)],
        ..Scsync::default()
    })
}
// *** host streams: pops fill the staging buffer, infeeds send buffer ranges over
// the ring, outfeeds drain the ring to the host ***

/// Buffer passes, units in the last.
fn passes(units: i64, buf: i64) -> (i64, i64) {
    let n = cdiv_i64(units, buf);
    (n, units - (n - 1) * buf)
}

/// avDataPop of a contiguous activation input of `nbytes` (a multiple of 8).
pub fn av_pop(nbytes: i64) -> Vec<u128> {
    assert!(nbytes % 8 == 0 && nbytes > 0, "av_pop: {nbytes} bytes");
    let mut f = fields(&[
        ("stream", 1),
        ("d0_stride", 1),
        ("k121", 1),
        ("k124", 1),
        ("av140", 1),
        ("k174", 1),
        ("k178", 1),
        ("k199", 1),
        ("k215", 1),
    ]);
    if nbytes == 8 {
        return POP.encode(&f);
    }
    let (n, last) = passes(nbytes / 8, AV_BUF);
    f = f.merge(fields(&[
        ("d0_limit", AV_BUF - 1),
        ("d1_stride", neg(AV_BUF - 1, 16)),
        ("d1_limit", n - 1),
        ("d2_stride", neg(AV_BUF - 1, 16)),
    ]));
    if last != AV_BUF {
        f = f.merge(fields(&[
            ("count_m1", last - 1),
            ("k157", 1),
            ("count_neg", neg(last, 14)),
        ]));
    }
    POP.encode(&f)
}

/// parameterPop of a PARAMETER_CACHING program (`nbytes` a multiple of 64).
pub fn param_pop(nbytes: i64) -> Vec<u128> {
    let u = nbytes / 64;
    let (n, last) = passes(u, PAR_BUF);
    let mut f = fields(&[
        ("stream", 0),
        ("d0_stride", 1),
        ("d0_limit", PAR_BUF - 1),
        ("d1_stride", neg(PAR_BUF - 1, 16)),
        ("d1_limit", n - 1),
        ("d2_stride", neg(PAR_BUF - 1, 16)),
        ("k121", 1),
        ("k124", 1),
        ("par136", 1),
        ("k174", 1),
        ("par177", 1),
        ("k178", 1),
        ("b183", i64::from(u % 2 == 0 || u >= 2048)),
        ("rows_neg", neg((u / 2).min(1024), 15)),
        ("k199", 1),
        ("k215", 1),
    ]);
    if last != PAR_BUF {
        f = f.merge(fields(&[
            ("count_m1", last - 1),
            ("k157", 1),
            ("count_neg", neg(last, 10)),
        ]));
    }
    POP.encode(&f)
}

/// Infeed of `nbytes` activations from byte `offset` of the input, multicast on
/// the ring to `tiles`. A transfer longer than the staging buffer takes several
/// passes; its offsets wrap.
pub fn av_infeed(offset: i64, nbytes: i64, tiles: i64) -> Vec<u128> {
    assert!(
        offset % 8 == 0 && nbytes % 8 == 0 && nbytes > 0,
        "av_infeed: {nbytes} bytes at {offset}"
    );
    let (off, u) = (offset / 8, nbytes / 8);
    let (n, last) = passes(u, AV_BUF);
    let wrap = u > AV_BUF;
    let mut f = fields(&[
        ("stream", 1),
        ("buf_off", if wrap { off % (1 << 15) } else { off }),
        ("d0_stride", 1),
        ("k157", 1),
        ("k160", 1),
        ("av176", 1),
        ("count_m1", last - 1),
        ("k311", 1),
        (
            "pop_wait",
            if wrap { (off + 1) % (1 << 16) } else { off + 1 },
        ),
        ("k336", 1),
        ("k352", 1),
        ("k392", 1),
        ("tiles", tiles),
        ("k434", 0xf),
        ("f438", 3),
    ]);
    if u > 1 {
        f = f.merge(fields(&[
            ("d0_limit", AV_BUF - 1),
            ("d1_stride", neg(AV_BUF - 1, 21)),
            ("d1_limit", n - 1),
            ("d2_stride", neg(AV_BUF - 1, 21)),
            ("k203", 1),
            ("count_neg", neg(last, 14)),
        ]));
    }
    INFEED.encode(&f)
}

/// Infeed of `nbytes` of the parameter blob from byte `offset` to `tiles`
/// (PARAMETER_CACHING); `lanes` = 64-byte lanes per weight row.
pub fn param_infeed(offset: i64, nbytes: i64, tiles: i64, lanes: i64) -> Vec<u128> {
    let (off, u) = (offset / 64, nbytes / 64);
    let (n, last) = passes(u, PAR_BUF);
    let mut f = fields(&[
        ("stream", 0),
        ("buf_off", off % 2048),
        ("d0_stride", 1),
        ("d0_limit", PAR_BUF - 1),
        ("d1_stride", neg(PAR_BUF - 1, 21)),
        ("d1_limit", n - 1),
        ("d2_stride", neg(PAR_BUF - 1, 21)),
        ("k157", 1),
        ("k160", 1),
        ("par172", 1),
        ("k311", 1),
        ("par314", 1),
        ("pop_wait", (off + 1) % (1 << 16)),
        ("k336", 1),
        ("k352", 1),
        ("k392", 1),
        ("tiles", tiles),
        ("k434", 0xf),
        ("f438", lanes - 1),
    ]);
    if last != PAR_BUF {
        f = f.merge(fields(&[
            ("count_m1", last - 1),
            ("k203", 1),
            ("count_neg", neg(last, 10)),
        ]));
    }
    INFEED.encode(&f)
}

/// Host outfeed of `nbytes` (a multiple of 8) from the ring (one per ringProducer).
pub fn outfeed(nbytes: i64) -> Vec<u128> {
    assert!(nbytes % 8 == 0 && nbytes > 0, "outfeed: {nbytes} bytes");
    OUTFEED.encode(&fields(&[
        ("d0_stride", i64::from(nbytes == 8)),
        ("d0_limit", nbytes / 8 - 1),
        ("k173", 3),
        ("f222", 0x7f),
    ]))
}

// *** host DMA descriptors, built by the scalar ALU (0x20): v_op=0xa pushes
// s[vs_reg] into descriptor word v_offset (address lo, address hi, length, tag),
// v_op=0xc issues it ***

#[derive(Default)]
pub struct Alu {
    pub s_op: i64,
    pub s_x: i64,
    pub s_y: i64,
    pub imm: i64,
    pub pred: Option<i64>,
    pub v_op: i64,
    pub v_offset: i64,
    pub vs_reg: i64,
}

/// `s_x = s_y <s_op> imm` (`s_op + 0x20`: immediate form; 1 ADD 3 AND 0xa NEQ 0xc LT
/// signed 0xf MOV), run iff `p<pred>` if given.
pub fn alu(a: &Alu) -> u128 {
    let mut f = fields(&[
        ("enable_scalar", 1),
        ("s_op", a.s_op),
        ("s_x", a.s_x),
        ("s_y", a.s_y),
        ("imm_scalar", a.imm & 0xffff_ffff),
        ("v_op", a.v_op),
        ("v_offset", a.v_offset),
        ("vs_reg", a.vs_reg),
    ]);
    if let Some(p) = a.pred {
        f = f.merge(fields(&[("gate", 1), ("pred_reg", p), ("yes_pred", 1)]));
    }
    WORD.encode(&f)[0]
}

fn op3(s_op: i64, s_x: i64, s_y: i64, imm: i64) -> u128 {
    alu(&Alu {
        s_op,
        s_x,
        s_y,
        imm,
        ..Alu::default()
    })
}

fn op3p(s_op: i64, s_x: i64, s_y: i64, imm: i64, pred: i64) -> u128 {
    alu(&Alu {
        s_op,
        s_x,
        s_y,
        imm,
        pred: Some(pred),
        ..Alu::default()
    })
}

pub fn movi(rd: i64, imm: i64) -> u128 {
    op3(0x2f, rd, 0, imm)
}

/// `(out_hi:out_lo) = (hi:lo) + off` for `0 <= off < 2^31`, the carry through
/// predicates p0/p1 (8 words, as edgetpu_compiler emits them).
pub fn add64(hi: i64, lo: i64, off: i64, out_hi: i64, out_lo: i64, tmp: i64) -> Vec<u128> {
    assert!((0..1 << 31).contains(&off), "add64: offset {off}");
    vec![
        op3(0x2c, 0, lo, 0),  // p0 = lo < 0 (bit 31 of lo)
        op3(0x0a, 1, lo, lo), // p1 = false
        op3(0x23, tmp, lo, 0x7fff_ffff),
        op3p(0x21, tmp, tmp, off, 0),
        op3p(0x2c, 1, tmp, 0, 0), // p0: p1 = carry out of bit 31
        op3(0x21, out_lo, lo, off),
        op3(0x21, out_hi, hi, 0),
        op3p(0x21, out_hi, out_hi, 1, 1),
    ]
}

pub fn push_issue(
    lo: i64,
    hi: i64,
    len_reg: i64,
    tag_reg: i64,
    length: i64,
    tag: i64,
) -> Vec<u128> {
    vec![
        alu(&Alu {
            v_op: 0xa,
            v_offset: 0,
            vs_reg: lo,
            ..Alu::default()
        }),
        alu(&Alu {
            s_op: 0x2f,
            s_x: len_reg,
            imm: length,
            v_op: 0xa,
            v_offset: 1,
            vs_reg: hi,
            ..Alu::default()
        }),
        alu(&Alu {
            s_op: 0x2f,
            s_x: tag_reg,
            imm: tag,
            v_op: 0xa,
            v_offset: 2,
            vs_reg: len_reg,
            ..Alu::default()
        }),
        alu(&Alu {
            v_op: 0xa,
            v_offset: 3,
            vs_reg: tag_reg,
            ..Alu::default()
        }),
        alu(&Alu {
            v_op: 0xc,
            ..Alu::default()
        }),
    ]
}

/// `(s[r+3]:s[r+2]) = (s[r]:s[r+1]) + off2`, then push and issue the descriptor
/// from `s[r+2..r+5]`.
fn descriptor(r: i64, off2: i64, length: i64, tag: i64) -> Vec<u128> {
    let mut words = add64(r, r + 1, off2, r + 3, r + 2, r + 6);
    words.extend(push_issue(r + 2, r + 3, r + 4, r + 5, length, tag));
    words
}

/// Which registers hold a DMA's base address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regs {
    /// An input / output: address in s11:s12, scratch s4..s10.
    Act,
    /// The parameters of a caching program: address s0:s1, scratch s2..s8.
    Par,
}

/// Push and issue a host DMA descriptor of the relocated buffer address + `off` +
/// `off2`. `load_base = (hi, lo)` also emits the two MOVIs the runtime relocates
/// (a caching program loads lo first).
pub fn host_dma(
    tag: i64,
    length: i64,
    off: i64,
    off2: i64,
    regs: Regs,
    load_base: Option<(i64, i64)>,
) -> Vec<u128> {
    let (hi, lo, r) = match regs {
        Regs::Act => (11, 12, 4),
        Regs::Par => (0, 1, 2),
    };
    let mut words = Vec::new();
    if let Some((base_hi, base_lo)) = load_base {
        match regs {
            Regs::Act => words.extend([movi(hi, base_hi), movi(lo, base_lo)]),
            Regs::Par => words.extend([movi(lo, base_lo), movi(hi, base_hi)]),
        }
    }
    words.extend(add64(hi, lo, off, r, r + 1, r + 2));
    words.extend(descriptor(r, off2, length, tag));
    words
}

/// The completion interrupt sc_int_0: a descriptor with tag 4, address 0, length 0.
pub fn interrupt(caching: bool) -> Vec<u128> {
    let r = if caching { 2 } else { 4 };
    let mut words = vec![movi(r, 0), movi(r + 1, 0)];
    words.extend(descriptor(r, 0, 0, TAG_INT0));
    words
}

// *** the program skeleton; seq = tile instructions (tile ops + syncs) before the block's first sync ***

/// `nwords`: the whole program, start and end included.
pub fn start(nwords: i64) -> Vec<u128> {
    START.encode(&fields(&[("length_bits", 128 * (nwords - 2))]))
}
pub fn end() -> Vec<u128> {
    END.encode(&fields(&[("length_bits", 128)]))
}
pub fn halt(code: i64) -> Vec<u128> {
    HALT.encode(&fields(&[("code", code)]))
}
/// 4 of these follow halt.
pub fn nop() -> Vec<u128> {
    WORD.encode(&fields(&[("enable_scalar", 1)]))
}

/// The program bytes: start, body, end.
pub fn program(body: &[u128]) -> Vec<u8> {
    let mut words = start(body.len() as i64 + 2);
    words.extend_from_slice(body);
    words.extend(end());
    words_to_bytes(&words)
}

/// After start: init fence, then s1/s0 = PARAMETER lo/hi, s3/s2 = SCRATCH lo/hi (relocated MOVIs).
pub fn exe_prologue() -> Vec<u128> {
    let mut words = sync_init(false);
    words.extend(scsync_init());
    words.extend([movi(1, 0), movi(0, 0), movi(3, 0), movi(2, 0)]);
    words
}

/// Drain all tile units, wait for their tokens; 1 seq.
pub fn signal_fence(seq: i64) -> Vec<u128> {
    let mut words = sync_signal(seq);
    words.extend(scsync_fence());
    words
}

/// Starts every input; 1 seq.
pub fn input_head(seq: i64) -> Vec<u128> {
    let mut words = scsync_set_av();
    words.extend(signal_fence(seq));
    words
}

/// s11/s12 = INPUT hi/lo (relocated: the first two words), credit, avDataPop, host
/// DMA (tag 1) of `nbytes` at `off` into the input.
pub fn input_dma(nbytes: i64, off: i64) -> Vec<u128> {
    let mut words = vec![movi(11, 0), movi(12, 0)];
    words.extend(scsync_av_credit());
    words.extend(scsync_set_avpop());
    words.extend(av_pop(nbytes));
    words.extend(host_dma(TAG_INPUT, nbytes, off, 0, Regs::Act, None));
    words
}

/// s11/s12 = OUTPUT hi/lo (relocated: the first two words).
pub fn output_dma_head(off: i64) -> Vec<u128> {
    let mut words = vec![movi(11, 0), movi(12, 0)];
    words.extend(add64(11, 12, off, 4, 5, 6));
    words
}

pub fn output_dma_tail(nbytes: i64) -> Vec<u128> {
    let mut words = add64(4, 5, 0, 7, 6, 10);
    words.extend(push_issue(6, 7, 8, 9, nbytes, TAG_OUTPUT));
    words
}

/// After the (outfeed, ringProducer) pairs: wait PRODUCER_A >= `n_outfeeds`, reset
/// RING_PRODUCER_A (2 seqs).
pub fn output_wait(seq: i64, n_outfeeds: i64) -> Vec<u128> {
    let mut words = sync_reset17(seq);
    words.extend(scsync_wait_pa(n_outfeeds));
    words.extend(sync_rpa(seq + 1));
    words.extend(scsync_fence());
    words
}

/// Final fence, completion interrupt, halt, 4 nops (end follows); 1 seq.
pub fn epilogue(seq: i64, caching: bool) -> Vec<u128> {
    let mut words = sync_final(seq, caching);
    words.extend(scsync_fence());
    words.extend(interrupt(caching));
    words.extend(halt(4));
    for _ in 0..4 {
        words.extend(nop());
    }
    words
}

/// `isa::ttu` with the default count name, for the scalar-side recipes.
pub fn ttu(p: &str, strides: &[i64], counts: &[i64], levels: usize) -> Fields {
    ttu_generic(p, strides, counts, levels, "cnt")
}
