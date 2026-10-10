//! Test-only plumbing shared across backend suites.
//!
//! Callers select the device, rounding mode and tolerance.

pub(crate) mod encoder;
mod decoder_ops;
#[cfg(any(feature = "rocm", feature = "vulkan", feature = "cuda"))]
mod decoder_weights;
#[cfg(any(feature = "rocm", feature = "vulkan", feature = "cuda"))]
mod decoder_attention;
pub(crate) mod generation;
pub(crate) mod graph;
pub(crate) mod parity;

use super::cpu::{CpuBackend, CpuBuf};
use super::{AttnSpec, Backend, KvView, Operation, Pool, QWeight, Result, RopeSpec};
use crate::gguf::{GgmlDType, GgufModel};

/// Rejected requests and zero-offload execution must never touch a device.
pub(crate) struct NoUpload;
impl Backend for NoUpload {
    type Buf = CpuBuf;
    fn supports(&self, op: Operation) -> bool { CpuBackend.supports(op) }
    fn alloc(&self, _: usize, _: Pool) -> Result<CpuBuf> { panic!("allocated before rejection") }
    fn upload(&self, _: &mut CpuBuf, _: usize, _: &[u8]) -> Result<()> { panic!("uploaded before rejection") }
    fn download(&self, _: &CpuBuf, _: usize, _: &mut [u8]) -> Result<()> { unreachable!() }
    fn copy(&self, _: &CpuBuf, _: usize, _: &mut CpuBuf, _: usize, _: usize) -> Result<()> { unreachable!() }
    fn qmatmul(&self, _: &QWeight<CpuBuf>, _: &CpuBuf, _: &mut CpuBuf) -> Result<()> { unreachable!() }
    fn embed(&self, _: &QWeight<CpuBuf>, _: &[u32], _: &mut CpuBuf) -> Result<()> { unreachable!() }
    fn rmsnorm(&self, _: &CpuBuf, _: &CpuBuf, _: &mut CpuBuf, _: f32) -> Result<()> { unreachable!() }
    fn add(&self, _: &CpuBuf, _: &CpuBuf, _: &mut CpuBuf) -> Result<()> { unreachable!() }
    fn rope(&self, _: &mut CpuBuf, _: &mut CpuBuf, _: u32, _: usize, _: &RopeSpec) -> Result<()> { unreachable!() }
    fn attn(&self, _: &CpuBuf, _: &KvView<CpuBuf>, _: &AttnSpec, _: &mut CpuBuf) -> Result<()> { unreachable!() }
    fn silu_mul(&self, _: &CpuBuf, _: &CpuBuf, _: &mut CpuBuf) -> Result<()> { unreachable!() }
}

/// Path to the optional Bespoke-MiniCheck-7B-Q4_K_M model.
pub(crate) fn fixture() -> &'static std::path::Path {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::var_os("MDMA_TEST_GGUF")
            .filter(|path| !path.is_empty())
            .expect("set MDMA_TEST_GGUF to Bespoke-MiniCheck-7B-Q4_K_M.gguf");
        let path = std::path::PathBuf::from(path);
        assert!(path.is_file(), "GGUF fixture not found: {}", path.display());
        path
    })
}

pub(crate) fn bytes<T: bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

pub(crate) fn f32s(b: &[u8]) -> Vec<f32> {
    bytemuck::cast_slice(b).to_vec()
}

/// Strict absolute tolerance; nonfinite differences fail the comparison.
pub(crate) fn close(got: &[f32], want: &[f32], tol: f32) {
    assert_eq!(got.len(), want.len());
    for (i, (a, b)) in got.iter().zip(want).enumerate() {
        assert!((a - b).abs() < tol, "index {i}: got {a}, want {b}");
    }
}

/// For tests whose KV cache stores f16, round the CPU inputs explicitly.
pub(crate) fn f16_round(v: &[f32]) -> Vec<f32> {
    v.iter().map(|&x| half::f16::from_f32(x).to_f32()).collect()
}

/// Upload little-endian halves to the KV pool. Callers must explicitly
/// round their CPU reference and use this only for f16-cache scenarios.
pub(crate) fn upload_f16_kv<B: Backend>(backend: &B, values: &[f32]) -> B::Buf {
    let bytes: Vec<u8> = values.iter().flat_map(|&x| half::f16::from_f32(x).to_le_bytes()).collect();
    let mut buf = backend.alloc(bytes.len(), Pool::Kv).unwrap();
    backend.upload(&mut buf, 0, &bytes).unwrap();
    buf
}

#[allow(dead_code)]
pub(crate) fn find_tensor(fixture: &std::path::Path, dtype: GgmlDType) -> (String, Vec<usize>) {
    use candle_core::quantized::gguf_file::Content;
    use std::fs::File;

    let mut file = File::open(fixture).expect("open fixture");
    let content = Content::read(&mut file).expect("read content");
    let (name, info) = content
        .tensor_infos
        .iter()
        .find(|(_, info)| info.ggml_dtype == dtype)
        .unwrap_or_else(|| panic!("fixture has no {dtype:?} tensor"));
    (name.clone(), info.shape.dims().to_vec())
}

pub(crate) fn cpu_qmatmul(dtype: GgmlDType, w: &[u8], shape: &[usize], x: &[f32]) -> Vec<f32> {
    let cpu = CpuBackend;
    let w_cpu = QWeight { dtype, shape: shape.to_vec(), buf: CpuBuf(w.to_vec()) };
    let x_cpu = CpuBuf(bytes(x));
    let n_tokens = x.len() / shape[1];
    let mut y_cpu = cpu.alloc(n_tokens * shape[0] * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    f32s(&y_cpu.0)
}

/// Execute a matmul on the configured backend and read back its output.
pub(crate) fn qmatmul<B: Backend>(gpu: &B, dtype: GgmlDType, w: &[u8], shape: &[usize], x: &[f32]) -> Vec<f32> {
    let n_tokens = x.len() / shape[1];
    let mut w_buf = gpu.alloc(w.len(), Pool::Weights).unwrap();
    gpu.upload(&mut w_buf, 0, w).unwrap();
    let w_gpu = QWeight { dtype, shape: shape.to_vec(), buf: w_buf };
    let mut x_buf = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut x_buf, 0, &bytes(x)).unwrap();
    let mut y_buf = gpu.alloc(n_tokens * shape[0] * 4, Pool::Scratch).unwrap();
    gpu.qmatmul(&w_gpu, &x_buf, &mut y_buf).unwrap();
    let mut out = vec![0u8; n_tokens * shape[0] * 4];
    gpu.download(&y_buf, 0, &mut out).unwrap();
    f32s(&out)
}

/// A backend against the CPU oracle on the same weights and activations.
pub(crate) fn qmatmul_matches_cpu<B: Backend>(gpu: &B, dtype: GgmlDType, w: &[u8], shape: &[usize], x: &[f32], tol: f32) {
    close(&qmatmul(gpu, dtype, w, shape, x), &cpu_qmatmul(dtype, w, shape, x), tol);
}

/// The first `max_rows` rows of the fixture's first `dtype` tensor with
/// their `[rows, n_in]` shape, limiting device memory use.
#[allow(dead_code)]
pub(crate) fn real_tensor_rows(dtype: GgmlDType, max_rows: usize) -> (Vec<u8>, Vec<usize>) {
    let (name, shape) = find_tensor(fixture(), dtype);
    let &[n_out, n_in] = shape.as_slice() else { panic!("rank {shape:?}") };
    let n_out = n_out.min(max_rows);
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read tensor");
    let row_bytes = raw.len() / shape[0];
    (raw[..n_out * row_bytes].to_vec(), vec![n_out, n_in])
}

/// One token of asymmetric activations in (-1, 1).
#[allow(dead_code)]
pub(crate) fn activation_row(n_in: usize) -> Vec<f32> {
    (0..n_in).map(|i| ((i * 17 % 255) as f32 - 127.0) / 128.0).collect()
}

/// 32 real rows of the fixture's `dtype` tensor, one token, against the CPU.
#[allow(dead_code)]
pub(crate) fn real_tensor_qmatmul_matches_cpu<B: Backend>(gpu: &B, dtype: GgmlDType, tol: f32) {
    let (raw, shape) = real_tensor_rows(dtype, 32);
    qmatmul_matches_cpu(gpu, dtype, &raw, &shape, &activation_row(shape[1]), tol);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_preserve_values_and_f16_rounding() {
        let values = [1.0f32, -2.5, 0.0, -0.0, 0.3333];
        assert_eq!(
            f32s(&bytes(&values)).iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            values.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        // Adjacent halves around one, plus a negative value and signed zero.
        assert_eq!(f16_round(&[1.0004, 1.0008, -2.0008]), [1.0, 1.0009766, -2.0]);
        assert_eq!(f16_round(&[-0.0])[0].to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn f16_kv_upload_preserves_layout_rounding_and_empty_input() {
        // CPU is only a byte-storage oracle here, not an f16 attention backend.
        let buf = upload_f16_kv(&CpuBackend, &[1.0, -2.0, -0.0, 1.0008]);
        assert_eq!(buf.0, [0x00, 0x3c, 0x00, 0xc0, 0x00, 0x80, 0x01, 0x3c]);
        assert!(upload_f16_kv(&CpuBackend, &[]).0.is_empty());
    }

    #[test]
    fn close_keeps_strict_tolerance_and_rejects_nonfinite_values() {
        close(&[1.0, -2.0], &[1.125, -2.125], 0.25);
        close(&[], &[], 0.0);
        for (got, want, tol) in [
            (vec![1.0], vec![1.25], 0.25),
            (vec![1.0], vec![1.0], 0.0),
            (vec![1.0], vec![], 1.0),
            (vec![f32::NAN], vec![0.0], 1.0),
            (vec![0.0], vec![f32::NAN], 1.0),
            (vec![f32::INFINITY], vec![f32::INFINITY], 1.0),
        ] {
            assert!(std::panic::catch_unwind(|| close(&got, &want, tol)).is_err());
        }
    }

    #[test]
    fn matmul_helpers_match_hand_computed_batched_results() {
        let w = bytes(&[1.0f32, -2.0, 3.0, 4.0, 5.0, -6.0]);
        let x = [2.0, -1.0, 0.5, -1.0, 2.0, 1.0];
        let expected = [5.5, 0.0, -2.0, 0.0];
        for tokens in [1, 2] {
            assert_eq!(cpu_qmatmul(GgmlDType::F32, &w, &[2, 3], &x[..3 * tokens]), expected[..2 * tokens]);
            assert_eq!(qmatmul(&CpuBackend, GgmlDType::F32, &w, &[2, 3], &x[..3 * tokens]), expected[..2 * tokens]);
        }
    }
}
