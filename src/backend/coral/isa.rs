//! The Edge TPU ("beagle") instruction set, reverse engineered from
//! edgetpu_compiler output. An instruction is one or more little-endian 128-bit words;
//! bits 6..12 of the first word are the opcode, which fixes the length. Every
//! instruction is a `Layout`: named bit fields over the words, encoded from the
//! fields a builder sets plus the layout's defaults. The generators are checked
//! word for word against the oracle corpus, so the field tables here must match the
//! decoded layouts exactly, reserved gaps included.

#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss, reason = "field values and geometry are small integers moved between usize and i64")]

pub mod op;
pub mod ring_mesh;
pub mod scalar;
pub mod wide_narrow;

use std::collections::{BTreeMap, HashMap};

pub const WORD_BITS: usize = 128;
pub const WORD_BYTES: usize = 16;

pub const fn cdiv(a: usize, b: usize) -> usize { a.div_ceil(b) }

pub const fn round_up(a: usize, b: usize) -> usize { cdiv(a, b) * b }

/// Ceiling division of non-negative field values.
pub const fn cdiv_i64(a: i64, b: i64) -> i64 {
    assert!(a >= 0 && b > 0, "cdiv_i64 of negative values");
    (a + b - 1) / b
}

/// The bits of an f32 as a field value.
pub fn f32_bits(x: f32) -> i64 { i64::from(x.to_bits()) }

pub const fn opcode(word: u128) -> u8 { ((word >> 6) & 0x3f) as u8 }

/// `(-x) mod 2^bits`: negative counts the hardware stores as unsigned.
pub const fn neg(x: i64, bits: u32) -> i64 { (-x).rem_euclid(1 << bits) }

/// Instruction lengths in words by opcode.
pub fn length(op: u8) -> usize {
    match op {
        0x00 => 14,
        0x01 | 0x02 => 17,
        0x03 => 8,
        0x05 | 0x10 | 0x11 | 0x12 | 0x1e | 0x26 => 4,
        0x06 | 0x29 => 3,
        0x07 | 0x0c | 0x13 | 0x14 => 7,
        0x0b | 0x24 | 0x25 | 0x27 | 0x34 => 2,
        0x15..=0x18 => 6,
        0x19 => 13,
        0x1c | 0x1d => 5,
        0x22 => 29,
        0x2a | 0x32 => 15,
        0x30 => 11,
        0x39 => 35,
        0x3c => 12,
        _ => 1,
    }
}

/// Field values a builder sets, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields(BTreeMap<String, i64>);

impl Fields {
    pub fn new() -> Self { Self::default() }

    #[must_use]
    pub fn set(mut self, name: &str, value: i64) -> Self {
        self.0.insert(name.into(), value);
        self
    }

    pub fn put(&mut self, name: &str, value: i64) { self.0.insert(name.into(), value); }

    /// `other`'s values win.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self
    }

    pub fn get(&self, name: &str) -> Option<i64> { self.0.get(name).copied() }

    pub fn iter(&self) -> impl Iterator<Item = (&str, i64)> { self.0.iter().map(|(name, value)| (name.as_str(), *value)) }
}

/// `fields(&[("a", 1), ("b", 2)])`.
pub fn fields(pairs: &[(&str, i64)]) -> Fields {
    let mut out = Fields::new();
    for (name, value) in pairs {
        out.put(name, *value);
    }
    out
}

/// Fields `{p}inc{d}` and `{p}{cnt}{d}` (count - 1) of an address generator loop
/// nest given innermost first, padded to `levels` with stride 0 and count 1. The
/// hardware adds `inc_d` when level d steps and the inner levels wrap, so
/// `inc_d = stride_d - sum_{e<d} (count_e - 1) * stride_e`.
pub fn ttu(p: &str, strides: &[i64], counts: &[i64], levels: usize, cnt: &str) -> Fields {
    assert!(strides.len().max(counts.len()) <= levels && counts.iter().all(|c| *c >= 1), "ttu {p:?}: bad loop nest");
    let mut out = Fields::new();
    let mut acc = 0i64;
    for d in 0..levels {
        let stride = strides.get(d).copied().unwrap_or(0);
        let count = counts.get(d).copied().unwrap_or(1);
        out.put(&format!("{p}inc{d}"), stride - acc);
        out.put(&format!("{p}{cnt}{d}"), count - 1);
        acc += stride * (count - 1);
    }
    out
}

/// A named bit range: `(name, low bit, width, signed)`.
pub type FieldSpec = (String, usize, usize, bool);

pub fn spec(name: &str, lo: usize, width: usize) -> FieldSpec { (name.into(), lo, width, false) }

pub fn sspec(name: &str, lo: usize, width: usize) -> FieldSpec { (name.into(), lo, width, true) }

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub lo: usize,
    pub width: usize,
    pub signed: bool,
}

/// A fixed-length instruction: the predication header, the fields, and reserved
/// fields `rsv<lo>` over every gap, so `encode(decode(words)) == words` for any
/// bit pattern and generators may set a reserved field by name.
#[derive(Debug)]
pub struct Layout {
    pub name: &'static str,
    pub ops: Vec<u8>,
    pub nwords: usize,
    pub fields: Vec<Field>,
    index: HashMap<String, usize>,
    defaults: Fields,
}

/// `gate`, `pred_reg`, `pred_pol`, `opcode`: if gate, run iff `p[pred_reg] == pred_pol`.
fn header() -> Vec<FieldSpec> { vec![spec("gate", 0, 1), spec("pred_reg", 1, 3), spec("pred_pol", 4, 1), spec("opcode", 6, 6)] }

impl Layout {
    /// `ops` are the opcodes this layout decodes; `encode` uses the first.
    pub fn new(name: &'static str, ops: &[u8], nwords: usize, mut specs: Vec<FieldSpec>, defaults: Fields) -> Self {
        if !ops.is_empty() {
            specs.extend(header());
        }
        specs.sort_by_key(|(_, lo, _, _)| *lo);
        let mut fields = Vec::new();
        let mut pos = 0;
        for (field_name, lo, width, signed) in specs.into_iter().chain(std::iter::once((String::new(), WORD_BITS * nwords, 0, false))) {
            assert!(lo >= pos, "{name}: field {field_name} overlaps");
            if lo > pos {
                fields.push(Field { name: format!("rsv{pos}"), lo: pos, width: lo - pos, signed: false });
            }
            if width > 0 {
                fields.push(Field { name: field_name, lo, width, signed });
                pos = lo + width;
            }
        }
        let index: HashMap<String, usize> = fields.iter().enumerate().map(|(i, f)| (f.name.clone(), i)).collect();
        assert_eq!(index.len(), fields.len(), "{name}: duplicate field names");
        let defaults = ops.first().map_or_else(Fields::new, |op| fields_default(*op)).merge(defaults);
        Self { name, ops: ops.to_vec(), nwords, fields, index, defaults }
    }

    pub fn field(&self, name: &str) -> Option<&Field> { self.index.get(name).map(|i| &self.fields[*i]) }

    /// The instruction words. Unknown fields and out-of-range values are
    /// generator bugs and panic with the layout and field named.
    pub fn encode(&self, values: &Fields) -> Vec<u128> {
        let mut bits = vec![0u8; self.nwords * WORD_BYTES];
        for (name, value) in self.defaults.iter().chain(values.iter()) {
            let field = self.field(name).unwrap_or_else(|| panic!("{}: unknown field {name}", self.name));
            let fits = if field.signed {
                field.width >= 64 || (-(1i64 << (field.width - 1))..(1i64 << (field.width - 1))).contains(&value)
            } else {
                value >= 0 && (field.width >= 63 || value < (1i64 << field.width))
            };
            assert!(fits, "{}.{name}={value} does not fit {} bits", self.name, field.width);
            let raw = value as u64;
            for i in 0..field.width.min(64) {
                let bit = field.lo + i;
                if (raw >> i) & 1 == 1 {
                    bits[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
        let words = bytes_to_words(&bits);
        assert!(self.ops.is_empty() || self.ops.contains(&opcode(words[0])), "{}: encoded opcode {:#x}", self.name, opcode(words[0]));
        words
    }

    /// Every field's value, signed fields sign-extended; reserved fields wider than
    /// 64 bits report their low 64 bits.
    pub fn decode(&self, words: &[u128]) -> Fields {
        assert_eq!(words.len(), self.nwords, "{}: expected {} words", self.name, self.nwords);
        let bits = words_to_bytes(words);
        let mut out = Fields::new();
        for field in &self.fields {
            let mut raw = 0u64;
            for i in 0..field.width.min(64) {
                let bit = field.lo + i;
                raw |= u64::from((bits[bit / 8] >> (bit % 8)) & 1) << i;
            }
            let value = if field.signed && field.width < 64 && raw >> (field.width - 1) & 1 == 1 { raw as i64 - (1i64 << field.width) } else { raw as i64 };
            out.put(&field.name, value);
        }
        out
    }
}

fn fields_default(op: u8) -> Fields { Fields::new().set("opcode", i64::from(op)) }

pub fn words_to_bytes(words: &[u128]) -> Vec<u8> { words.iter().flat_map(|w| w.to_le_bytes()).collect() }

pub fn bytes_to_words(bytes: &[u8]) -> Vec<u128> {
    bytes.as_chunks::<WORD_BYTES>().0.iter().map(|chunk| u128::from_le_bytes(*chunk)).collect()
}

/// `(word index, words)` of every instruction in a program.
pub fn split(program: &[u8]) -> Vec<(usize, Vec<u128>)> {
    let words = bytes_to_words(program);
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let n = length(opcode(words[i]));
        out.push((i, words[i..(i + n).min(words.len())].to_vec()));
        i += n;
    }
    out
}

#[cfg(test)]
mod tests;
