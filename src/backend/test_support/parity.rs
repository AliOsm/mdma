//! Shared decoder-op scenarios. Keep inputs and tolerances fixed here;
//! callers still select the backend and its numerical execution mode.
use super::{bytes, close, f32s, qmatmul_matches_cpu};
use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::backend::{AttnSpec, Backend, KvView, Pool, QWeight, RopeSpec};
use crate::gguf::GgmlDType;

pub(crate) fn asymmetric_matmul<B: Backend>(gpu: &B) {
    let w: Vec<f32> = (0..12).map(|i| (i as f32) * 0.37 - 1.5).collect();
    let x = vec![1.0f32, -2.0, 0.5, 3.0, 0.25, -1.0, 2.0, -0.5];
    qmatmul_matches_cpu(gpu, GgmlDType::F32, &bytes(&w), &[3, 4], &x, 1e-4);
}

pub(crate) fn matmul_row_tail<B: Backend>(gpu: &B) {
    let n_out = 20;
    let n_in = 64;
    let w: Vec<f32> = (0..n_out * n_in)
        .map(|i| ((i % 19) as f32 - 9.0) * 0.17)
        .collect();
    let x: Vec<f32> = (0..n_in * 3)
        .map(|i| ((i % 7) as f32 - 3.0) * 0.41)
        .collect();
    qmatmul_matches_cpu(gpu, GgmlDType::F32, &bytes(&w), &[n_out, n_in], &x, 1e-3);
}

pub(crate) fn elementwise_norm_rope<B: Backend>(gpu: &B) {
    let cpu = CpuBackend;
    let a: Vec<f32> = (0..16).map(|i| (i as f32) * 0.3 - 2.0).collect();
    let b: Vec<f32> = (0..16).map(|i| (i as f32) * -0.2 + 0.7).collect();
    let mut y_cpu = cpu.alloc(64, Pool::Scratch).unwrap();
    let a_cpu = CpuBuf(bytes(&a));
    let b_cpu = CpuBuf(bytes(&b));
    cpu.add(&a_cpu, &b_cpu, &mut y_cpu).unwrap();
    cpu.silu_mul(&a_cpu, &b_cpu, &mut y_cpu).unwrap();
    let want_silu = f32s(&y_cpu.0);

    let mut a_g = gpu.alloc(64, Pool::Scratch).unwrap();
    let mut b_g = gpu.alloc(64, Pool::Scratch).unwrap();
    gpu.upload(&mut a_g, 0, &bytes(&a)).unwrap();
    gpu.upload(&mut b_g, 0, &bytes(&b)).unwrap();
    let mut y_g = gpu.alloc(64, Pool::Scratch).unwrap();
    gpu.add(&a_g, &b_g, &mut y_g).unwrap();
    let mut got = vec![0u8; 64];
    gpu.download(&y_g, 0, &mut got).unwrap();
    close(
        &f32s(&got),
        &a.iter().zip(&b).map(|(x, y)| x + y).collect::<Vec<_>>(),
        1e-5,
    );
    gpu.silu_mul(&a_g, &b_g, &mut y_g).unwrap();
    gpu.download(&y_g, 0, &mut got).unwrap();
    close(&f32s(&got), &want_silu, 1e-5);

    let w: Vec<f32> = (0..8).map(|i| 0.5 + (i as f32) * 0.1).collect();
    let x: Vec<f32> = (0..16).map(|i| ((i % 5) as f32) - 2.0).collect();
    let w_cpu = CpuBuf(bytes(&w));
    let x_cpu = CpuBuf(bytes(&x));
    let mut y_cpu = cpu.alloc(64, Pool::Scratch).unwrap();
    cpu.rmsnorm(&w_cpu, &x_cpu, &mut y_cpu, 1e-5).unwrap();
    let mut w_g = gpu.alloc(32, Pool::Weights).unwrap();
    let mut x_g = gpu.alloc(64, Pool::Scratch).unwrap();
    gpu.upload(&mut w_g, 0, &bytes(&w)).unwrap();
    gpu.upload(&mut x_g, 0, &bytes(&x)).unwrap();
    gpu.rmsnorm(&w_g, &x_g, &mut y_g, 1e-5).unwrap();
    gpu.download(&y_g, 0, &mut got).unwrap();
    close(&f32s(&got), &f32s(&y_cpu.0), 1e-5);

    let head = 8usize;
    let spec = RopeSpec {
        head_dim: head,
        theta: 10000.0,
    };
    let q: Vec<f32> = (0..2 * 2 * head)
        .map(|i| ((i % 9) as f32) * 0.2 - 0.8)
        .collect();
    let k: Vec<f32> = (0..2 * head)
        .map(|i| ((i % 5) as f32) * 0.3 - 0.4)
        .collect();
    let mut q_cpu = CpuBuf(bytes(&q));
    let mut k_cpu = CpuBuf(bytes(&k));
    cpu.rope(&mut q_cpu, &mut k_cpu, 7, 2, &spec).unwrap();
    let mut q_g = gpu.alloc(q.len() * 4, Pool::Scratch).unwrap();
    let mut k_g = gpu.alloc(k.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut q_g, 0, &bytes(&q)).unwrap();
    gpu.upload(&mut k_g, 0, &bytes(&k)).unwrap();
    gpu.rope(&mut q_g, &mut k_g, 7, 2, &spec).unwrap();
    let mut q_out = vec![0u8; q.len() * 4];
    let mut k_out = vec![0u8; k.len() * 4];
    gpu.download(&q_g, 0, &mut q_out).unwrap();
    gpu.download(&k_g, 0, &mut k_out).unwrap();
    close(&f32s(&q_out), &f32s(&q_cpu.0), 1e-4);
    close(&f32s(&k_out), &f32s(&k_cpu.0), 1e-4);
}

/// Only f32-KV backends: CUDA/ROCm keep their f16-cache scenarios.
pub(crate) fn attention_f32_prefix<B: Backend>(gpu: &B) {
    assert_eq!(gpu.kv_elem_bytes(), 4, "this scenario uploads an f32 cache");
    let spec = AttnSpec {
        head_dim: 8,
        n_q_heads: 4,
        n_kv_heads: 2,
    };
    let n_q = 3;
    let kv_len = 5;
    let q: Vec<f32> = (0..n_q * spec.n_q_heads * spec.head_dim)
        .map(|i| ((i % 11) as f32) * 0.13 - 0.5)
        .collect();
    let k: Vec<f32> = (0..kv_len * spec.n_kv_heads * spec.head_dim)
        .map(|i| ((i % 7) as f32) * 0.17 - 0.3)
        .collect();
    let v: Vec<f32> = (0..k.len()).map(|i| ((i % 5) as f32) * 0.2 - 0.2).collect();
    let cpu = CpuBackend;
    let q_cpu = CpuBuf(bytes(&q));
    let k_cpu = CpuBuf(bytes(&k));
    let v_cpu = CpuBuf(bytes(&v));
    let mut o_cpu = cpu.alloc(q.len() * 4, Pool::Scratch).unwrap();
    cpu.attn(
        &q_cpu,
        &KvView {
            k: &k_cpu,
            v: &v_cpu,
            len: kv_len,
        },
        &spec,
        &mut o_cpu,
    )
    .unwrap();

    let mut q_g = gpu.alloc(q.len() * 4, Pool::Scratch).unwrap();
    let mut k_g = gpu.alloc(k.len() * 4, Pool::Scratch).unwrap();
    let mut v_g = gpu.alloc(v.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut q_g, 0, &bytes(&q)).unwrap();
    gpu.upload(&mut k_g, 0, &bytes(&k)).unwrap();
    gpu.upload(&mut v_g, 0, &bytes(&v)).unwrap();
    let mut o_g = gpu.alloc(q.len() * 4, Pool::Scratch).unwrap();
    gpu.attn(
        &q_g,
        &KvView {
            k: &k_g,
            v: &v_g,
            len: kv_len,
        },
        &spec,
        &mut o_g,
    )
    .unwrap();
    let mut out = vec![0u8; q.len() * 4];
    gpu.download(&o_g, 0, &mut out).unwrap();
    close(&f32s(&out), &f32s(&o_cpu.0), 1e-4);
}

pub(crate) fn embed_f32<B: Backend>(gpu: &B) {
    let vocab = 6usize;
    let n = 4usize;
    let table: Vec<f32> = (0..vocab * n).map(|i| (i as f32) * 0.25 - 1.0).collect();
    let ids = [5u32, 0, 3];
    let cpu = CpuBackend;
    let w = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![vocab, n],
        buf: CpuBuf(bytes(&table)),
    };
    let mut out_cpu = cpu.alloc(ids.len() * n * 4, Pool::Scratch).unwrap();
    cpu.embed(&w, &ids, &mut out_cpu).unwrap();
    let mut buf = gpu.alloc(table.len() * 4, Pool::Weights).unwrap();
    gpu.upload(&mut buf, 0, &bytes(&table)).unwrap();
    let w_g = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![vocab, n],
        buf,
    };
    let mut out = gpu.alloc(ids.len() * n * 4, Pool::Scratch).unwrap();
    gpu.embed(&w_g, &ids, &mut out).unwrap();
    let mut got = vec![0u8; ids.len() * n * 4];
    gpu.download(&out, 0, &mut got).unwrap();
    close(&f32s(&got), &f32s(&out_cpu.0), 1e-6);
}

/// Both binary kernels at the local-size boundaries used by the native
/// launch helpers. Exact-size, nonempty outputs work with every backend's
/// shape policy; empty-buffer behavior remains backend-specific.
pub(crate) fn binary_dispatch_tails<B: Backend>(gpu: &B) {
    for n in [1, 63, 64, 65, 255, 256, 257] {
        let a: Vec<f32> = (0..n).map(|i| (i % 17) as f32 * 0.2 - 1.5).collect();
        let b: Vec<f32> = (0..n).map(|i| (i % 11) as f32 * -0.3 + 0.7).collect();
        let mut ga = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        let mut gb = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        gpu.upload(&mut ga, 0, &bytes(&a)).unwrap();
        gpu.upload(&mut gb, 0, &bytes(&b)).unwrap();
        let mut out = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        for silu in [false, true] {
            let run = |b: &B::Buf, y: &mut B::Buf| {
                if silu {
                    gpu.silu_mul(&ga, b, y)
                } else {
                    gpu.add(&ga, b, y)
                }
            };
            run(&gb, &mut out).unwrap();
            let mut got = vec![0; n * 4];
            gpu.download(&out, 0, &mut got).unwrap();
            let want: Vec<f32> = a
                .iter()
                .zip(&b)
                .map(|(&a, &b)| {
                    if silu {
                        a / (1.0 + (-a).exp()) * b
                    } else {
                        a + b
                    }
                })
                .collect();
            close(&f32s(&got), &want, 1e-5);
            let mut short = gpu.alloc((n - 1) * 4, Pool::Scratch).unwrap();
            assert!(run(&gb, &mut short).is_err());
            assert!(run(&short, &mut out).is_err());
        }
    }
}

#[test]
fn shared_decoder_scenarios_on_cpu() {
    asymmetric_matmul(&CpuBackend);
    matmul_row_tail(&CpuBackend);
    elementwise_norm_rope(&CpuBackend);
    attention_f32_prefix(&CpuBackend);
    embed_f32(&CpuBackend);
}

#[test]
fn binary_dispatch_boundaries_on_cpu() {
    binary_dispatch_tails(&CpuBackend);
}
