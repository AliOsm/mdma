//! HIP parity against the CPU backend, using asymmetric data to detect
//! transposed tiles and swapped indices.
use super::HipBackend;
use crate::backend::cpu::CpuBackend;
use crate::backend::test_support::generation::{benchmark_prefix, greedy_continuation};
use crate::backend::test_support::{
    bytes as pod_bytes, close as assert_close, f16_round, f32s as as_f32_vec, find_tensor,
    upload_f16_kv,
};
use crate::backend::{AttnSpec, Backend, KvView, Pool, QWeight, RopeSpec};
use crate::gguf::{GgmlDType, GgufModel};
use crate::model::Model;
use crate::sampler::greedy;
use crate::tokenizer::Tokenizer;
use std::sync::Mutex;

use crate::backend::test_support::fixture;

/// Serialize full-model tests to keep concurrent loads within the VRAM budget.
pub(crate) static GPU_MODEL_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn hip_oom_is_classified_by_driver_code_not_message_text() {
    use rocm_rs::hip::{error::Error, ffi};
    assert!(super::hip_err(Error::new(ffi::hipError_t_hipErrorOutOfMemory)).is_memory_exhausted());
    assert!(
        !super::hip_err(Error::new(ffi::hipError_t_hipErrorInvalidValue)).is_memory_exhausted()
    );
}

#[test]
fn only_rdna4_build_targets_enable_wmma() {
    for arch in ["gfx1200", "gfx1201", "gfx1201:xnack-"] {
        assert!(super::has_rdna4_wmma(arch), "{arch}");
    }
    for arch in ["gfx1030", "gfx1100", "gfx120", "gfx12010"] {
        assert!(!super::has_rdna4_wmma(arch), "{arch}");
    }
}

fn hip() -> HipBackend {
    HipBackend::new().expect("init HIP backend - does the GPU match MDMA_HIP_OFFLOAD_ARCH?")
}

fn kv_upload(hip: &HipBackend, values: &[f32]) -> super::HipBuf {
    if hip.kv_elem_bytes() == 2 {
        upload_f16_kv(hip, values)
    } else {
        let mut buf = hip.alloc(values.len() * 4, Pool::Kv).unwrap();
        hip.upload(&mut buf, 0, &pod_bytes(values)).unwrap();
        buf
    }
}

#[test]
#[ignore = "requires a matching HIP GPU; bounded generated K-quant matrices"]
fn portable_kquant_and_prepared_q4k_match_cpu() {
    use crate::backend::test_support::{cpu_qmatmul, qmatmul_matches_cpu};
    use crate::cache::{WeightLayout, layout::pack_q4k};

    let gpu = HipBackend::new_f32().unwrap();
    let shape = vec![5, 768];
    for (dtype, block_bytes, offsets) in [
        (GgmlDType::Q4K, 144, &[0, 2][..]),
        (GgmlDType::Q6K, 210, &[208][..]),
    ] {
        let raw: Vec<u8> = (0..15)
            .flat_map(|block| {
                let mut bytes: Vec<u8> = (0..block_bytes)
                    .map(|i| ((i * 73 + block * 31) % 256) as u8)
                    .collect();
                for &offset in offsets {
                    bytes[offset..offset + 2]
                        .copy_from_slice(&half::f16::from_f32(0.001).to_le_bytes());
                }
                bytes
            })
            .collect();
        for tokens in [1, 3, 17] {
            let x: Vec<f32> = (0..tokens * 768)
                .map(|i| ((i * 37 % 101) as f32 - 50.0) / 64.0)
                .collect();
            qmatmul_matches_cpu(&gpu, dtype, &raw, &shape, &x, 1e-3);
            if dtype == GgmlDType::Q4K {
                let buf = gpu
                    .upload_weight(&pack_q4k(&raw).unwrap(), WeightLayout::RocmQ4kV3)
                    .unwrap();
                let weight = QWeight {
                    dtype,
                    shape: shape.clone(),
                    buf,
                };
                let mut input = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
                gpu.upload(&mut input, 0, &pod_bytes(&x)).unwrap();
                let mut output = gpu.alloc(tokens * 5 * 4, Pool::Scratch).unwrap();
                gpu.qmatmul(&weight, &input, &mut output).unwrap();
                let mut got = vec![0; tokens * 5 * 4];
                gpu.download(&output, 0, &mut got).unwrap();
                assert_close(
                    &as_f32_vec(&got),
                    &cpu_qmatmul(dtype, &raw, &shape, &x),
                    1e-3,
                );
            }
        }
    }
}

#[test]
fn qmatmul_matches_cpu_on_asymmetric_f32_weights() {
    let cpu = CpuBackend;
    let hip = hip();
    let w_data: Vec<f32> = (0..12).map(|i| (i as f32) * 0.37 - 1.5).collect();
    let x_data: Vec<f32> = vec![1.0, -2.0, 0.5, 3.0];

    let w_cpu = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![3, 4],
        buf: crate::backend::cpu::CpuBuf(pod_bytes(&w_data)),
    };
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(12, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let mut w_buf = hip.alloc(w_data.len() * 4, Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let w_hip = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![3, 4],
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(x_data.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(12, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; 12];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-4);
}

/// Twenty rows leave a partial final block; 64 columns exercise two lane strides.
#[test]
fn qmatmul_gemv_handles_non_multiple_of_rows_per_block() {
    let n_out = 20usize;
    let n_in = 64usize;

    let w_data: Vec<f32> = (0..n_out * n_in)
        .map(|i| ((i % 19) as f32 - 9.0) * 0.17)
        .collect();
    let x_data: Vec<f32> = (0..n_in).map(|i| ((i % 7) as f32 - 3.0) * 0.41).collect();

    let cpu = CpuBackend;
    let w_cpu = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![n_out, n_in],
        buf: crate::backend::cpu::CpuBuf(pod_bytes(&w_data)),
    };
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(n_out * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(w_data.len() * 4, Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let w_hip = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![n_out, n_in],
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(x_data.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(n_out * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; n_out * 4];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-3);
}

fn qmatmul_matches_cpu_on_real_tensor(dtype: GgmlDType, tol: f32) {
    let (name, shape) = find_tensor(fixture(), dtype);
    let &[n_out, n_in] = shape.as_slice() else {
        panic!("expected rank-2 tensor, got {shape:?}")
    };

    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

    // Isolate unpacking from activation rounding: each block's max is
    // 127/128, making these asymmetric inputs exactly Q8_1-representable.
    let x_data: Vec<f32> = (0..n_in)
        .map(|i| {
            if i % 32 == 0 {
                127.0 / 128.0
            } else {
                ((i * 17 % 255) as f32 - 127.0) / 128.0
            }
        })
        .collect();

    let cpu = CpuBackend;
    let w_cpu = QWeight {
        dtype,
        shape: shape.clone(),
        buf: crate::backend::cpu::CpuBuf(raw.clone()),
    };
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(n_out * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &raw).unwrap();
    let w_hip = QWeight {
        dtype,
        shape,
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(n_in * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(n_out * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; n_out * 4];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, tol);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_matches_cpu_on_real_q4k_tensor() {
    qmatmul_matches_cpu_on_real_tensor(GgmlDType::Q4K, 5e-2);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_matches_cpu_on_real_q6k_tensor() {
    qmatmul_matches_cpu_on_real_tensor(GgmlDType::Q6K, 5e-2);
}

#[test]
fn qmatmul_wmma_handles_non_tile_aligned_dimensions() {
    let n_out = 20usize;
    let n_in = 20usize;
    let n_tokens = 16usize;

    let w_data: Vec<f32> = (0..n_out * n_in)
        .map(|i| ((i % 17) as f32 - 8.0) * 0.23)
        .collect();
    let x_data: Vec<f32> = (0..n_tokens * n_in)
        .map(|i| ((i % 11) as f32 - 5.0) * 0.31)
        .collect();

    let cpu = CpuBackend;
    let w_cpu = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![n_out, n_in],
        buf: crate::backend::cpu::CpuBuf(pod_bytes(&w_data)),
    };
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(w_data.len() * 4, Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let w_hip = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![n_out, n_in],
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(x_data.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; n_tokens * n_out * 4];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    // WMMA rounds operands to f16.
    assert_close(&as_f32_vec(&y_bytes), &want, 1e-1);
}

/// Compare identical Q8_1 inputs for kernel error, and unquantized inputs
/// separately for activation-rounding error.
fn qmatmul_i8_matches_requantized_cpu_reference(dtype: GgmlDType, n_tokens: usize) {
    let (name, shape) = find_tensor(fixture(), dtype);
    let &[n_out, n_in] = shape.as_slice() else {
        panic!("expected rank-2 tensor, got {shape:?}")
    };
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

    let x_data: Vec<f32> = (0..n_tokens * n_in)
        .map(|e| {
            let t = e / n_in;
            let i = e % n_in;
            ((i % 13) as f32 - 6.0) * 0.1 + (t as f32) * 0.017
        })
        .collect();

    // Host requantization, same representation as quantize_q8_1_kernel.
    let mut x_hat = vec![0f32; x_data.len()];
    for t in 0..n_tokens {
        for b in 0..n_in / 32 {
            let off = t * n_in + b * 32;
            let blk = &x_data[off..off + 32];
            let amax = blk.iter().fold(0f32, |m, v| m.max(v.abs()));
            // d is an f16 in ggml's block_q8_1; the quantizer rounds it.
            let d = half::f16::from_f32(amax / 127.0).to_f32();
            let inv = if amax > 0.0 { 127.0 / amax } else { 0.0 };
            for i in 0..32 {
                x_hat[off + i] = d * (blk[i] * inv).round();
            }
        }
    }

    let cpu = CpuBackend;
    let w_cpu = QWeight {
        dtype,
        shape: shape.clone(),
        buf: crate::backend::cpu::CpuBuf(raw.clone()),
    };
    let mut y_ref = cpu.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(
        &w_cpu,
        &crate::backend::cpu::CpuBuf(pod_bytes(&x_hat)),
        &mut y_ref,
    )
    .unwrap();
    let want_exact = as_f32_vec(&y_ref.0);
    let mut y_f32 = cpu.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(
        &w_cpu,
        &crate::backend::cpu::CpuBuf(pod_bytes(&x_data)),
        &mut y_f32,
    )
    .unwrap();
    let want_f32 = as_f32_vec(&y_f32.0);

    let hip = hip();
    let mut w_buf = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &raw).unwrap();
    let w_hip = QWeight {
        dtype,
        shape,
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(x_data.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; n_tokens * n_out * 4];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();
    let got = as_f32_vec(&y_bytes);

    let (mut num, mut den, mut max_dev) = (0f64, 0f64, 0f32);
    for (g, w) in got.iter().zip(want_f32.iter()) {
        num += ((g - w) as f64).powi(2);
        den += (*w as f64).powi(2);
        max_dev = max_dev.max((g - w).abs());
    }
    let rel_rms = (num / den.max(1e-30)).sqrt();
    eprintln!(
        "{name} [{n_out}x{n_in}] T={n_tokens} int8 vs f32 CPU: rel RMS {rel_rms:.4}, max abs dev {max_dev:.4}"
    );
    assert!(
        rel_rms < 0.03,
        "int8 activation error vs f32 reference: rel RMS {rel_rms}"
    );

    assert_close(&got, &want_exact, 1e-3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t1() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 1);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t1() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 1);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t3() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t16() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 16);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t32() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 32);
}

/// T>64 dispatches the 128x128 / 256-thread tile: two token tiles at
/// T=200, the second partial.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t200_j128() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 200);
}

/// The tiles' partial-token edges: T=50 lands in the 64-token kernel
/// with two live tokens in its last 16-token sub-tile, T=129 in the
/// 128-token kernel with one live token in its second tile.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t50() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 50);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q4k_tensor_t129() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q4K, 129);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t50() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 50);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t129() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 129);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t3() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t16() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 16);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_matches_cpu_on_real_q6k_tensor_t32() {
    qmatmul_i8_matches_requantized_cpu_reference(GgmlDType::Q6K, 32);
}

/// A two-token F32 matmul with n_out = n_in/2 overwrites all n_in cached inputs.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn q8_cache_is_invalidated_by_the_f16_matmul_path() {
    let hip = hip();
    let (name, shape) = find_tensor(fixture(), GgmlDType::Q4K);
    let n_in = shape[1];
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");
    let mut wq = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut wq, 0, &raw).unwrap();
    let w = QWeight {
        dtype: GgmlDType::Q4K,
        shape: shape.clone(),
        buf: wq,
    };

    let mut xin = hip.alloc(n_in * 4, Pool::Scratch).unwrap();
    hip.upload(
        &mut xin,
        0,
        &pod_bytes(
            &(0..n_in)
                .map(|i| (i as f32) * 0.001 - 0.5)
                .collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    let mut wn = hip.alloc(n_in * 4, Pool::Weights).unwrap();
    hip.upload(&mut wn, 0, &pod_bytes(&vec![1.0f32; n_in]))
        .unwrap();
    // Everything uploaded before the norm: upload itself invalidates the
    // cache, so the hazard only exists if nothing touches the backend
    // between the norm and the matmul that overwrites its output.
    let mut zeros_buf = hip.alloc(n_in / 2 * n_in * 4, Pool::Weights).unwrap();
    hip.upload(&mut zeros_buf, 0, &vec![0u8; n_in / 2 * n_in * 4])
        .unwrap();
    let zeros_w = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![n_in / 2, n_in],
        buf: zeros_buf,
    };
    let mut two_in = hip.alloc(2 * n_in * 4, Pool::Scratch).unwrap();
    hip.upload(&mut two_in, 0, &pod_bytes(&vec![0.5f32; 2 * n_in]))
        .unwrap();
    let mut y = hip.alloc(n_in * 4, Pool::Scratch).unwrap();

    hip.rmsnorm(&wn, &xin, &mut y, 1e-5).unwrap(); // caches y's Q8_1
    hip.qmatmul(&zeros_w, &two_in, &mut y).unwrap(); // WMMA path: y := 0

    let mut out = hip.alloc(shape[0] * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w, &y, &mut out).unwrap(); // int8 matvec on y
    let mut got = vec![0u8; shape[0] * 4];
    hip.download(&out, 0, &mut got).unwrap();
    assert!(
        as_f32_vec(&got).iter().all(|v| *v == 0.0),
        "matvec consumed a stale Q8 cache"
    );
}

#[test]
fn undersized_outputs_are_rejected() {
    let hip = hip();
    let n = 256usize;
    let a = hip.alloc(n * 4, Pool::Scratch).unwrap();
    let b = hip.alloc(n * 4, Pool::Scratch).unwrap();
    let w = hip.alloc(n * 4, Pool::Weights).unwrap();
    let mut short = hip.alloc(n * 4 - 4, Pool::Scratch).unwrap();
    let mut full = hip.alloc(n * 4, Pool::Scratch).unwrap();
    assert!(hip.add(&a, &b, &mut short).is_err());
    assert!(hip.rmsnorm(&w, &a, &mut short, 1e-5).is_err());
    assert!(
        hip.add_rmsnorm(&w, &a, &b, &mut short, &mut full, 1e-5)
            .is_err()
    );
    assert!(
        hip.add_rmsnorm(&w, &a, &b, &mut full, &mut short, 1e-5)
            .is_err()
    );
    let spec = AttnSpec {
        head_dim: 4,
        n_q_heads: 3,
        n_kv_heads: 2,
    }; // 3 % 2 != 0
    let k = hip.alloc(64, Pool::Kv).unwrap();
    let v = hip.alloc(64, Pool::Kv).unwrap();
    let q = hip.alloc(3 * 4 * 4, Pool::Scratch).unwrap();
    assert!(
        hip.attn(
            &q,
            &KvView {
                k: &k,
                v: &v,
                len: 1
            },
            &spec,
            &mut full
        )
        .is_err()
    );
    let spec = AttnSpec {
        head_dim: 4,
        n_q_heads: 2,
        n_kv_heads: 1,
    };
    let mut tiny_o = hip.alloc(4, Pool::Scratch).unwrap();
    let q2 = hip.alloc(2 * 4 * 4, Pool::Scratch).unwrap();
    assert!(
        hip.attn(
            &q2,
            &KvView {
                k: &k,
                v: &v,
                len: 1
            },
            &spec,
            &mut tiny_o
        )
        .is_err()
    );
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn qmatmul_i8_cache_is_invalidated_when_input_is_rewritten() {
    let (name, shape) = find_tensor(fixture(), GgmlDType::Q4K);
    let &[n_out, n_in] = shape.as_slice() else {
        panic!("expected rank-2 tensor")
    };
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");
    let n_tokens = 3usize;
    let xa: Vec<f32> = (0..n_tokens * n_in)
        .map(|i| ((i % 11) as f32 - 5.0) * 0.1)
        .collect();
    let xb: Vec<f32> = (0..n_tokens * n_in)
        .map(|i| ((i % 7) as f32 - 3.0) * 0.2)
        .collect();

    let hip = hip();
    let mut w_buf = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &raw).unwrap();
    let w = QWeight {
        dtype: GgmlDType::Q4K,
        shape,
        buf: w_buf,
    };
    let mut x = hip.alloc(xa.len() * 4, Pool::Scratch).unwrap();
    let mut y = hip.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();

    hip.upload(&mut x, 0, &pod_bytes(&xa)).unwrap();
    hip.qmatmul(&w, &x, &mut y).unwrap(); // populates the cache for x
    hip.upload(&mut x, 0, &pod_bytes(&xb)).unwrap(); // must invalidate it
    hip.qmatmul(&w, &x, &mut y).unwrap();
    let mut got = vec![0u8; n_tokens * n_out * 4];
    hip.download(&y, 0, &mut got).unwrap();

    let mut x2 = hip.alloc(xb.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x2, 0, &pod_bytes(&xb)).unwrap();
    let mut y2 = hip.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w, &x2, &mut y2).unwrap();
    let mut want = vec![0u8; n_tokens * n_out * 4];
    hip.download(&y2, 0, &mut want).unwrap();

    assert_eq!(as_f32_vec(&got), as_f32_vec(&want));
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn embed_matches_cpu_on_real_q4k_table() {
    let name = "token_embd.weight".to_string();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let shape = gguf
        .tensor_desc(&name)
        .expect("has token_embd.weight")
        .shape
        .clone();
    let &[vocab, n_embd] = shape.as_slice() else {
        panic!("expected rank-2 table")
    };
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");
    let token_id = 12345u32;
    assert!(
        (token_id as usize) < vocab,
        "test token_id must be in range"
    );

    let cpu = CpuBackend;
    let table_cpu = QWeight {
        dtype: GgmlDType::Q4K,
        shape: shape.clone(),
        buf: crate::backend::cpu::CpuBuf(raw.clone()),
    };
    let mut out_cpu = cpu.alloc(n_embd * 4, Pool::Scratch).unwrap();
    cpu.embed(&table_cpu, &[token_id], &mut out_cpu).unwrap();
    let want = as_f32_vec(&out_cpu.0);

    let hip = hip();
    let mut table_buf = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut table_buf, 0, &raw).unwrap();
    let table_hip = QWeight {
        dtype: GgmlDType::Q4K,
        shape,
        buf: table_buf,
    };
    let mut out_buf = hip.alloc(n_embd * 4, Pool::Scratch).unwrap();
    hip.embed(&table_hip, &[token_id], &mut out_buf).unwrap();
    let mut out_bytes = vec![0u8; n_embd * 4];
    hip.download(&out_buf, 0, &mut out_bytes).unwrap();

    assert_close(&as_f32_vec(&out_bytes), &want, 5e-2);
}

#[test]
fn rmsnorm_matches_cpu_on_asymmetric_data() {
    let x_data: Vec<f32> = vec![3.0, -4.0, 2.5, 0.1, -7.0, 1.0, 6.0, -2.0];
    let w_data: Vec<f32> = vec![1.0, 0.5, 2.0, 1.5, 0.9, 1.1, 0.3, 2.2];

    let cpu = CpuBackend;
    let w_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&w_data));
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(32, Pool::Scratch).unwrap();
    cpu.rmsnorm(&w_cpu, &x_cpu, &mut y_cpu, 1e-5).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let mut x_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.rmsnorm(&w_buf, &x_buf, &mut y_buf, 1e-5).unwrap();
    let mut y_bytes = vec![0u8; 32];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-4);
}

#[test]
fn rmsnorm_wave_reduction_handles_tails_and_zero_rows() {
    let hip = hip();
    for dim in [1, 3, 7, 31, 32, 33, 63, 96, 127, 255, 256, 257, 4096, 4103] {
        for tokens in [1, 3, 17] {
            let w: Vec<f32> = (0..dim)
                .map(|i| ((i * 7 % 23) as f32 - 11.0) * 0.2)
                .collect();
            let x: Vec<f32> = (0..dim * tokens)
                .map(|i| match i / dim % 3 {
                    0 => ((i * 17 % 61) as f32 - 30.0) * 0.13,
                    1 => 0.0,
                    _ => ((i * 11 % 37) as f32 - 18.0) * 1e-8,
                })
                .collect();
            let mut w_buf = hip.alloc(w.len() * 4, Pool::Scratch).unwrap();
            let mut x_buf = hip.alloc(x.len() * 4, Pool::Scratch).unwrap();
            hip.upload(&mut w_buf, 0, &pod_bytes(&w)).unwrap();
            hip.upload(&mut x_buf, 0, &pod_bytes(&x)).unwrap();
            let mut y_buf = hip.alloc((x.len() + 8) * 4, Pool::Scratch).unwrap();
            hip.upload(&mut y_buf, 0, &pod_bytes(&vec![123456.25f32; x.len() + 8]))
                .unwrap();
            hip.rmsnorm(&w_buf, &x_buf, &mut y_buf, 1e-5).unwrap();
            let mut bytes = vec![0; (x.len() + 8) * 4];
            hip.download(&y_buf, 0, &mut bytes).unwrap();
            let got = as_f32_vec(&bytes);
            let mut want = CpuBackend.alloc(x.len() * 4, Pool::Scratch).unwrap();
            CpuBackend
                .rmsnorm(
                    &crate::backend::cpu::CpuBuf(pod_bytes(&w)),
                    &crate::backend::cpu::CpuBuf(pod_bytes(&x)),
                    &mut want,
                    1e-5,
                )
                .unwrap();
            assert_close(&got[..x.len()], &as_f32_vec(&want.0), 1e-4);
            assert!(
                got[x.len()..].iter().all(|&v| v == 123456.25),
                "dim={dim} T={tokens}: output overrun"
            );
        }
    }
}

#[test]
fn add_matches_cpu_on_asymmetric_data() {
    let a_data: Vec<f32> = vec![1.0, -2.0, 3.5, 0.0, 8.0];
    let b_data: Vec<f32> = vec![10.0, 20.0, -3.0, 1.5, -8.0];

    let cpu = CpuBackend;
    let a_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&a_data));
    let b_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&b_data));
    let mut y_cpu = cpu.alloc(20, Pool::Scratch).unwrap();
    cpu.add(&a_cpu, &b_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut a_buf = hip.alloc(20, Pool::Scratch).unwrap();
    hip.upload(&mut a_buf, 0, &pod_bytes(&a_data)).unwrap();
    let mut b_buf = hip.alloc(20, Pool::Scratch).unwrap();
    hip.upload(&mut b_buf, 0, &pod_bytes(&b_data)).unwrap();
    let mut y_buf = hip.alloc(20, Pool::Scratch).unwrap();
    hip.add(&a_buf, &b_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; 20];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-6);
}

#[test]
fn silu_mul_matches_cpu_on_asymmetric_data() {
    let gate_data: Vec<f32> = vec![0.0, 2.0, -1.5, 4.0];
    let up_data: Vec<f32> = vec![1.0, 3.0, 2.0, -0.5];

    let cpu = CpuBackend;
    let gate_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&gate_data));
    let up_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&up_data));
    let mut y_cpu = cpu.alloc(16, Pool::Scratch).unwrap();
    cpu.silu_mul(&gate_cpu, &up_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut gate_buf = hip.alloc(16, Pool::Scratch).unwrap();
    hip.upload(&mut gate_buf, 0, &pod_bytes(&gate_data))
        .unwrap();
    let mut up_buf = hip.alloc(16, Pool::Scratch).unwrap();
    hip.upload(&mut up_buf, 0, &pod_bytes(&up_data)).unwrap();
    let mut y_buf = hip.alloc(16, Pool::Scratch).unwrap();
    hip.silu_mul(&gate_buf, &up_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; 16];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-5);
}

#[test]
fn rope_matches_cpu_on_asymmetric_data() {
    let spec = RopeSpec {
        head_dim: 4,
        theta: 10000.0,
    };
    let q_data: Vec<f32> = vec![1.0, 2.0, -1.0, 0.5, 3.0, -2.0, 0.1, 4.0]; // 2 heads
    let k_data: Vec<f32> = vec![0.5, -0.5, 2.0, 1.0]; // 1 head
    let pos = 7u32;

    let cpu = CpuBackend;
    let mut q_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&q_data));
    let mut k_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&k_data));
    cpu.rope(&mut q_cpu, &mut k_cpu, pos, 1, &spec).unwrap();
    let want_q = as_f32_vec(&q_cpu.0);
    let want_k = as_f32_vec(&k_cpu.0);

    let hip = hip();
    let mut q_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.upload(&mut q_buf, 0, &pod_bytes(&q_data)).unwrap();
    let mut k_buf = hip.alloc(16, Pool::Scratch).unwrap();
    hip.upload(&mut k_buf, 0, &pod_bytes(&k_data)).unwrap();
    hip.rope(&mut q_buf, &mut k_buf, pos, 1, &spec).unwrap();
    let mut q_bytes = vec![0u8; 32];
    let mut k_bytes = vec![0u8; 16];
    hip.download(&q_buf, 0, &mut q_bytes).unwrap();
    hip.download(&k_buf, 0, &mut k_bytes).unwrap();

    assert_close(&as_f32_vec(&q_bytes), &want_q, 1e-4);
    assert_close(&as_f32_vec(&k_bytes), &want_k, 1e-4);
}

#[test]
fn attn_decode_matches_cpu_on_asymmetric_data() {
    // head_dim=4, n_q_heads=2, n_kv_heads=1 (GQA group=2), kv_len=3.
    let spec = AttnSpec {
        head_dim: 4,
        n_q_heads: 2,
        n_kv_heads: 1,
    };
    let q_data: Vec<f32> = vec![1.0, 0.5, -1.0, 2.0, 0.2, -0.3, 1.5, 0.8];
    let k_data: Vec<f32> = (0..12).map(|i| (i as f32) * 0.13 - 0.7).collect(); // 3 positions x 4
    let v_data: Vec<f32> = (0..12).map(|i| (i as f32) * -0.21 + 1.3).collect();
    let (k_data, v_data) = (f16_round(&k_data), f16_round(&v_data));

    let cpu = CpuBackend;
    let q_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&q_data));
    let k_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&k_data));
    let v_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&v_data));
    let kv_cpu = KvView {
        k: &k_cpu,
        v: &v_cpu,
        len: 3,
    };
    let mut o_cpu = cpu.alloc(32, Pool::Scratch).unwrap();
    cpu.attn(&q_cpu, &kv_cpu, &spec, &mut o_cpu).unwrap();
    let want = as_f32_vec(&o_cpu.0);

    let hip = hip();
    let mut q_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.upload(&mut q_buf, 0, &pod_bytes(&q_data)).unwrap();
    let k_buf = kv_upload(&hip, &k_data);
    let v_buf = kv_upload(&hip, &v_data);
    let kv_hip = KvView {
        k: &k_buf,
        v: &v_buf,
        len: 3,
    };
    let mut o_buf = hip.alloc(32, Pool::Scratch).unwrap();
    hip.attn(&q_buf, &kv_hip, &spec, &mut o_buf).unwrap();
    let mut o_bytes = vec![0u8; 32];
    hip.download(&o_buf, 0, &mut o_bytes).unwrap();

    assert_close(&as_f32_vec(&o_bytes), &want, 1e-4);
}

/// Every dimension is smaller than a WMMA tile; operands are rounded to f16.
#[test]
fn qmatmul_batched_matches_cpu_on_asymmetric_data() {
    let w_data: Vec<f32> = (0..12).map(|i| (i as f32) * 0.37 - 1.5).collect(); // [4 out, 3 in]
    let x_data: Vec<f32> = vec![1.0, -2.0, 0.5, 0.3, 1.1, -0.7, -1.0, 2.0, 0.2]; // 3 tokens x 3 in

    let cpu = CpuBackend;
    let w_cpu = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![4, 3],
        buf: crate::backend::cpu::CpuBuf(pod_bytes(&w_data)),
    };
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(3 * 4 * 4, Pool::Scratch).unwrap();
    cpu.qmatmul(&w_cpu, &x_cpu, &mut y_cpu).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(w_data.len() * 4, Pool::Weights).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let w_hip = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![4, 3],
        buf: w_buf,
    };
    let mut x_buf = hip.alloc(x_data.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(3 * 4 * 4, Pool::Scratch).unwrap();
    hip.qmatmul(&w_hip, &x_buf, &mut y_buf).unwrap();
    let mut y_bytes = vec![0u8; 3 * 4 * 4];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-2);
}

#[test]
fn rmsnorm_batched_matches_cpu_on_asymmetric_data() {
    let w_data: Vec<f32> = vec![1.0, 0.5, 2.0, 1.5];
    let x_data: Vec<f32> = vec![
        3.0, -4.0, 2.5, 0.1, -7.0, 1.0, 6.0, -2.0, 0.5, 1.5, -1.0, 2.0,
    ]; // 3 tokens x dim=4

    let cpu = CpuBackend;
    let w_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&w_data));
    let x_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&x_data));
    let mut y_cpu = cpu.alloc(48, Pool::Scratch).unwrap();
    cpu.rmsnorm(&w_cpu, &x_cpu, &mut y_cpu, 1e-5).unwrap();
    let want = as_f32_vec(&y_cpu.0);

    let hip = hip();
    let mut w_buf = hip.alloc(16, Pool::Scratch).unwrap();
    hip.upload(&mut w_buf, 0, &pod_bytes(&w_data)).unwrap();
    let mut x_buf = hip.alloc(48, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x_data)).unwrap();
    let mut y_buf = hip.alloc(48, Pool::Scratch).unwrap();
    hip.rmsnorm(&w_buf, &x_buf, &mut y_buf, 1e-5).unwrap();
    let mut y_bytes = vec![0u8; 48];
    hip.download(&y_buf, 0, &mut y_bytes).unwrap();

    assert_close(&as_f32_vec(&y_bytes), &want, 1e-4);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn embed_batched_matches_cpu_on_real_q4k_table() {
    let name = "token_embd.weight".to_string();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let shape = gguf
        .tensor_desc(&name)
        .expect("has token_embd.weight")
        .shape
        .clone();
    let &[vocab, n_embd] = shape.as_slice() else {
        panic!("expected rank-2 table")
    };
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");
    let ids = [12345u32, 1, 42];
    assert!(ids.iter().all(|&id| (id as usize) < vocab));

    let cpu = CpuBackend;
    let table_cpu = QWeight {
        dtype: GgmlDType::Q4K,
        shape: shape.clone(),
        buf: crate::backend::cpu::CpuBuf(raw.clone()),
    };
    let mut out_cpu = cpu.alloc(3 * n_embd * 4, Pool::Scratch).unwrap();
    cpu.embed(&table_cpu, &ids, &mut out_cpu).unwrap();
    let want = as_f32_vec(&out_cpu.0);

    let hip = hip();
    let mut table_buf = hip.alloc(raw.len(), Pool::Weights).unwrap();
    hip.upload(&mut table_buf, 0, &raw).unwrap();
    let table_hip = QWeight {
        dtype: GgmlDType::Q4K,
        shape,
        buf: table_buf,
    };
    let mut out_buf = hip.alloc(3 * n_embd * 4, Pool::Scratch).unwrap();
    hip.embed(&table_hip, &ids, &mut out_buf).unwrap();
    let mut out_bytes = vec![0u8; 3 * n_embd * 4];
    hip.download(&out_buf, 0, &mut out_bytes).unwrap();

    assert_close(&as_f32_vec(&out_bytes), &want, 5e-2);
}

#[test]
fn rope_batched_matches_cpu_on_asymmetric_data() {
    let spec = RopeSpec {
        head_dim: 4,
        theta: 10000.0,
    };
    // 3 tokens, 1 head each, head_dim=4.
    let q_data: Vec<f32> = vec![
        1.0, 2.0, -1.0, 0.5, 0.3, -0.8, 1.2, 0.1, -0.5, 1.5, 0.7, -1.2,
    ];
    let k_data: Vec<f32> = vec![
        0.5, -0.5, 2.0, 1.0, -1.0, 0.3, 0.9, -0.4, 1.1, 0.2, -0.6, 0.8,
    ];
    let start_pos = 5u32;

    let cpu = CpuBackend;
    let mut q_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&q_data));
    let mut k_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&k_data));
    cpu.rope(&mut q_cpu, &mut k_cpu, start_pos, 3, &spec)
        .unwrap();
    let want_q = as_f32_vec(&q_cpu.0);
    let want_k = as_f32_vec(&k_cpu.0);

    let hip = hip();
    let mut q_buf = hip.alloc(48, Pool::Scratch).unwrap();
    hip.upload(&mut q_buf, 0, &pod_bytes(&q_data)).unwrap();
    let mut k_buf = hip.alloc(48, Pool::Scratch).unwrap();
    hip.upload(&mut k_buf, 0, &pod_bytes(&k_data)).unwrap();
    hip.rope(&mut q_buf, &mut k_buf, start_pos, 3, &spec)
        .unwrap();
    let mut q_bytes = vec![0u8; 48];
    let mut k_bytes = vec![0u8; 48];
    hip.download(&q_buf, 0, &mut q_bytes).unwrap();
    hip.download(&k_buf, 0, &mut k_bytes).unwrap();

    assert_close(&as_f32_vec(&q_bytes), &want_q, 1e-4);
    assert_close(&as_f32_vec(&k_bytes), &want_k, 1e-4);
}

#[test]
fn attn_batched_causal_matches_cpu_on_asymmetric_data() {
    let spec = AttnSpec {
        head_dim: 4,
        n_q_heads: 2,
        n_kv_heads: 1,
    };
    // 3 query positions x 2 heads x head_dim=4.
    let q_data: Vec<f32> = (0..24).map(|i| (i as f32) * 0.11 - 1.1).collect();
    let k_data: Vec<f32> = (0..12).map(|i| (i as f32) * 0.13 - 0.7).collect(); // 3 positions x 4
    let v_data: Vec<f32> = (0..12).map(|i| (i as f32) * -0.21 + 1.3).collect();
    let (k_data, v_data) = (f16_round(&k_data), f16_round(&v_data));

    let cpu = CpuBackend;
    let q_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&q_data));
    let k_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&k_data));
    let v_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&v_data));
    let kv_cpu = KvView {
        k: &k_cpu,
        v: &v_cpu,
        len: 3,
    };
    let mut o_cpu = cpu.alloc(96, Pool::Scratch).unwrap();
    cpu.attn(&q_cpu, &kv_cpu, &spec, &mut o_cpu).unwrap();
    let want = as_f32_vec(&o_cpu.0);

    let hip = hip();
    let mut q_buf = hip.alloc(96, Pool::Scratch).unwrap();
    hip.upload(&mut q_buf, 0, &pod_bytes(&q_data)).unwrap();
    let k_buf = kv_upload(&hip, &k_data);
    let v_buf = kv_upload(&hip, &v_data);
    let kv_hip = KvView {
        k: &k_buf,
        v: &v_buf,
        len: 3,
    };
    let mut o_buf = hip.alloc(96, Pool::Scratch).unwrap();
    hip.attn(&q_buf, &kv_hip, &spec, &mut o_buf).unwrap();
    let mut o_bytes = vec![0u8; 96];
    hip.download(&o_buf, 0, &mut o_bytes).unwrap();

    assert_close(&as_f32_vec(&o_bytes), &want, 1e-4);
}

/// Head dimension 128 selects WMMA attention with f16 operands and f32 accumulation.
fn attn_prefill_real_shape_matches_cpu(n_q: usize, prefix: usize) {
    let spec = AttnSpec {
        head_dim: 128,
        n_q_heads: 32,
        n_kv_heads: 8,
    };
    let kv_len = prefix + n_q;
    let q_n = n_q * spec.n_q_heads * spec.head_dim;
    let kv_n = kv_len * spec.n_kv_heads * spec.head_dim;
    // Bounded, asymmetric, non-periodic-in-head values so no two heads or
    // positions coincide and softmax is neither flat nor saturated.
    let q_data: Vec<f32> = (0..q_n)
        .map(|i| (((i * 7919) % 1009) as f32 / 1009.0 - 0.5) * 0.6)
        .collect();
    let k_data: Vec<f32> = (0..kv_n)
        .map(|i| (((i * 104729) % 1013) as f32 / 1013.0 - 0.5) * 0.6)
        .collect();
    let v_data: Vec<f32> = (0..kv_n)
        .map(|i| (((i * 15485863) % 1019) as f32 / 1019.0 - 0.5) * 2.0)
        .collect();
    let (k_data, v_data) = (f16_round(&k_data), f16_round(&v_data));

    let cpu = CpuBackend;
    let q_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&q_data));
    let k_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&k_data));
    let v_cpu = crate::backend::cpu::CpuBuf(pod_bytes(&v_data));
    let kv_cpu = KvView {
        k: &k_cpu,
        v: &v_cpu,
        len: kv_len,
    };
    let mut o_cpu = cpu.alloc(q_n * 4, Pool::Scratch).unwrap();
    cpu.attn(&q_cpu, &kv_cpu, &spec, &mut o_cpu).unwrap();
    let want = as_f32_vec(&o_cpu.0);

    let hip = hip();
    let mut q_buf = hip.alloc(q_n * 4, Pool::Scratch).unwrap();
    hip.upload(&mut q_buf, 0, &pod_bytes(&q_data)).unwrap();
    let k_buf = kv_upload(&hip, &k_data);
    let v_buf = kv_upload(&hip, &v_data);
    let kv_hip = KvView {
        k: &k_buf,
        v: &v_buf,
        len: kv_len,
    };
    let mut o_buf = hip.alloc(q_n * 4, Pool::Scratch).unwrap();
    hip.attn(&q_buf, &kv_hip, &spec, &mut o_buf).unwrap();
    let mut o_bytes = vec![0u8; q_n * 4];
    hip.download(&o_buf, 0, &mut o_bytes).unwrap();

    let got = as_f32_vec(&o_bytes);
    let max_dev = got
        .iter()
        .zip(want.iter())
        .fold(0f32, |m, (g, w)| m.max((g - w).abs()));
    eprintln!("attn_prefill n_q={n_q} prefix={prefix}: max abs dev vs f32 CPU {max_dev:.2e}");
    assert_close(&got, &want, 1e-3);
}

// 20 and 100 queries end in a partial 16-query tile; 256 is all full tiles.
// The multi-query slots (1..qt) share one staged tile and carry their
// own online-softmax state -- the path a single-query launch never runs.
#[test]
fn attn_prefill_real_shape_matches_cpu_no_prefix() {
    attn_prefill_real_shape_matches_cpu(20, 0);
}

#[test]
fn attn_prefill_real_shape_matches_cpu_with_prefix() {
    attn_prefill_real_shape_matches_cpu(20, 9);
}

#[test]
fn attn_prefill_real_shape_matches_cpu_t100_qt3() {
    attn_prefill_real_shape_matches_cpu(100, 5);
}

#[test]
fn attn_prefill_real_shape_matches_cpu_t256_qt8() {
    attn_prefill_real_shape_matches_cpu(256, 0);
}

// n_q == 1 takes the flash-decoding path (split-KV partials + combine),
// f32 throughout. 2085 keys: 65 chunks, the last with 5 live keys of 32;
// 32 keys: exactly one full chunk; 1 key: one chunk, 31 masked lanes.
#[test]
fn attn_decode_real_shape_matches_cpu_at_2085_keys() {
    attn_prefill_real_shape_matches_cpu(1, 2084);
}

#[test]
fn attn_decode_real_shape_matches_cpu_at_one_full_chunk() {
    attn_prefill_real_shape_matches_cpu(1, 31);
}

#[test]
fn attn_decode_real_shape_matches_cpu_at_one_key() {
    attn_prefill_real_shape_matches_cpu(1, 0);
}

const PROMPT: &[u32] = &[1, 918, 6872, 446, 9760, 505];
const EXPECTED: &[u32] = &[12247, 281, 12247, 505, 395];

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn model_matches_llama_cpp_greedy_continuation_on_gpu() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let mut kv = model
        .new_kv_cache(&backend, 4096.min(model.config().n_ctx))
        .expect("new kv cache"); // mdma-serve's default; 32768 is 8.6GB and OOMs beside co-resident servers
    let mut ws = model.alloc_workspace(&backend, 1).expect("alloc workspace");

    let mut pos: u32 = 0;
    let mut logits = Vec::new();
    for &tok in PROMPT {
        logits = model
            .forward(&backend, &mut kv, &mut ws, tok, pos)
            .expect("forward");
        pos += 1;
    }

    let generated = greedy_continuation(
        &backend,
        &model,
        &mut kv,
        &mut ws,
        logits,
        pos,
        EXPECTED.len(),
    );
    assert_eq!(generated, EXPECTED);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn model_prefill_matches_llama_cpp_greedy_continuation_on_gpu() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let mut kv = model
        .new_kv_cache(&backend, 4096.min(model.config().n_ctx))
        .expect("new kv cache"); // mdma-serve's default; 32768 is 8.6GB and OOMs beside co-resident servers
    let mut prefill_ws = model
        .alloc_workspace(&backend, PROMPT.len())
        .expect("alloc prefill workspace");
    let mut ws = model
        .alloc_workspace(&backend, 1)
        .expect("alloc decode workspace");

    let logits = model
        .prefill(&backend, &mut kv, &mut prefill_ws, PROMPT, 0)
        .expect("prefill");
    let pos = PROMPT.len() as u32;
    let generated = greedy_continuation(
        &backend,
        &model,
        &mut kv,
        &mut ws,
        logits,
        pos,
        EXPECTED.len(),
    );
    assert_eq!(generated, EXPECTED);
}

#[test]
fn wmma_ab_fragment_packing_matches_reference_matmul() {
    let hip = hip();

    let mut a = vec![0f32; 256];
    let mut b = vec![0f32; 256];
    for m in 0..16 {
        for k in 0..16 {
            a[m * 16 + k] = m as f32 + 0.07 * k as f32;
        }
    }
    for k in 0..16 {
        for n in 0..16 {
            b[k * 16 + n] = n as f32 + 0.13 * k as f32;
        }
    }

    let mut want = vec![0f32; 256];
    for m in 0..16 {
        for n in 0..16 {
            let mut acc = 0f32;
            for k in 0..16 {
                acc += a[m * 16 + k] * b[k * 16 + n];
            }
            want[m * 16 + n] = acc;
        }
    }

    let mut a_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();
    hip.upload(&mut a_buf, 0, &pod_bytes(&a)).unwrap();
    let mut b_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();
    hip.upload(&mut b_buf, 0, &pod_bytes(&b)).unwrap();
    let mut d_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();

    hip.wmma_probe(&a_buf, &b_buf, &mut d_buf)
        .expect("wmma probe launch");

    let mut d_bytes = vec![0u8; 256 * 4];
    hip.download(&d_buf, 0, &mut d_bytes).unwrap();
    let got = as_f32_vec(&d_bytes);

    // WMMA rounds operands to f16.
    assert_close(&got, &want, 2.0);

    // This off-diagonal pair distinguishes the result from its transpose.
    assert!(
        (got[1] - got[16]).abs() > 1.0,
        "d[0][1] and d[1][0] are suspiciously close -- possible transpose"
    );
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn model_prefill_wmma_matches_cpu_at_t16_plus() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut gguf_cpu = GgufModel::open(fixture()).expect("open gguf");
    let tokenizer = Tokenizer::from_gguf(&gguf_cpu).expect("build tokenizer");
    let prompt = "The history of artificial intelligence began in antiquity, with myths and legends of artificial beings";
    let tokens = tokenizer.encode(prompt);
    assert!(
        tokens.len() >= 16,
        "prompt must exceed the WMMA dispatch threshold, got {} tokens",
        tokens.len()
    );

    let cpu_backend = CpuBackend;
    let cpu_model = Model::load(&cpu_backend, &mut gguf_cpu).expect("load cpu model");
    let mut cpu_kv = cpu_model
        .new_kv_cache(&cpu_backend, 4096.min(cpu_model.config().n_ctx))
        .expect("cpu kv");
    let mut cpu_ws = cpu_model
        .alloc_workspace(&cpu_backend, tokens.len())
        .expect("cpu workspace");
    let cpu_logits = cpu_model
        .prefill(&cpu_backend, &mut cpu_kv, &mut cpu_ws, &tokens, 0)
        .expect("cpu prefill");

    let hip_backend = hip();
    let mut gguf_hip = GgufModel::open(fixture()).expect("open gguf");
    let hip_model = Model::load(&hip_backend, &mut gguf_hip).expect("load hip model");
    let mut hip_kv = hip_model
        .new_kv_cache(&hip_backend, 4096.min(hip_model.config().n_ctx))
        .expect("hip kv");
    let mut hip_ws = hip_model
        .alloc_workspace(&hip_backend, tokens.len())
        .expect("hip workspace");
    let hip_logits = hip_model
        .prefill(&hip_backend, &mut hip_kv, &mut hip_ws, &tokens, 0)
        .expect("hip prefill (WMMA path)");

    let cpu_top = greedy(&cpu_logits);
    let hip_top = greedy(&hip_logits);
    eprintln!(
        "prefill T={}: cpu top {cpu_top} (cpu {:.4}, gpu {:.4}); gpu top {hip_top} (cpu {:.4}, gpu {:.4})",
        tokens.len(),
        cpu_logits[cpu_top as usize],
        hip_logits[cpu_top as usize],
        cpu_logits[hip_top as usize],
        hip_logits[hip_top as usize]
    );
    // Quantized activations may reorder near-tied logits.
    // Require both the CPU margin and the top-token logit drift to stay within 2%.
    if cpu_top != hip_top {
        let top = cpu_logits[cpu_top as usize];
        let margin = top - cpu_logits[hip_top as usize];
        assert!(
            margin.abs() < 0.02 * top.abs(),
            "CPU and GPU prefill disagree on the greedy token and it is not a near-tie: cpu {cpu_top} leads {hip_top} by {margin:.4} on {top:.4}"
        );
        let dev = (hip_logits[cpu_top as usize] - top).abs();
        assert!(
            dev < 0.02 * top.abs(),
            "GPU logit for the CPU's top token is off by {dev:.4} on {top:.4}"
        );
    }

    let cpu_top_logit = cpu_logits[cpu_top as usize];
    let hip_top_logit = hip_logits[hip_top as usize];
    let rel_diff = (cpu_top_logit - hip_top_logit).abs() / cpu_top_logit.abs().max(1.0);
    assert!(
        rel_diff < 0.3,
        "top logit magnitude diverged too far: cpu={cpu_top_logit}, hip={hip_top_logit}"
    );
}

use std::time::Instant;

fn time_kernel(iters: u32, mut f: impl FnMut()) -> std::time::Duration {
    f(); // warmup, not timed -- first launch pays module/JIT-adjacent cost
    rocm_rs::hip::device_synchronize().expect("sync after warmup");
    let start = Instant::now();
    for _ in 0..iters {
        f();
        // Include execution time, not just host-side enqueue time.
        rocm_rs::hip::device_synchronize().expect("sync after kernel");
    }
    start.elapsed() / iters
}

/// Amortize synchronization over a batch to measure per-call overhead.
fn time_kernel_batched(iters: u32, mut f: impl FnMut()) -> std::time::Duration {
    f();
    rocm_rs::hip::device_synchronize().expect("sync after warmup");
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    rocm_rs::hip::device_synchronize().expect("sync after batch");
    start.elapsed() / iters
}

/// Compare direct WMMA and scalar launches at identical batch sizes.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_wmma_vs_scalar_qmatmul_kernel() {
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let name = "blk.0.attn_q.weight".to_string();
    let desc = gguf.tensor_desc(&name).expect("has attn_q.weight").clone();
    let &[n_out, n_in] = desc.shape.as_slice() else {
        panic!("expected rank-2 weight")
    };
    let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

    let mut w_buf = backend.alloc(raw.len(), Pool::Weights).expect("alloc w");
    backend.upload(&mut w_buf, 0, &raw).expect("upload w");
    let dtype = crate::backend::dtype_code::code(desc.dtype).expect("dtype code");

    for &n_tokens in &[16usize, 64] {
        let x_data: Vec<f32> = (0..n_tokens * n_in)
            .map(|i| ((i % 13) as f32 - 6.0) * 0.1)
            .collect();
        let mut x_buf = backend
            .alloc(x_data.len() * 4, Pool::Scratch)
            .expect("alloc x");
        backend
            .upload(&mut x_buf, 0, bytemuck::cast_slice(&x_data))
            .expect("upload x");
        let y_buf = backend
            .alloc(n_tokens * n_out * 4, Pool::Scratch)
            .expect("alloc y");

        let mut dtype_mut = dtype;
        let mut n_out_i = n_out as i32;
        let mut n_in_i = n_in as i32;
        let mut n_tokens_i = n_tokens as i32;

        let scalar_time = time_kernel(5, || {
            backend
                .launch1d(
                    "qmatmul_gemm",
                    n_out,
                    &mut [
                        &mut w_buf.as_device_ptr() as *mut _ as *mut std::ffi::c_void,
                        &mut dtype_mut as *mut _ as *mut std::ffi::c_void,
                        &mut n_out_i as *mut _ as *mut std::ffi::c_void,
                        &mut n_in_i as *mut _ as *mut std::ffi::c_void,
                        &mut n_tokens_i as *mut _ as *mut std::ffi::c_void,
                        &mut x_buf.as_device_ptr() as *mut _ as *mut std::ffi::c_void,
                        &mut y_buf.as_device_ptr() as *mut _ as *mut std::ffi::c_void,
                    ],
                )
                .expect("scalar qmatmul_gemm launch")
        });

        let wmma_time = time_kernel(5, || {
            backend
                .qmatmul_wmma(
                    w_buf.as_device_ptr(),
                    dtype,
                    n_out as i32,
                    n_in as i32,
                    n_tokens as i32,
                    x_buf.as_device_ptr(),
                    y_buf.as_device_ptr(),
                )
                .expect("qmatmul_gemm_wmma launch")
        });

        let speedup = scalar_time.as_secs_f64() / wmma_time.as_secs_f64();
        eprintln!(
            "attn_q [{n_out}x{n_in}] T={n_tokens}: scalar={scalar_time:?}  wmma={wmma_time:?}  speedup={speedup:.2}x"
        );
    }
}

/// Compare GEMV against identical memory traffic with reduced arithmetic.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_gemv_valu_vs_memory_bound() {
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");

    for tensor in ["blk.0.attn_q.weight", "blk.0.ffn_gate.weight"] {
        let name = tensor.to_string();
        let desc = gguf.tensor_desc(&name).expect("has tensor").clone();
        assert_eq!(
            desc.dtype,
            GgmlDType::Q4K,
            "probe only implements the Q4_K path"
        );
        let &[n_out, n_in] = desc.shape.as_slice() else {
            panic!("expected rank-2 weight")
        };
        let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

        let mut w_buf = backend.alloc(raw.len(), Pool::Weights).expect("alloc w");
        backend.upload(&mut w_buf, 0, &raw).expect("upload w");
        let dtype = crate::backend::dtype_code::code(desc.dtype).expect("dtype code");

        let x_data: Vec<f32> = (0..n_in).map(|i| ((i % 13) as f32 - 6.0) * 0.1).collect();
        let mut x_buf = backend
            .alloc(x_data.len() * 4, Pool::Scratch)
            .expect("alloc x");
        backend
            .upload(&mut x_buf, 0, bytemuck::cast_slice(&x_data))
            .expect("upload x");
        let y_buf = backend.alloc(n_out * 4, Pool::Scratch).expect("alloc y");

        let n_out_i = n_out as i32;
        let n_in_i = n_in as i32;

        let real_time = time_kernel(10, || {
            backend
                .launch_gemv(
                    w_buf.as_device_ptr(),
                    dtype,
                    n_out_i,
                    n_in_i,
                    x_buf.as_device_ptr(),
                    y_buf.as_device_ptr(),
                )
                .expect("real qmatmul_gemv launch")
        });
        let probe_time = time_kernel(10, || {
            backend
                .launch_gemv_membound_probe(
                    w_buf.as_device_ptr(),
                    n_out_i,
                    n_in_i,
                    x_buf.as_device_ptr(),
                    y_buf.as_device_ptr(),
                )
                .expect("membound probe launch")
        });

        let valu_tax = real_time.as_secs_f64() / probe_time.as_secs_f64();
        eprintln!(
            "{tensor} [{n_out}x{n_in}] gemv: real={real_time:?}  membound_probe={probe_time:?}  real/probe={valu_tax:.2}x"
        );

        let batched_time = time_kernel_batched(50, || {
            backend
                .launch_gemv(
                    w_buf.as_device_ptr(),
                    dtype,
                    n_out_i,
                    n_in_i,
                    x_buf.as_device_ptr(),
                    y_buf.as_device_ptr(),
                )
                .expect("real qmatmul_gemv launch (batched)")
        });
        let overhead_ratio = real_time.as_secs_f64() / batched_time.as_secs_f64();
        eprintln!(
            "{tensor} [{n_out}x{n_in}] gemv: per-call-sync={real_time:?}  batched-sync={batched_time:?}  per-call/batched={overhead_ratio:.2}x"
        );
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_gemv_rows_per_block_sweep() {
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");

    for tensor in ["blk.0.attn_q.weight", "blk.0.ffn_gate.weight"] {
        let name = tensor.to_string();
        let desc = gguf.tensor_desc(&name).expect("has tensor").clone();
        let &[n_out, n_in] = desc.shape.as_slice() else {
            panic!("expected rank-2 weight")
        };
        let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

        let mut w_buf = backend.alloc(raw.len(), Pool::Weights).expect("alloc w");
        backend.upload(&mut w_buf, 0, &raw).expect("upload w");
        let dtype = crate::backend::dtype_code::code(desc.dtype).expect("dtype code");

        let x_data: Vec<f32> = (0..n_in).map(|i| ((i % 13) as f32 - 6.0) * 0.1).collect();
        let mut x_buf = backend
            .alloc(x_data.len() * 4, Pool::Scratch)
            .expect("alloc x");
        backend
            .upload(&mut x_buf, 0, bytemuck::cast_slice(&x_data))
            .expect("upload x");
        let y_buf = backend.alloc(n_out * 4, Pool::Scratch).expect("alloc y");

        let n_out_i = n_out as i32;
        let n_in_i = n_in as i32;

        for rows_per_block in [4u32, 8, 16, 32] {
            let t = time_kernel_batched(50, || {
                backend
                    .launch_gemv_rows_per_block(
                        w_buf.as_device_ptr(),
                        dtype,
                        n_out_i,
                        n_in_i,
                        x_buf.as_device_ptr(),
                        y_buf.as_device_ptr(),
                        rows_per_block,
                    )
                    .expect("gemv launch (swept rows_per_block)")
            });
            eprintln!("{tensor} [{n_out}x{n_in}] rows_per_block={rows_per_block}: {t:?}");
        }
    }
}

/// Compare WMMA against identical weight staging with reduced arithmetic.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_wmma_valu_vs_memory_bound() {
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");

    for tensor in ["blk.0.attn_q.weight", "blk.0.ffn_gate.weight"] {
        let name = tensor.to_string();
        let desc = gguf.tensor_desc(&name).expect("has tensor").clone();
        assert_eq!(
            desc.dtype,
            GgmlDType::Q4K,
            "probe only implements the Q4_K path"
        );
        let &[n_out, n_in] = desc.shape.as_slice() else {
            panic!("expected rank-2 weight")
        };
        let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");

        let mut w_buf = backend.alloc(raw.len(), Pool::Weights).expect("alloc w");
        backend.upload(&mut w_buf, 0, &raw).expect("upload w");
        let dtype = crate::backend::dtype_code::code(desc.dtype).expect("dtype code");

        for &n_tokens in &[16i32, 64] {
            let x_data: Vec<f32> = (0..n_tokens as usize * n_in)
                .map(|i| ((i % 13) as f32 - 6.0) * 0.1)
                .collect();
            let mut x_buf = backend
                .alloc(x_data.len() * 4, Pool::Scratch)
                .expect("alloc x");
            backend
                .upload(&mut x_buf, 0, bytemuck::cast_slice(&x_data))
                .expect("upload x");
            let y_buf = backend
                .alloc(n_tokens as usize * n_out * 4, Pool::Scratch)
                .expect("alloc y");

            let n_out_i = n_out as i32;
            let n_in_i = n_in as i32;

            let real_time = time_kernel_batched(20, || {
                backend
                    .qmatmul_wmma(
                        w_buf.as_device_ptr(),
                        dtype,
                        n_out_i,
                        n_in_i,
                        n_tokens,
                        x_buf.as_device_ptr(),
                        y_buf.as_device_ptr(),
                    )
                    .expect("real qmatmul_gemm_wmma launch")
            });
            let probe_time = time_kernel_batched(20, || {
                backend
                    .qmatmul_wmma_membound_probe(
                        w_buf.as_device_ptr(),
                        n_out_i,
                        n_in_i,
                        n_tokens,
                        x_buf.as_device_ptr(),
                        y_buf.as_device_ptr(),
                    )
                    .expect("wmma membound probe launch")
            });

            let dequant_tax = real_time.as_secs_f64() / probe_time.as_secs_f64();
            eprintln!(
                "{tensor} [{n_out}x{n_in}] T={n_tokens} wmma: real={real_time:?}  membound_probe={probe_time:?}  real/probe={dequant_tax:.2}x"
            );
        }
    }
}

/// Isolated int8 GEMM time per real shape and T, through the normal
/// qmatmul dispatch, with achieved TOPS. The unchanged input pointer
/// reuses cached Q8 activations after warmup, excluding quantization.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_i8_gemm_shapes() {
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    for tensor in [
        "blk.0.attn_q.weight",
        "blk.0.ffn_gate.weight",
        "blk.0.ffn_down.weight",
    ] {
        let name = tensor.to_string();
        let desc = gguf.tensor_desc(&name).expect("has tensor").clone();
        let &[n_out, n_in] = desc.shape.as_slice() else {
            panic!("expected rank-2 weight")
        };
        let raw = gguf.read_tensor_bytes(&name).expect("read raw bytes");
        let mut w_buf = backend.alloc(raw.len(), Pool::Weights).expect("alloc w");
        backend.upload(&mut w_buf, 0, &raw).expect("upload w");
        let w = QWeight {
            dtype: desc.dtype,
            shape: desc.shape.clone(),
            buf: w_buf,
        };
        for &n_tokens in &[16usize, 64, 256] {
            let x_data: Vec<f32> = (0..n_tokens * n_in)
                .map(|i| ((i % 13) as f32 - 6.0) * 0.1)
                .collect();
            let mut x_buf = backend
                .alloc(x_data.len() * 4, Pool::Scratch)
                .expect("alloc x");
            backend
                .upload(&mut x_buf, 0, bytemuck::cast_slice(&x_data))
                .expect("upload x");
            let mut y_buf = backend
                .alloc(n_tokens * n_out * 4, Pool::Scratch)
                .expect("alloc y");
            let t = time_kernel_batched(20, || {
                backend.qmatmul(&w, &x_buf, &mut y_buf).expect("qmatmul")
            });
            let flops = 2.0 * n_out as f64 * n_in as f64 * n_tokens as f64;
            eprintln!(
                "{tensor} {:?} [{n_out}x{n_in}] T={n_tokens}: {t:?}  {:.1} TOPS",
                desc.dtype,
                flops / t.as_secs_f64() / 1e12
            );
        }
    }
}

fn bench_positive_usize(name: &str, default: usize) -> usize {
    let value = match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a positive integer")),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    };
    assert!(value > 0, "{name} must be positive");
    value
}

/// Full Model::prefill wall time on the real fixture.
/// MDMA_BENCH_T restricts prompt length; MDMA_BENCH_CTX (default 4096)
/// and MDMA_BENCH_REPS (default 1) control capacity and warm samples.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_prefill_wall_time() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let tokenizer = Tokenizer::from_gguf(&gguf).expect("build tokenizer");
    let n_ctx = bench_positive_usize("MDMA_BENCH_CTX", 4096).min(model.config().n_ctx);
    let repetitions = bench_positive_usize("MDMA_BENCH_REPS", 1);

    let long_prompt = "The history of artificial intelligence began in antiquity, with myths and legends of \
        artificial beings endowed with intelligence or consciousness by master craftsmen. The study of mechanical \
        reasoning began in antiquity as philosophers sought to describe human thought as a mechanical process. \
        Greek mythology contains stories of mechanical men, and later thinkers built on these ideas across \
        centuries of philosophy, mathematics, and engineering, culminating in the modern field of computer science \
        and the digital computers built in the mid-twentieth century that made real machine intelligence possible.";

    // Longer prompts cycle the encoded text; this is a dense model.
    let base = tokenizer.encode(long_prompt);
    // MDMA_BENCH_T=256 restricts the sweep to one shape so a kernel-trace
    // profile (rocprofv3 --kernel-trace --stats) attributes to that shape.
    let shapes: Vec<usize> = match std::env::var("MDMA_BENCH_T") {
        Ok(t) => vec![t.parse().expect("MDMA_BENCH_T must be a token count")],
        Err(_) => vec![4, 8, 16, 64, 256],
    };
    for &n_tokens in &shapes {
        assert!(
            n_tokens > 0 && n_tokens <= n_ctx,
            "prompt must fit MDMA_BENCH_CTX"
        );
        let tokens: Vec<u32> = base.iter().cycle().take(n_tokens).copied().collect();
        let tokens = tokens.as_slice();

        let mut kv = model.new_kv_cache(&backend, n_ctx).expect("kv");
        let mut ws = model
            .alloc_workspace(&backend, n_tokens)
            .expect("workspace");

        // Allocation is outside timing; the cold call includes deferred
        // first-touch/setup. Every call downloads the final token's logits.
        let start = Instant::now();
        let _logits = model
            .prefill(&backend, &mut kv, &mut ws, tokens, 0)
            .expect("prefill");
        let cold = start.elapsed();
        for sample in 1..=repetitions {
            kv.set_len(0);
            let start = Instant::now();
            let _logits = model
                .prefill(&backend, &mut kv, &mut ws, tokens, 0)
                .expect("prefill (warm)");
            let warm = start.elapsed();
            eprintln!(
                "Model::prefill T={n_tokens}: cold {cold:?} ({:.1} tok/s), warm {warm:?} ({:.1} tok/s)",
                n_tokens as f64 / cold.as_secs_f64(),
                n_tokens as f64 / warm.as_secs_f64()
            );
            eprintln!(
                "BENCH kind=prefill tokens={n_tokens} ctx={n_ctx} sample={sample} elapsed_ns={}",
                warm.as_nanos()
            );
        }
    }
}

/// MDMA_BENCH_CTX and MDMA_BENCH_REPS work as for prefill.
/// MDMA_BENCH_GREEDY=0 excludes sampling and times positions 0..32,
/// while the default retains greedy positions 1..=32.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_decode_wall_time() {
    let _guard = GPU_MODEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let backend = hip();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let n_ctx = bench_positive_usize("MDMA_BENCH_CTX", 4096).min(model.config().n_ctx);
    let repetitions = bench_positive_usize("MDMA_BENCH_REPS", 1);
    let sample_greedy = match std::env::var("MDMA_BENCH_GREEDY").as_deref() {
        Ok("0") => false,
        Ok("1") | Err(std::env::VarError::NotPresent) => true,
        _ => panic!("MDMA_BENCH_GREEDY must be 0 or 1"),
    };

    let n_steps = 32u32;
    let prefix: u32 = std::env::var("MDMA_BENCH_PREFIX")
        .ok()
        .map(|s| s.parse().expect("MDMA_BENCH_PREFIX must be a u32"))
        .unwrap_or(0);
    let first_pos = prefix.max(u32::from(sample_greedy));
    assert!(
        n_ctx >= (first_pos + n_steps) as usize,
        "decode must fit MDMA_BENCH_CTX"
    );
    let mut kv = model.new_kv_cache(&backend, n_ctx).expect("kv");
    let mut ws = model.alloc_workspace(&backend, 1).expect("workspace");
    benchmark_prefix(&backend, &model, &mut kv, prefix);

    let setup = Instant::now();
    let mut decoder = backend.decoder(&model, &mut kv, &mut ws).expect("decoder");
    eprintln!(
        "DECODE_SETUP graph={} elapsed_ns={}",
        decoder.is_graph(),
        setup.elapsed().as_nanos()
    );
    let warmup_logits = decoder.forward(1u32, prefix).expect("warmup forward");
    for sample in 1..=repetitions {
        let mut logits = warmup_logits.clone();
        let start = Instant::now();
        for pos in first_pos..first_pos + n_steps {
            let next = if sample_greedy {
                greedy(&logits)
            } else {
                pos % 128 + 1
            };
            logits = decoder.forward(next, pos).expect("forward");
        }
        let elapsed = start.elapsed();
        eprintln!(
            "Model::forward (decode) x{n_steps}: {elapsed:?} total, {:.2} tok/s",
            n_steps as f64 / elapsed.as_secs_f64()
        );
        eprintln!(
            "BENCH kind=decode tokens={n_steps} ctx={n_ctx} sample={sample} greedy={sample_greedy} prefix={prefix} elapsed_ns={}",
            elapsed.as_nanos()
        );
    }
}

#[test]
fn wmma_i8_fragment_packing_matches_reference_matmul() {
    let hip = hip();

    let mut a = vec![0i32; 256];
    let mut b = vec![0i32; 256];
    let mut c = vec![0i32; 256];
    for m in 0..16 {
        for k in 0..16 {
            a[m * 16 + k] = ((m as i32 * 7 + k as i32 * 3) % 23) - 11; // -11..11
        }
    }
    for k in 0..16 {
        for n in 0..16 {
            b[k * 16 + n] = ((k as i32 * 5 + n as i32 * 11) % 29) - 14; // -14..14
        }
    }
    for (i, v) in c.iter_mut().enumerate() {
        *v = (i as i32 * 13) % 17 - 8;
    }
    // Pin the extremes too: a lane holding -128 or 127 must survive packing.
    a[0] = -128;
    a[17] = 127;
    b[255] = -128;
    b[16] = 127;

    let mut want = vec![0i32; 256];
    for m in 0..16 {
        for n in 0..16 {
            let mut acc = c[m * 16 + n];
            for k in 0..16 {
                acc += a[m * 16 + k] * b[k * 16 + n];
            }
            want[m * 16 + n] = acc;
        }
    }

    let mut a_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();
    hip.upload(&mut a_buf, 0, &pod_bytes(&a)).unwrap();
    let mut b_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();
    hip.upload(&mut b_buf, 0, &pod_bytes(&b)).unwrap();
    let mut c_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();
    hip.upload(&mut c_buf, 0, &pod_bytes(&c)).unwrap();
    let mut d_buf = hip.alloc(256 * 4, Pool::Scratch).unwrap();

    hip.wmma_i8_probe(&a_buf, &b_buf, &c_buf, &mut d_buf)
        .expect("wmma i8 probe launch");

    let mut d_bytes = vec![0u8; 256 * 4];
    hip.download(&d_buf, 0, &mut d_bytes).unwrap();
    let got: Vec<i32> = bytemuck::cast_slice(&d_bytes).to_vec();

    for m in 0..16 {
        for n in 0..16 {
            assert_eq!(got[m * 16 + n], want[m * 16 + n], "D[{m}][{n}]");
        }
    }
}

/// quantize_q8_1_kernel against a Rust reference of the *same*
/// representation (d = amax/127 rounded to f16 as in ggml's block_q8_1,
/// q = round-half-away-from-zero of x*127/amax, and the block sum of q
/// as an exact float). Covers negatives, a block whose amax sits
/// on a negative value, and an all-zero block (d=0, q=0, no division by
/// zero). Exact on q8 -- the GPU computes x*inv the same way -- and
/// tight on the float metadata.
#[test]
fn quantize_q8_1_matches_reference() {
    let hip = hip();
    let n_in = 64usize; // 2 blocks per token
    let n_tokens = 3usize;
    let mut x = vec![0f32; n_in * n_tokens];
    for t in 0..n_tokens {
        for i in 0..n_in {
            x[t * n_in + i] = ((i as f32) * 0.37 - 9.5)
                * (1.0 + t as f32 * 0.5)
                * if i % 3 == 0 { -1.0 } else { 1.0 };
        }
    }
    // Token 2, block 1: all zeros.
    for i in 32..64 {
        x[2 * n_in + i] = 0.0;
    }

    let nb = n_in / 32;
    let mut want_q = vec![0i8; n_in * n_tokens];
    let mut want_ys = vec![0f32; n_tokens * nb * 2];
    for t in 0..n_tokens {
        for b in 0..nb {
            let xb = &x[t * n_in + b * 32..t * n_in + b * 32 + 32];
            let amax = xb.iter().fold(0f32, |m, v| m.max(v.abs()));
            let d = half::f16::from_f32(amax / 127.0).to_f32();
            let inv = if amax > 0.0 { 127.0 / amax } else { 0.0 };
            let mut sum_q = 0i32;
            for i in 0..32 {
                let q = (xb[i] * inv).round() as i32;
                want_q[t * n_in + b * 32 + i] = q as i8;
                sum_q += q;
            }
            want_ys[(t * nb + b) * 2] = d;
            want_ys[(t * nb + b) * 2 + 1] = sum_q as f32;
        }
    }

    let mut x_buf = hip.alloc(x.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut x_buf, 0, &pod_bytes(&x)).unwrap();
    let mut q_buf = hip.alloc(n_in * n_tokens, Pool::Scratch).unwrap();
    let mut ys_buf = hip.alloc(n_tokens * nb * 8, Pool::Scratch).unwrap();
    hip.quantize_q8_1(
        &x_buf,
        n_in as i32,
        n_tokens as i32,
        &mut q_buf,
        &mut ys_buf,
    )
    .expect("quantize launch");

    let mut q_bytes = vec![0u8; n_in * n_tokens];
    hip.download(&q_buf, 0, &mut q_bytes).unwrap();
    let got_q: Vec<i8> = q_bytes.iter().map(|&b| b as i8).collect();
    assert_eq!(got_q, want_q);

    let mut ys_bytes = vec![0u8; n_tokens * nb * 8];
    hip.download(&ys_buf, 0, &mut ys_bytes).unwrap();
    let got_ys = as_f32_vec(&ys_bytes);
    for (i, (g, w)) in got_ys.iter().zip(want_ys.iter()).enumerate() {
        assert!(
            (g - w).abs() <= 1e-5 * w.abs().max(1.0),
            "ys[{i}]: got {g}, want {w}"
        );
    }
}
