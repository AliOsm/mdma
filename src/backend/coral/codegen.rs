//! Code generation for the Edge TPU, checked word for word against the oracle
//! corpus of compiler-exact programs in `data/fixtures/coral/fc_corpus`.
//! The emitter collects instruction words and numbers the tile-dispatched ones;
//! generators build the PARAMETER_CACHING program that loads weights into the
//! tiles' wide memory and the EXECUTION_ONLY program that runs one call.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "geometry is small integers moved between usize and i64"
)]

pub mod fc;

use super::executable::{Description, FieldOffset};
use super::isa::scalar::{self as sc, IMM_BIT};
use super::isa::{Fields, ring_mesh as rm};

/// 64-byte units: the execution programs' own wide buffers sit right below this,
/// above every parameter region.
pub const WIDE_TOP: i64 = 8320;
/// narrowToWide output -> ringProducer -> outfeed.
pub const WIDE_OUT_FIFO: i64 = 8312;

/// A quantized matmul's numerics: `y = requant(x @ W.T + b)`, uint8 everywhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quant {
    pub in_zp: u8,
    pub w_zp: u8,
    pub out_zp: u8,
    /// `in_scale * w_scale / out_scale`.
    pub mult: f32,
    /// Clamp in the quantized output domain (fused ReLU etc.).
    pub lo: u8,
    pub hi: u8,
}

impl Quant {
    /// The quantization of the compiler references.
    pub const REFERENCE: Self = Self {
        in_zp: 128,
        w_zp: 128,
        out_zp: 128,
        mult: (1.0 / 32.0) * (1.0 / 64.0) / (1.0 / 4.0),
        lo: 0,
        hi: 255,
    };

    pub const fn new(in_zp: u8, w_zp: u8, out_zp: u8, mult: f32) -> Self {
        Self {
            in_zp,
            w_zp,
            out_zp,
            mult,
            lo: 0,
            hi: 255,
        }
    }

    pub fn clamp_min(&self) -> f32 {
        f32::from(self.lo) - f32::from(self.out_zp)
    }

    pub fn clamp_max(&self) -> f32 {
        f32::from(self.hi) - f32::from(self.out_zp)
    }
}

/// Collects 128-bit words; `seq` counts the tile-dispatched instructions
/// (0x01-0x19) and the tile fences (0x1a). Records where the host-address MOVIs
/// are, as relocations of the finished program.
#[derive(Default)]
pub struct Emitter {
    pub words: Vec<u128>,
    pub seq: i64,
    relocs: Vec<FieldOffset>,
}

impl Emitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tile instructions: each takes one sequence number.
    pub fn tile(&mut self, insn: Vec<u128>) {
        self.words.extend(insn);
        self.seq += 1;
    }

    /// `f(seq)` holds exactly one tile fence.
    pub fn sync(&mut self, f: impl FnOnce(i64) -> Vec<u128>) {
        let words = f(self.seq);
        self.tile(words);
    }

    /// Scalar words; `seqs`: tile fences inside a skeleton block.
    pub fn scalar(&mut self, words: Vec<u128>, seqs: i64) {
        self.words.extend(words);
        self.seq += seqs;
    }

    /// Scalar words whose first two are the MOVIs of a base address (`hi` then `lo`,
    /// or the reverse), relocated by the runtime.
    pub fn scalar_with_base(
        &mut self,
        words: Vec<u128>,
        desc: Description,
        name: Option<&str>,
        lo_first: bool,
    ) {
        let at = self.words.len();
        let (hi, lo) = if lo_first { (at + 1, at) } else { (at, at + 1) };
        for (word, upper) in [(hi, true), (lo, false)] {
            // The program gets a start word in front of the body.
            self.relocs.push(FieldOffset {
                desc,
                batch: 0,
                name: name.map(str::to_owned),
                upper,
                offset_bit: 128 * (word + 1) + IMM_BIT,
            });
        }
        self.scalar(words, 0);
    }

    /// The program bytes and the relocations into them.
    pub fn program(self) -> (Vec<u8>, Vec<FieldOffset>) {
        (sc::program(&self.words), self.relocs)
    }
}

/// One caching part: the tiles it lands on, their ringConsumer fields (without
/// seq), blob byte offset, bytes, 64-byte lanes per weight row.
pub struct CachingPart {
    pub tiles: i64,
    pub consumer: Fields,
    pub offset: i64,
    pub bytes: i64,
    pub lanes: i64,
}

/// PARAMETER_CACHING of an `nbytes` blob: per part its ringConsumers, then its
/// parameter infeeds (to the ringConsumer's tiles).
pub fn caching_program(nbytes: i64, parts: &[Vec<CachingPart>]) -> (Vec<u8>, Vec<FieldOffset>) {
    let mut e = Emitter::new();
    let mut head = sc::sync_init(true);
    head.extend(sc::scsync_init());
    e.scalar(head, 1);
    e.scalar_with_base(
        sc::host_dma(
            sc::TAG_PARAMETERS,
            nbytes,
            0,
            0,
            sc::Regs::Par,
            Some((0, 0)),
        ),
        Description::Parameter,
        None,
        true,
    );
    e.scalar(sc::param_pop(nbytes), 0);
    for part in parts {
        for p in part {
            let seq = e.seq;
            e.tile(rm::encode_ring_consumer(
                &p.consumer.clone().set("seq", seq),
            ));
        }
        for p in part {
            e.scalar(sc::param_infeed(p.offset, p.bytes, p.tiles, p.lanes), 0);
        }
    }
    e.sync(|seq| sc::epilogue(seq, true));
    e.program()
}
