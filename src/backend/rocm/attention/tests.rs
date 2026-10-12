use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::backend::rocm::HipBackend;
use crate::backend::test_support::upload_f16_kv;
use crate::backend::{AttnSpec, Backend, KvView, Pool};

#[test]
fn decode_hd128_matches_cpu_across_wave_and_context_tails() {
    let hip = HipBackend::new().unwrap();
    for len in [
        1, 2, 7, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 1024, 4096,
    ] {
        for amplitude in [0.0, 1.0, 20.0] {
            let spec = AttnSpec {
                head_dim: 128,
                n_q_heads: 8,
                n_kv_heads: 2,
            };
            let q: Vec<f32> = (0..1024)
                .map(|i| (i as f32 * 0.17).sin() * amplitude)
                .collect();
            // The ROCm cache stores f16: round so the f32 reference sees the same values.
            let f16 = |x: f32| half::f16::from_f32(x).to_f32();
            let k: Vec<f32> = (0..len * 256)
                .map(|i| f16((i as f32 * 0.13 + 0.5).cos()))
                .collect();
            let v: Vec<f32> = (0..len * 256)
                .map(|i| f16((i as f32 * 0.07 - 0.4).sin()))
                .collect();
            let cpu_q = CpuBuf(bytemuck::cast_slice(&q).to_vec());
            let cpu_k = CpuBuf(bytemuck::cast_slice(&k).to_vec());
            let cpu_v = CpuBuf(bytemuck::cast_slice(&v).to_vec());
            let mut cpu_o = CpuBackend.alloc(1024 * 4, Pool::Scratch).unwrap();
            CpuBackend
                .attn(
                    &cpu_q,
                    &KvView {
                        k: &cpu_k,
                        v: &cpu_v,
                        len,
                    },
                    &spec,
                    &mut cpu_o,
                )
                .unwrap();
            let upload = |values: &[f32]| {
                let mut buf = hip.alloc(size_of_val(values), Pool::Scratch).unwrap();
                hip.upload(&mut buf, 0, bytemuck::cast_slice(values))
                    .unwrap();
                buf
            };
            let (hq, hk, hv) = (upload(&q), upload_f16_kv(&hip, &k), upload_f16_kv(&hip, &v));
            let mut ho = upload(&vec![-123.0; 1024 + 8]);
            hip.attn(
                &hq,
                &KvView {
                    k: &hk,
                    v: &hv,
                    len,
                },
                &spec,
                &mut ho,
            )
            .unwrap();
            let mut got = vec![0.0f32; 1032];
            hip.download(&ho, 0, bytemuck::cast_slice_mut(&mut got))
                .unwrap();
            let want: &[f32] = bytemuck::cast_slice(&cpu_o.0);
            for (i, (&a, &b)) in got.iter().zip(want).enumerate() {
                assert!(
                    (a - b).abs() < 1e-4,
                    "len={len} amplitude={amplitude} i={i}: {a} != {b}"
                );
            }
            assert_eq!(&got[1024..], &[-123.0; 8]);
        }
    }
}
