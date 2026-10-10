use super::{Error, GgmlDType, HipBackend, HipBuf, Q8Scratch, Q8_1_BLOCK, QK_K, QWeight, Result, hip_err};
use crate::memory::{product, sum};
use rocm_rs::hip::Dim3;
use std::ffi::c_void;

impl HipBackend {
    pub(super) fn launch_gemv_i8(&self, w: &QWeight<HipBuf>, x: &HipBuf, y: &mut HipBuf) -> Result<()> {
        self.launch_decode_i8(w, None, x, y, 8)
    }

    pub(super) fn launch_gated_gemv_i8(&self, gate: &QWeight<HipBuf>, up: &QWeight<HipBuf>, x: &HipBuf, y: &mut HipBuf) -> Result<()> {
        if gate.dtype != up.dtype || gate.shape != up.shape || gate.buf.packed_q4 != up.buf.packed_q4 {
            return Err(Error::ShapeMismatch("gated decode requires matching weight shapes and dtypes".into()));
        }
        self.launch_decode_i8(gate, Some(up), x, y, 8)
    }

    /// Three K-quant matvecs on one input in one launch (q, k, v).
    pub(super) fn launch_gemv_i8_x3(&self, w: [&QWeight<HipBuf>; 3], x: &HipBuf, y: [&mut HipBuf; 3]) -> Result<()> {
        let n_in = w[0].shape[1];
        if w.iter().any(|m| m.shape.len() != 2 || m.shape[1] != n_in) {
            return Err(Error::ShapeMismatch("grouped decode requires three rank-2 weights of one width".into()));
        }
        if w.iter().zip(&y).any(|(m, b)| b.len < m.shape[0] * size_of::<f32>()) || x.len < n_in * size_of::<f32>() {
            return Err(Error::ShapeMismatch("grouped decode: an output or the input is smaller than its weight".into()));
        }
        let mut q6 = [0i32; 3];
        for (flag, m) in q6.iter_mut().zip(&w) {
            super::prepared::dtype(m)?;
            *flag = match m.dtype {
                GgmlDType::Q4K if m.buf.packed_q4 => 2,
                GgmlDType::Q4K => 0,
                GgmlDType::Q6K => 1,
                _ => return Err(Error::NotImplemented("packed decode requires Q4_K or Q6_K")),
            };
        }
        let kernel = "qmatmul_gemv_i8_x3";
        let _guard = self.lock.lock().unwrap();
        let mut scratch = self.q8_scratch.lock().unwrap();
        let (mut xq_ptr, mut ys_ptr) = self.quantized_input(&mut scratch, x, n_in)?;
        scratch.cached = if y.iter().any(|b| x.as_device_ptr() == b.as_device_ptr()) { None } else { scratch.cached };
        let mut n_in_i = n_in as i32;
        let mut wp = [w[0].buf.as_device_ptr(), w[1].buf.as_device_ptr(), w[2].buf.as_device_ptr()];
        let mut np = [w[0].shape[0] as i32, w[1].shape[0] as i32, w[2].shape[0] as i32];
        let mut yp = [y[0].as_device_ptr(), y[1].as_device_ptr(), y[2].as_device_ptr()];
        let func = self.module.get_function(kernel).map_err(hip_err)?;
        let mut args = [
            (&mut wp[0] as *mut *mut c_void).cast(), (&mut np[0] as *mut i32).cast(), (&mut q6[0] as *mut i32).cast(), (&mut yp[0] as *mut *mut c_void).cast(),
            (&mut wp[1] as *mut *mut c_void).cast(), (&mut np[1] as *mut i32).cast(), (&mut q6[1] as *mut i32).cast(), (&mut yp[1] as *mut *mut c_void).cast(),
            (&mut wp[2] as *mut *mut c_void).cast(), (&mut np[2] as *mut i32).cast(), (&mut q6[2] as *mut i32).cast(), (&mut yp[2] as *mut *mut c_void).cast(),
            (&mut n_in_i as *mut i32).cast(), (&mut xq_ptr as *mut *mut c_void).cast(), (&mut ys_ptr as *mut *mut c_void).cast(),
        ];
        let rows: usize = w.iter().map(|m| m.shape[0]).sum();
        func.launch(Dim3::new_1d((rows as u32).div_ceil(8)), Dim3::new_1d(8 * 32), 0, self.stream.as_ref(), &mut args).map_err(hip_err)
    }

    /// The scratch's q8 and scale regions for one row of `n_in`, growing
    /// it (after a device sync) if needed. Caller holds `lock` and the
    /// scratch guard.
    pub(super) fn q8_row_ptrs(&self, scratch: &mut Q8Scratch, n_in: usize) -> Result<(*mut c_void, *mut c_void)> {
        if n_in == 0 || n_in % QK_K as usize != 0 {
            return Err(Error::ShapeMismatch("K-quant decode width must be a positive multiple of 256".into()));
        }
        let need = sum([n_in, product(&[n_in / Q8_1_BLOCK as usize, size_of::<[f32; 2]>()])?])?;
        scratch.reserve(need, &self.buffers)?;
        let xq_ptr = scratch.mem.as_ref().expect("scratch allocated above").as_ptr();
        // SAFETY: n_in is a multiple of 256; the aligned float2 metadata
        // occupies the remaining bytes of the scratch allocation.
        let ys_ptr = unsafe { xq_ptr.cast::<u8>().add(n_in).cast::<c_void>() };
        Ok((xq_ptr, ys_ptr))
    }

    /// Q8_1-quantize `x` (n_in floats) into the scratch unless it already
    /// holds that quantization (a norm kernel may have emitted it);
    /// returns the q8 and scale pointers. Caller holds `lock` and the
    /// scratch guard.
    fn quantized_input(&self, scratch: &mut Q8Scratch, x: &HipBuf, n_in: usize) -> Result<(*mut c_void, *mut c_void)> {
        let (xq_ptr, ys_ptr) = self.q8_row_ptrs(scratch, n_in)?;
        let key = (x.as_device_ptr() as usize, 1, n_in as i32);
        if scratch.cached != Some(key) {
            self.launch_quantize_q8_1_unlocked(x.as_device_ptr(), n_in as i32, 1, xq_ptr, ys_ptr)?;
            scratch.cached = Some(key);
        }
        Ok((xq_ptr, ys_ptr))
    }

    fn launch_decode_i8(&self, w: &QWeight<HipBuf>, up: Option<&QWeight<HipBuf>>, x: &HipBuf, y: &mut HipBuf, rows_per_block: u32) -> Result<()> {
        let [n_out, n_in] = w.shape[..] else {
            return Err(Error::ShapeMismatch("decode matvec requires a rank-2 weight".into()));
        };
        super::prepared::dtype(w)?;
        if let Some(up) = up { super::prepared::dtype(up)?; }
        let kernel = match (w.dtype, up.is_some()) {
            (GgmlDType::Q4K, false) if w.buf.packed_q4 => "qmatmul_gemv_i8_q4k_prepared",
            (GgmlDType::Q4K, true) if w.buf.packed_q4 => "qmatmul_gemv_i8_q4k_prepared_gated",
            (GgmlDType::Q4K, false) => "qmatmul_gemv_i8_q4k",
            (GgmlDType::Q6K, false) => "qmatmul_gemv_i8_q6k",
            (GgmlDType::Q4K, true) => "qmatmul_gemv_i8_q4k_gated",
            (GgmlDType::Q6K, true) => "qmatmul_gemv_i8_q6k_gated",
            _ => return Err(Error::NotImplemented("packed decode requires Q4_K or Q6_K")),
        };
        let _guard = self.lock.lock().unwrap();
        let mut scratch = self.q8_scratch.lock().unwrap();
        let (mut xq_ptr, mut ys_ptr) = self.quantized_input(&mut scratch, x, n_in)?;
        // Valid until any op writes a buffer -- or this matvec writes into
        // the cached input itself.
        if x.as_device_ptr() == y.as_device_ptr() {
            scratch.cached = None;
        }
        let mut w_ptr = w.buf.as_device_ptr();
        let mut up_ptr = up.map_or(std::ptr::null_mut(), |w| w.buf.as_device_ptr());
        let mut n_in = n_in as i32;
        let mut n_out = n_out as i32;
        let mut y_ptr = y.as_device_ptr();
        let func = self.module.get_function(kernel).map_err(hip_err)?;
        let mut args = [
            (&mut w_ptr as *mut *mut c_void).cast(),
            (&mut n_out as *mut i32).cast(),
            (&mut n_in as *mut i32).cast(),
            (&mut xq_ptr as *mut *mut c_void).cast(),
            (&mut ys_ptr as *mut *mut c_void).cast(),
            (&mut y_ptr as *mut *mut c_void).cast(),
            (&mut up_ptr as *mut *mut c_void).cast(),
        ];
        let n_args = if up.is_some() { 7 } else { 6 };
        func.launch(Dim3::new_1d((n_out as u32).div_ceil(rows_per_block)), Dim3::new_1d(rows_per_block * 32), 0, self.stream.as_ref(), &mut args[..n_args])
            .map_err(hip_err)
    }
}

#[cfg(test)]
mod tests;
