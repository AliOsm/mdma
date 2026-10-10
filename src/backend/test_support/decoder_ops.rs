//! Compatibility checks for optional decoder primitives.

#[cfg(test)]
mod tests {
    use crate::backend::{AttnSpec, Backend, KvView, Operation, Pool, RopeSpec, cpu::CpuBackend};

    fn upload<B: Backend>(b: &B, x: &[f32]) -> B::Buf {
        let mut buf = b.alloc(x.len() * 4, Pool::Scratch).unwrap();
        b.upload(&mut buf, 0, bytemuck::cast_slice(x)).unwrap();
        buf
    }

    fn read<B: Backend>(b: &B, x: &B::Buf, n: usize) -> Vec<f32> {
        let mut data = vec![0f32; n];
        b.download(x, 0, bytemuck::cast_slice_mut(&mut data)).unwrap();
        data
    }

    fn kv_buffers<B: Backend>(b: &B, k: &[f32], v: &[f32]) -> (B::Buf, B::Buf) {
        let mut dk = b.alloc(k.len() * b.kv_elem_bytes(), Pool::Kv).unwrap();
        let mut dv = b.alloc(v.len() * b.kv_elem_bytes(), Pool::Kv).unwrap();
        b.kv_write(&upload(b, k), &upload(b, v), &mut dk, &mut dv, 0, k.len()).unwrap();
        (dk, dv)
    }

    fn check<B: Backend>(b: &B) {
        for operation in [Operation::Scale, Operation::GeluTanhMul, Operation::Softcap,
            Operation::RopeSplitHalf, Operation::WindowedAttention] {
            assert!(b.supports(operation));
        }
        let x = upload(b, &[-1.0, 0.0, 1.0]);
        let ones = upload(b, &[1.0; 3]);
        let mut out = b.alloc(12, Pool::Scratch).unwrap();
        b.gelu_tanh_mul(&x, &ones, &mut out).unwrap();
        for (got, want) in read(b, &out, 3).iter().zip([-0.158808, 0.0, 0.841192]) {
            assert!((got - want).abs() < 2e-6);
        }
        b.scale(&mut out, 2.0).unwrap();
        b.softcap(&mut out, 1.0).unwrap();
        assert!(read(b, &out, 3).iter().all(|v| v.abs() < 1.0));
        for cap in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(b.softcap(&mut out, cap).is_err());
        }
        for factor in [f32::NAN, f32::INFINITY] {
            assert!(b.scale(&mut out, factor).is_err());
        }

        // Dispatch tails must not overwrite the canary.
        for n in [1, 255, 256, 257] {
            let values: Vec<_> = (0..n).map(|i| (i % 17) as f32 / 8.0 - 1.0).collect();
            let x = upload(b, &values);
            let ones = upload(b, &vec![1.0; n]);
            let mut out = upload(b, &vec![12345.0; n + 7]);
            b.gelu_tanh_mul(&x, &ones, &mut out).unwrap();
            let got = read(b, &out, n + 7);
            for (&x, &got) in values.iter().zip(&got) {
                let want = 0.5 * x * (1.0 + (0.7978846 * x * (1.0 + 0.044715 * x * x)).tanh());
                assert!((got - want).abs() < 2e-6);
            }
            assert_eq!(&got[n..], &[12345.0; 7]);
            let mut unary = upload(b, &values);
            b.scale(&mut unary, -2.0).unwrap();
            b.softcap(&mut unary, 1.5).unwrap();
            for (&x, got) in values.iter().zip(read(b, &unary, n)) {
                assert!((got - (-2.0 * x / 1.5).tanh() * 1.5).abs() < 2e-6);
            }
        }

        // Split-half, not interleaved RoPE; infinity disables the second pair.
        let mut q = upload(b, &[1.0, 2.0, 3.0, 4.0]);
        let factors = upload(b, &[1.0, f32::INFINITY]);
        b.rope_split_half(&mut q, 1, &RopeSpec { head_dim: 4, theta: 10000.0 }, &factors).unwrap();
        let expected = [1f32.cos() - 3.0 * 1f32.sin(), 2.0, 1f32.sin() + 3.0 * 1f32.cos(), 4.0];
        for (got, want) in read(b, &q, 4).iter().zip(expected) {
            assert!((got - want).abs() < 1e-5);
        }

        // At/before/after a 512-token sliding window, including
        // 512-wide global heads (the Metal SIMD path uses 16 values/lane).
        for dim in [256, 512] {
            for len in [511, 512, 513] {
                let values: Vec<_> = (0..len).flat_map(|i| vec![i as f32; dim]).collect();
                let (k, v) = kv_buffers(b, &vec![0.0; len * dim], &values);
                let q = upload(b, &vec![0.0; dim]);
                let mut out = b.alloc(dim * 4, Pool::Scratch).unwrap();
                let spec = AttnSpec { head_dim: dim, n_q_heads: 1, n_kv_heads: 1 };
                for window in [0, 512] {
                    b.attn_windowed(&q, &KvView { k: &k, v: &v, len }, &spec, 1.0, window, &mut out).unwrap();
                    let start = if window == 0 { 0 } else { len.saturating_sub(window) };
                    let expected = (start + len - 1) as f32 / 2.0;
                    assert!(read(b, &out, dim).iter().all(|v| (v - expected).abs() < 2e-3));
                }
            }
        }

        // The score multiplier is explicit, not fixed to 1/sqrt(head_dim).
        let q = upload(b, &[1.0, 0.0]);
        let (k, v) = kv_buffers(b, &[0.0, 0.0, 1.0, 0.0], &[0.0, 0.0, 1.0, 1.0]);
        let mut out = b.alloc(8, Pool::Scratch).unwrap();
        let spec = AttnSpec { head_dim: 2, n_q_heads: 1, n_kv_heads: 1 };
        let kv = KvView { k: &k, v: &v, len: 2 };
        for scale in [0.0f32, 0.5, 1.0, 2.0] {
            b.attn_windowed(&q, &kv, &spec, scale, 0, &mut out).unwrap();
            let expected = 1.0 / (1.0 + (-scale).exp());
            assert!(read(b, &out, 2).iter().all(|v| (v - expected).abs() < 1e-6));
        }
        for scale in [f32::NAN, f32::INFINITY] {
            assert!(b.attn_windowed(&q, &kv, &spec, scale, 0, &mut out).is_err());
        }
        if b.supports(Operation::AddBias) {
            let mut rows = upload(b, &[1.0; 10]);
            let bias = upload(b, &[-2.0, -1.0, 0.0, 1.0, 2.0]);
            b.add_bias(&bias, &mut rows).unwrap();
            assert_eq!(read(b, &rows, 10), [-1.0, 0.0, 1.0, 2.0, 3.0, -1.0, 0.0, 1.0, 2.0, 3.0]);
            let wrong = upload(b, &[0.0; 3]);
            assert!(b.add_bias(&wrong, &mut rows).is_err());
        }
        if b.supports(Operation::RopePartialInterleaved) {
            for hd in [8, 128] {
                for rot in [2, hd / 2, hd] {
                    for heads in [1, 3, 9] {
                        let values: Vec<_> = (0..heads * hd).map(|i| (i as f32 * 0.13).sin()).collect();
                        let spec = RopeSpec { head_dim: hd, theta: 10000.0 };
                        let mut data = upload(b, &values);
                        b.rope_partial_interleaved(&mut data, 7, &spec, rot).unwrap();
                        let got = read(b, &data, values.len());
                        for (before, after) in values.chunks_exact(hd).zip(got.chunks_exact(hd)) {
                            assert_eq!(&before[rot..], &after[rot..], "unrotated suffix must be unchanged");
                            for i in (0..rot).step_by(2) {
                                let angle = 7.0 / 10000f32.powf(i as f32 / rot as f32);
                                let (s, c) = angle.sin_cos();
                                assert!((after[i] - (before[i] * c - before[i + 1] * s)).abs() < 1e-5);
                                assert!((after[i + 1] - (before[i] * s + before[i + 1] * c)).abs() < 1e-5);
                            }
                        }
                    }
                }
            }
            let spec = RopeSpec { head_dim: 8, theta: 10000.0 };
            for rot in [0, 1, 10] {
                assert!(b.rope_partial_interleaved(&mut upload(b, &[1.0; 8]), 0, &spec, rot).is_err());
            }
            assert!(b.rope_partial_interleaved(&mut upload(b, &[1.0; 9]), 0, &spec, 4).is_err());
            assert!(b.rope_partial_interleaved(&mut upload(b, &[]), 0, &spec, 4).is_err());
            for theta in [0.0, -1.0, f32::NAN, f32::INFINITY] {
                assert!(b.rope_partial_interleaved(&mut upload(b, &[1.0; 8]), 0, &RopeSpec { head_dim: 8, theta }, 4).is_err());
            }
        }
    }

    #[cfg(any(feature = "rocm", feature = "vulkan", feature = "cuda"))]
    fn check_gpu<B: Backend>(b: &B) {
        check(b);
        let x = upload(b, &[]);
        let mut out = upload(b, &[12345.0]);
        b.gelu_tanh_mul(&x, &x, &mut out).unwrap();
        assert_eq!(read(b, &out, 1), [12345.0]);
        let mut empty = upload(b, &[]);
        b.scale(&mut empty, 2.0).unwrap();
        b.softcap(&mut empty, 1.0).unwrap();
    }

    #[test]
    fn cpu_decoder_ops_compat() { check(&CpuBackend); }

    #[cfg(feature = "cuda")]
    #[test]
    fn cuda_decoder_ops_compat() { check_gpu(&crate::backend::cuda::CudaBackend::new().unwrap()); }

    #[cfg(feature = "vulkan")]
    #[test]
    fn vulkan_decoder_ops_compat() { check_gpu(&crate::backend::vulkan::VulkanBackend::new().unwrap()); }

    #[cfg(feature = "rocm")]
    #[test]
    fn rocm_decoder_ops_compat() {
        check_gpu(&crate::backend::rocm::HipBackend::new().unwrap());
        check_gpu(&crate::backend::rocm::HipBackend::new_f32().unwrap());
    }

    #[cfg(all(feature = "metal", target_os = "macos"))]
    #[test]
    fn metal_decoder_ops_compat() { check(&crate::backend::metal::MetalBackend::new().unwrap()); }
}
