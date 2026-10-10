//! ROCm Q4_K v3: original halves/nibbles, aligned low-nibble and high-bit planes.
use crate::gguf::{GgmlDType, TensorDesc};
use serde::{Deserialize, Serialize};
use std::io;

pub use super::layout_abi::{Q4K_BLOCK_BYTES, Q4K_QUANTS_OFFSET, Q4K_DTYPE_TAG};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeightLayout {
    #[default]
    Gguf,
    RocmQ4kV3,
}

impl WeightLayout {
    pub(crate) fn for_tensor(desc: &TensorDesc) -> Self {
        if desc.dtype == GgmlDType::Q4K && desc.shape.len() == 2 {
            Self::RocmQ4kV3
        } else {
            Self::Gguf
        }
    }

    pub(crate) fn bytes(self, desc: &TensorDesc) -> io::Result<usize> {
        match self {
            Self::Gguf => Ok(desc.n_bytes),
            Self::RocmQ4kV3 if desc.dtype == GgmlDType::Q4K && desc.n_bytes.is_multiple_of(144) => {
                (desc.n_bytes / 144).checked_mul(Q4K_BLOCK_BYTES)
                    .ok_or_else(|| io::Error::other("packed weight size overflow"))
            }
            Self::RocmQ4kV3 => Err(io::Error::other("packed layout does not match tensor dtype")),
        }
    }
}

pub(crate) fn pack_q4k(raw: &[u8]) -> io::Result<Vec<u8>> {
    if !raw.len().is_multiple_of(144) {
        return Err(io::Error::other("partial Q4_K block"));
    }
    let len = (raw.len() / 144).checked_mul(Q4K_BLOCK_BYTES)
        .ok_or_else(|| io::Error::other("packed weight size overflow"))?;
    let mut packed = vec![0; len];
    for (src, dst) in raw.as_chunks::<144>().0.iter().zip(packed.as_chunks_mut::<Q4K_BLOCK_BYTES>().0) {
        dst[..4].copy_from_slice(&src[..4]);
        let scales = &src[4..16];
        let mut planes = [0u32; 3];
        for j in 0..8 {
            let (scale, min) = if j < 4 {
                (scales[j] & 63, scales[j + 4] & 63)
            } else {
                ((scales[j + 4] & 15) | ((scales[j - 4] >> 6) << 4),
                 (scales[j + 4] >> 4) | ((scales[j] >> 6) << 4))
            };
            let shift = (j / 2) * 8;
            let parity = j % 2;
            planes[0] |= u32::from(scale & 15) << (shift + parity * 4);
            planes[1] |= u32::from(min & 15) << (shift + parity * 4);
            planes[2] |= (u32::from(scale >> 4) << (shift + parity * 2))
                | (u32::from(min >> 4) << (shift + 4 + parity * 2));
        }
        for (plane, bits) in planes.into_iter().enumerate() {
            dst[4 + plane * 4..8 + plane * 4].copy_from_slice(&bits.to_le_bytes());
        }
        dst[Q4K_QUANTS_OFFSET..].copy_from_slice(&src[16..]);
    }
    Ok(packed)
}

#[cfg(test)]
mod tests;
