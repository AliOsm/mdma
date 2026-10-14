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

pub mod attention;
pub mod block;
pub mod conv2d;
pub mod eltops;
pub mod fc;
pub mod layer;

use super::executable::{Bitstream, Description, FieldOffset};
use super::isa::scalar::{self as sc, IMM_BIT};
use super::isa::{self, Fields, f32_bits, fields, op, ring_mesh as rm};
use super::{Error, Result};

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

    /// The tile-op fields that hold this quantization.
    pub fn fields(&self) -> Fields {
        fields(&[
            ("w_zp", i64::from(self.w_zp)),
            ("in_zp", i64::from(self.in_zp)),
            ("out_zp", i64::from(self.out_zp)),
            ("mult_bits", f32_bits(self.mult)),
            ("clamp_max_bits", f32_bits(self.clamp_max())),
            ("clamp_min_bits", f32_bits(self.clamp_min())),
        ])
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

    /// Several tile instructions, one sequence number each.
    pub fn tiles(&mut self, insns: Vec<Vec<u128>>) {
        for insn in insns {
            self.tile(insn);
        }
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

    /// Program word index of the next instruction (the start word comes first).
    pub fn here(&self) -> usize {
        self.words.len() + 1
    }

    /// The program bytes and the relocations into them.
    pub fn program(self) -> (Vec<u8>, Vec<FieldOffset>) {
        (sc::program(&self.words), self.relocs)
    }
}

/// Most words of one bitstream, start and end included.
pub const MAX_WORDS: usize = 16384;

/// The emitted stream (no start / end) cut into bitstreams of at most `MAX_WORDS`
/// words as edgetpu_compiler does: groups that each start at a tile sync (0x1a),
/// packed greedily; a cut bitstream ends with halt (code 1), 4 nops and end, the
/// next one starts with start (the sequence numbers continue). Relocations move
/// with their words.
pub fn split_bitstreams(e: &Emitter) -> Vec<Bitstream> {
    let words = &e.words;
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let n = isa::length(isa::opcode(words[i]));
        if groups.is_empty() || isa::opcode(words[i]) == 0x1a {
            groups.push(i..i);
        }
        i = (i + n).min(words.len());
        if let Some(last) = groups.last_mut() {
            last.end = i;
        }
    }
    let mut trailer = sc::halt(1);
    for _ in 0..4 {
        trailer.extend(sc::nop());
    }
    // (body word ranges, trailer?) per bitstream.
    let mut chunks: Vec<(Vec<std::ops::Range<usize>>, bool)> = Vec::new();
    let mut cur: Vec<std::ops::Range<usize>> = Vec::new();
    let mut len = 0;
    for gw in groups {
        if !cur.is_empty() && 1 + len + gw.len() + trailer.len() + 1 > MAX_WORDS {
            chunks.push((std::mem::take(&mut cur), true));
            len = 0;
        }
        len += gw.len();
        cur.push(gw);
    }
    chunks.push((cur, false));
    chunks
        .into_iter()
        .map(|(ranges, cut)| {
            let mut body = Vec::new();
            let mut relocs = Vec::new();
            for r in ranges {
                for reloc in &e.relocs {
                    let word = (reloc.offset_bit - IMM_BIT) / 128 - 1;
                    if r.contains(&word) {
                        let mut moved = reloc.clone();
                        moved.offset_bit = 128 * (body.len() + word - r.start + 1) + IMM_BIT;
                        relocs.push(moved);
                    }
                }
                body.extend_from_slice(&words[r]);
            }
            if cut {
                body.extend_from_slice(&trailer);
            }
            Bitstream {
                data: sc::program(&body),
                relocs,
            }
        })
        .collect()
}

/// Rewrites the quantization of the tile op at program word `word`: the requant
/// immediates sit in the instruction stream, so a loaded program takes new
/// activation scales without being generated again.
pub fn patch_quant(program: &mut [u8], word: usize, quant: &Quant) -> Result<()> {
    op::OP
        .patch(program, word, &quant.fields())
        .ok_or_else(|| Error::Codegen(format!("no tile op at program word {word}")))
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
