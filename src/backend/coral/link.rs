//! Links device addresses into a program. Every relocation names a 32-bit immediate
//! (the lower or upper half of a 64-bit base address) at a bit offset in the
//! bitstream: the MOVI the scalar core runs before it builds a DMA descriptor. Over
//! USB every base is zero; over PCIe the chip reads host memory at these addresses.

use super::executable::{Bitstream, Description, FieldOffset};
use super::{Error, Result};
use std::collections::HashMap;

/// Device addresses of the buffers a program refers to, one per batch for layers.
#[derive(Debug, Default)]
pub struct Addresses {
    pub parameter: Option<u64>,
    pub scratch: Option<u64>,
    pub inputs: HashMap<String, Vec<u64>>,
    pub outputs: HashMap<String, Vec<u64>>,
}

impl Addresses {
    fn resolve(&self, reloc: &FieldOffset) -> Result<u64> {
        let missing = |what: &str| {
            Error::Link(format!(
                "relocation at bit {} needs {what}",
                reloc.offset_bit
            ))
        };
        let batched = |table: &HashMap<String, Vec<u64>>, kind: &str| {
            let name = reloc
                .name
                .as_deref()
                .ok_or_else(|| missing(&format!("a named {kind}")))?;
            let addresses = table
                .get(name)
                .ok_or_else(|| missing(&format!("{kind} {name:?}")))?;
            addresses
                .get(reloc.batch)
                .copied()
                .ok_or_else(|| missing(&format!("batch {} of {kind} {name:?}", reloc.batch)))
        };
        match reloc.desc {
            Description::InputActivation => batched(&self.inputs, "input"),
            Description::OutputActivation => batched(&self.outputs, "output"),
            Description::Parameter => self
                .parameter
                .ok_or_else(|| missing("the parameter address")),
            Description::Scratch => self.scratch.ok_or_else(|| missing("the scratch address")),
        }
    }
}

/// The bits of a 32-bit field at `offset_bit`, if it lies inside `len` bytes.
fn field_bits(len: usize, offset_bit: usize) -> Result<std::ops::Range<usize>> {
    let end = offset_bit
        .checked_add(32)
        .filter(|end| *end <= len * 8)
        .ok_or_else(|| {
            Error::Link(format!(
                "32-bit field at bit {offset_bit} is outside {len} bytes"
            ))
        })?;
    Ok(offset_bit..end)
}

/// Writes `value` at bit `offset_bit`, least significant bit first, straddling bytes
/// as needed; the other bits of the touched bytes are kept.
pub fn copy_u32(buf: &mut [u8], offset_bit: usize, value: u32) -> Result<()> {
    for (i, bit) in field_bits(buf.len(), offset_bit)?.enumerate() {
        let (byte, shift) = (bit / 8, bit % 8);
        let mask = 1u8 << shift;
        let set = ((value >> i) & 1) as u8;
        buf[byte] = (buf[byte] & !mask) | (set << shift);
    }
    Ok(())
}

/// Reads the field `copy_u32` writes, for checks and tests.
pub fn read_u32(buf: &[u8], offset_bit: usize) -> Result<u32> {
    Ok(field_bits(buf.len(), offset_bit)?
        .enumerate()
        .fold(0u32, |acc, (i, bit)| {
            acc | (u32::from((buf[bit / 8] >> (bit % 8)) & 1) << i)
        }))
}

/// The bitstream with every relocation resolved. Every relocation must resolve:
/// a program that refers to a buffer the host did not map would read or write at
/// device address zero.
pub fn link(bitstream: &Bitstream, addresses: &Addresses) -> Result<Vec<u8>> {
    let mut data = bitstream.data.clone();
    for reloc in &bitstream.relocs {
        let address = addresses.resolve(reloc)?;
        let half = if reloc.upper {
            (address >> 32) as u32
        } else {
            address as u32
        };
        copy_u32(&mut data, reloc.offset_bit, half)?;
    }
    Ok(data)
}

#[cfg(test)]
mod tests;
