//! The program model: what a DarwiNN executable carries, as plain data. The
//! fields follow libedgetpu's `executable.fbs`; our own generators fill the same
//! structure, so the runtime does not care where a program came from.

use super::{Error, Result};

/// Which base address a relocation or a DMA hint refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Description {
    OutputActivation,
    InputActivation,
    Parameter,
    Scratch,
}

/// A 32-bit immediate in the bitstream that holds one half of a 64-bit base address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldOffset {
    pub desc: Description,
    pub batch: usize,
    pub name: Option<String>,
    /// The upper 32 bits of the address; otherwise the lower.
    pub upper: bool,
    pub offset_bit: usize,
}

/// One instruction chunk and the relocations inside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitstream {
    pub data: Vec<u8>,
    pub relocs: Vec<FieldOffset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Host to device.
    Infeed,
    /// Device to host.
    Outfeed,
}

/// What the program's descriptors will ask the host interface to move, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    Instruction {
        chunk: usize,
    },
    Dma {
        direction: Direction,
        desc: Description,
        name: Option<String>,
        offset: usize,
        size: usize,
    },
    /// Scalar-core interrupt 0..=3; 0 is the completion interrupt.
    Interrupt {
        id: u8,
    },
    Fence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutableType {
    StandAlone,
    /// Loads parameters into the tiles' wide memory; paired with an `ExecutionOnly`.
    ParameterCaching,
    /// Assumes its parameters are already on the chip.
    ExecutionOnly,
}

/// An input or output of the program, as the host sees it: `size_bytes` includes
/// the padding the DMA engines expect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub name: String,
    pub size_bytes: usize,
}

/// Instruction word size: every bitstream is a whole number of these.
pub const WORD_BYTES: usize = 16;
/// Scalar-core interrupts the chip has.
pub const INTERRUPTS: u8 = 4;

#[derive(Clone, Debug)]
pub struct Executable {
    pub name: String,
    pub kind: ExecutableType,
    pub batch_size: usize,
    pub scratch_size: usize,
    pub bitstreams: Vec<Bitstream>,
    /// The parameter blob a caching program uploads, when the program carries it;
    /// generated programs leave it empty and the runtime maps the weights separately.
    pub parameters: Vec<u8>,
    pub hints: Vec<Hint>,
    pub inputs: Vec<Layer>,
    pub outputs: Vec<Layer>,
    /// Caching programs with the same token share the chip's parameter memory.
    pub caching_token: u64,
}

impl Executable {
    pub fn input(&self, name: &str) -> Option<&Layer> {
        self.inputs.iter().find(|layer| layer.name == name)
    }

    pub fn output(&self, name: &str) -> Option<&Layer> {
        self.outputs.iter().find(|layer| layer.name == name)
    }

    /// Checks what the runtime relies on: whole instruction words, hints that point
    /// at existing chunks and inside their layers, relocations inside the bitstream
    /// that name an existing layer.
    pub fn validate(&self) -> Result<()> {
        let fail = |what: String| Error::Executable(format!("{}: {what}", self.name));
        if self.batch_size == 0 {
            return Err(fail("batch size is zero".into()));
        }
        if self.bitstreams.is_empty() {
            return Err(fail("no instruction bitstream".into()));
        }
        for (i, chunk) in self.bitstreams.iter().enumerate() {
            if chunk.data.is_empty() || chunk.data.len() % WORD_BYTES != 0 {
                return Err(fail(format!(
                    "bitstream {i} has {} bytes, not a whole number of {WORD_BYTES}-byte words",
                    chunk.data.len()
                )));
            }
            for reloc in &chunk.relocs {
                let end = reloc.offset_bit.checked_add(32).ok_or_else(|| {
                    fail(format!("bitstream {i}: relocation bit offset overflows"))
                })?;
                if end > chunk.data.len() * 8 {
                    return Err(fail(format!(
                        "bitstream {i}: relocation at bit {} is outside {} bytes",
                        reloc.offset_bit,
                        chunk.data.len()
                    )));
                }
                if reloc.batch >= self.batch_size {
                    return Err(fail(format!(
                        "bitstream {i}: relocation batch {} of {}",
                        reloc.batch, self.batch_size
                    )));
                }
                self.check_layer(reloc.desc, reloc.name.as_deref())
                    .map_err(|what| fail(format!("bitstream {i}: relocation {what}")))?;
            }
        }
        for (i, hint) in self.hints.iter().enumerate() {
            match hint {
                Hint::Instruction { chunk } if *chunk >= self.bitstreams.len() => {
                    return Err(fail(format!(
                        "hint {i} names instruction chunk {chunk} of {}",
                        self.bitstreams.len()
                    )));
                }
                Hint::Dma {
                    desc,
                    name,
                    offset,
                    size,
                    ..
                } => {
                    let layer_size = self
                        .check_layer(*desc, name.as_deref())
                        .map_err(|what| fail(format!("hint {i}: {what}")))?;
                    let end = offset
                        .checked_add(*size)
                        .ok_or_else(|| fail(format!("hint {i}: DMA extent overflows")))?;
                    if *size == 0 || end > layer_size {
                        return Err(fail(format!(
                            "hint {i}: DMA of {size} bytes at {offset} is outside the {layer_size}-byte {desc:?}"
                        )));
                    }
                }
                Hint::Interrupt { id } if *id >= INTERRUPTS => {
                    return Err(fail(format!("hint {i}: interrupt {id} of {INTERRUPTS}")));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The byte size of the buffer a relocation or hint refers to.
    fn check_layer(
        &self,
        desc: Description,
        name: Option<&str>,
    ) -> std::result::Result<usize, String> {
        match (desc, name) {
            (Description::InputActivation, Some(name)) => self
                .input(name)
                .map(|layer| layer.size_bytes)
                .ok_or_else(|| format!("names unknown input {name:?}")),
            (Description::OutputActivation, Some(name)) => self
                .output(name)
                .map(|layer| layer.size_bytes)
                .ok_or_else(|| format!("names unknown output {name:?}")),
            // A program that does not carry its parameters gets them from the runtime, which checks the extent then.
            (Description::Parameter, None) => Ok(if self.parameters.is_empty() {
                usize::MAX
            } else {
                self.parameters.len()
            }),
            (Description::Scratch, None) => {
                if self.scratch_size == 0 {
                    Err("refers to scratch memory this program does not declare".into())
                } else {
                    Ok(self.scratch_size)
                }
            }
            (desc, Some(name)) => Err(format!("{desc:?} must not carry a name ({name:?})")),
            (desc, None) => Err(format!("{desc:?} needs a layer name")),
        }
    }
}

#[cfg(test)]
mod tests;
