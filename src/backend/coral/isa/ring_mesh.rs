//! Inter-tile data movement DMAs:
//!   0x10 ringProducer   4 words  narrow/wide memory -> ring, multicast to a destination bitmap
//!   0x11 ringConsumer   4 words  ring -> memory (parameters in caching programs, activations in execution programs)
//!   0x12 ringConsumer1  4 words  same layout; receives the packets sent with to_c1=1
//!   0x15 / 0x16 / 0x17 / 0x18 meshBus, 6 words: the opcode is the direction the data moves, south (t -> t+4),
//!        west (t -> t-1), north (t -> t-4), east (t -> t+1), with tile t at row t//4, column t%4.
//!        An outbound (o_) and an inbound (i_) half.

use super::{FieldSpec, Fields, Layout, spec, sspec};
use std::sync::LazyLock;

/// Bit 16 of the ring destination bitmap = the scalar core (outfeed).
pub const SCALAR_CORE: i64 = 16;

/// One address generator: 18-bit address, 4 loop dims (signed increment, count - 1), the buffer / sync block.
fn desc(p: &str, off: usize) -> Vec<FieldSpec> {
    let mut f = vec![spec(&format!("{p}addr"), 60 + off, 18)];
    for j in 0..4 {
        f.push(sspec(&format!("{p}inc{j}"), 78 + off + 33 * j, 17));
        f.push(spec(&format!("{p}cnt{j}"), 95 + off + 33 * j, 16));
    }
    f.extend([
        spec(&format!("{p}sdims"), 210 + off, 4), // 1 = plain transfer; a mask of the used levels (1/3/7) for multi-dim mesh moves
        spec(&format!("{p}cbuf"), 214 + off, 2),  // 1 = circular staging buffer in use (with slots)
        spec(&format!("{p}slots"), 216 + off, 11), // buffer depth: ring = packets in stream, mesh forwarder = 4
        spec(&format!("{p}rsv_s"), 227 + off, 6),
        spec(&format!("{p}grp"), 233 + off, 10), // ring consumer: packets per tile group - 1 (multi-packet groups)
    ]);
    f
}

/// 41-bit sync record: id (ring: op<<5 | tile SyncCounter, mesh: counter<<1 | op), signed value, enable a, enable b.
fn rec(p: &str, lo: usize, idw: usize, valw: usize) -> Vec<FieldSpec> {
    let q = lo + idw + valw;
    let mut f = vec![
        spec(&format!("{p}id"), lo, idw),
        sspec(&format!("{p}val"), lo + idw, valw),
        spec(&format!("{p}en_a"), q, 1),
        spec(&format!("{p}x"), q + 1, 15),
        spec(&format!("{p}en_b"), q + 16, 1),
    ];
    if q + 17 < lo + 41 {
        f.push(spec(&format!("{p}y"), q + 17, lo + 24 - q));
    }
    f
}

pub static RING_PRODUCER: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![spec("tile_mask", 12, 16), spec("seq", 46, 14)];
    f.extend(desc("", 0));
    f.extend([
        spec("mode", 335, 2), // 3 = to the scalar core (output), 1 = tile -> tile activation forward, 0 = other
        spec("pcfg", 337, 6), // 20 output, 0 forward, 4 / 12 parameter broadcast
    ]);
    f.extend(rec("r0_", 343, 7, 16));
    f.extend(rec("r1_", 384, 7, 16));
    f.extend(rec("r2_", 425, 7, 16));
    f.extend([
        spec("to_c1", 466, 1), // 1 = the packets go to ringBusConsumer1 (0x12)
        spec("dest", 467, 17), // destination bitmap: bit t = tile t, bit 16 = the scalar core
    ]);
    Layout::new("ringProducer", &[0x10], 4, f, Fields::new())
});

pub static RING_CONSUMER: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![spec("tile_mask", 12, 16), spec("seq", 46, 14)];
    f.extend(desc("", 0));
    f.extend([
        spec("gstride", 249, 18), // multi-packet groups: 4*(G-1)*p + 1 (G groups of p packets)
        spec("aux_en0", 267, 1),
        spec("aux_addr0", 268, 18),
        spec("aux_addr1", 300, 18),
        spec("aux_en1", 318, 1), // caching: parameter relocation fields
        spec("mode", 335, 2),    // 3 = infeed-fed / parameter, 1 = fed by a tile's ringProducer
        spec("s_id", 339, 7),
        sspec("s_val", 346, 16),
        spec("s_en_a", 362, 1),
        spec("s_cnt", 368, 10),
        spec("s_en_b", 378, 1),
        spec("s_y", 379, 1),
    ]);
    Layout::new("ringConsumer", &[0x11, 0x12], 4, f, Fields::new())
});

pub static MESH: LazyLock<Layout> = LazyLock::new(|| {
    let mut f = vec![spec("tile_mask", 12, 16), spec("seq", 46, 14)];
    f.extend(desc("o_", 0));
    f.extend(desc("i_", 205));
    f.extend([spec("out_mode", 473, 2), spec("in_mode", 477, 3)]);
    for k in 0..5 {
        f.extend(rec(&format!("s{k}_"), 480 + 41 * k, 6, 18));
    }
    f.extend([
        spec("fill_en", 727, 1), // the inbound half generates a constant instead of receiving (edge padding)
        spec("fill", 728, 32), // 32-bit fill pattern, e.g. 0x80808080 = zero point 128 in every byte
    ]);
    Layout::new("mesh", &[0x16, 0x17, 0x15, 0x18], 6, f, Fields::new())
});

pub fn encode_ring_producer(f: &Fields) -> Vec<u128> {
    RING_PRODUCER.encode(f)
}

pub fn encode_ring_consumer(f: &Fields) -> Vec<u128> {
    RING_CONSUMER.encode(f)
}

pub fn encode_mesh(f: &Fields) -> Vec<u128> {
    MESH.encode(f)
}
