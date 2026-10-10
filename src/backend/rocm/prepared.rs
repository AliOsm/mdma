use super::{Error, GgmlDType, HipBuf, QWeight, Result, dtype_code};

pub(super) fn dtype(w: &QWeight<HipBuf>) -> Result<i32> {
    if w.buf.packed_q4 {
        if w.dtype != GgmlDType::Q4K {
            return Err(Error::ShapeMismatch("packed Q4_K buffer used with another dtype".into()));
        }
        let elements = crate::memory::product(&w.shape)?;
        if !elements.is_multiple_of(256)
            || crate::memory::product(&[elements / 256, crate::cache::layout::Q4K_BLOCK_BYTES])? != w.buf.len
        {
            return Err(Error::ShapeMismatch("packed Q4_K buffer size does not match weight shape".into()));
        }
        Ok(crate::cache::layout::Q4K_DTYPE_TAG)
    } else {
        dtype_code::decoder_code(w.dtype)
    }
}

#[cfg(test)]
mod tests;
