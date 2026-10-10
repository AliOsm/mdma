use super::super::{Backend, HipBackend, HipBuf, Pool, QWeight};
use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::gguf::{GgmlDType, GgufModel};
use half::f16;
use std::time::{Duration, Instant};

use crate::backend::test_support::fixture;

#[test]
fn scratch_reserve_reuses_capacity_and_invalidates_only_on_growth() {
    let hip = HipBackend::new().unwrap();
    let _guard = hip.lock.lock().unwrap();
    let mut scratch = hip.q8_scratch.lock().unwrap();
    scratch.reserve(0, &hip.buffers).unwrap();
    assert!(scratch.mem.is_none());
    let (base, _) = hip.q8_row_ptrs(&mut scratch, 256).unwrap();
    let cap = scratch.cap;
    let key = Some((123, 1, 256));
    scratch.cached = key;
    for need in [0, cap - 1, cap] {
        scratch.reserve(need, &hip.buffers).unwrap();
        assert_eq!(scratch.mem.as_ref().unwrap().as_ptr(), base);
        assert_eq!(scratch.cap, cap);
        assert_eq!(scratch.cached, key);
    }
    scratch.reserve(cap + 256, &hip.buffers).unwrap();
    assert_eq!(scratch.cap, cap + 256);
    assert_eq!(scratch.cached, None);
    scratch.cached = key;
    hip.q8_row_ptrs(&mut scratch, 1024).unwrap();
    assert_eq!(scratch.cap, 1024 + 32 * size_of::<[f32; 2]>());
    assert_eq!(scratch.cached, None);
}

fn upload(hip: &HipBackend, bytes: &[u8]) -> HipBuf {
    let mut buf = hip.alloc(bytes.len(), Pool::Scratch).unwrap();
    hip.upload(&mut buf, 0, bytes).unwrap();
    buf
}

fn download(hip: &HipBackend, buf: &HipBuf, n: usize) -> Vec<f32> {
    let mut out = vec![0f32; n];
    hip.download(buf, 0, bytemuck::cast_slice_mut(&mut out)).unwrap();
    out
}

fn weights(dtype: GgmlDType, rows: usize, k: usize) -> Vec<u8> {
    let bytes = if dtype == GgmlDType::Q4K { 144 } else { 210 };
    let mut raw = vec![0; rows * (k / 256) * bytes];
    for (b, block) in raw.chunks_exact_mut(bytes).enumerate() {
        for (i, v) in block.iter_mut().enumerate() {
            *v = (i * 73 + b * 31 + 19) as u8;
        }
        let d = f16::from_f32((1 + b % 3) as f32 / 1024.0).to_le_bytes();
        if dtype == GgmlDType::Q4K {
            block[..2].copy_from_slice(&d);
            block[2..4].copy_from_slice(&f16::from_f32(1.0 / 512.0).to_le_bytes());
        } else {
            block[208..210].copy_from_slice(&d);
            block[192] = 128; // -128 and +127 sub-scales, with nonzero high bits.
            block[193] = 127;
        }
    }
    raw
}

fn requantize(x: &[f32]) -> Vec<f32> {
    x.chunks_exact(32)
        .flat_map(|block| {
            let amax = block.iter().fold(0f32, |m, v| m.max(v.abs()));
            let d = f16::from_f32(amax / 127.0).to_f32();
            let inv = if amax == 0.0 { 0.0 } else { 127.0 / amax };
            block.iter().map(move |v| d * (v * inv).round())
        })
        .collect()
}

fn check_synthetic(dtype: GgmlDType) {
    let hip = HipBackend::new().unwrap();
    for rows in [1, 7, 9, 20] {
        for k in [256, 512, 768] {
            let raw = weights(dtype, rows, k);
            let w = QWeight { dtype, shape: vec![rows, k], buf: upload(&hip, &raw) };
            for kind in 0..3 {
                let x: Vec<f32> = (0..k)
                    .map(|i| match kind {
                        0 => ((i * 17 % 255) as f32 - 127.0) / 32.0,
                        1 => -((i * 7 % 31 + 1) as f32) / 16.0,
                        _ => 0.0,
                    })
                    .collect();
                let x_buf = upload(&hip, bytemuck::cast_slice(&x));
                let mut y = upload(&hip, bytemuck::cast_slice(&vec![123456.25f32; rows + 8]));
                hip.qmatmul(&w, &x_buf, &mut y).unwrap();
                let got = download(&hip, &y, rows + 8);

                let cpu_w = QWeight { dtype, shape: vec![rows, k], buf: CpuBuf(raw.clone()) };
                let x_hat = requantize(&x);
                let mut cpu_y = CpuBuf(vec![0; rows * 4]);
                CpuBackend.qmatmul(&cpu_w, &CpuBuf(bytemuck::cast_slice(&x_hat).to_vec()), &mut cpu_y).unwrap();
                let want: &[f32] = bytemuck::cast_slice(&cpu_y.0);
                for (i, (&g, &w)) in got[..rows].iter().zip(want).enumerate() {
                    assert!((g - w).abs() <= 2e-4 + 2e-5 * w.abs(), "{dtype:?} rows={rows} k={k} input={kind} row={i}: {g} != {w}");
                }
                assert!(got[rows..].iter().all(|&v| v == 123456.25), "partial block wrote past output");
            }
        }
    }
}

#[test]
fn q4k_packed_decode_matches_reference_and_masks_tail() {
    check_synthetic(GgmlDType::Q4K);
}

#[test]
fn q6k_packed_decode_matches_reference_and_masks_tail() {
    check_synthetic(GgmlDType::Q6K);
}

#[test]
fn packed_decode_reuses_q8_across_dtypes_and_invalidates_on_upload() {
    let hip = HipBackend::new().unwrap();
    let (rows, k) = (9, 768);
    let w4 = QWeight { dtype: GgmlDType::Q4K, shape: vec![rows, k], buf: upload(&hip, &weights(GgmlDType::Q4K, rows, k)) };
    let w6 = QWeight { dtype: GgmlDType::Q6K, shape: vec![rows, k], buf: upload(&hip, &weights(GgmlDType::Q6K, rows, k)) };
    let xa: Vec<f32> = (0..k).map(|i| ((i % 37) as f32 - 18.0) * 0.1).collect();
    let mut x = upload(&hip, bytemuck::cast_slice(&xa));
    let mut y = hip.alloc(rows * 4, Pool::Scratch).unwrap();
    hip.launch_gemv_i8(&w4, &x, &mut y).unwrap();
    let key = Some((x.as_device_ptr() as usize, 1, k as i32));
    assert_eq!(hip.q8_scratch.lock().unwrap().cached, key);
    hip.launch_gemv_i8(&w6, &x, &mut y).unwrap();
    let reused = download(&hip, &y, rows);
    assert_eq!(hip.q8_scratch.lock().unwrap().cached, key);
    hip.q8_invalidate();
    hip.launch_gemv_i8(&w6, &x, &mut y).unwrap();
    assert_eq!(reused, download(&hip, &y, rows));

    let xb: Vec<f32> = xa.iter().map(|v| -v * 0.5 + 0.25).collect();
    hip.upload(&mut x, 0, bytemuck::cast_slice(&xb)).unwrap();
    assert_eq!(hip.q8_scratch.lock().unwrap().cached, None);
    hip.launch_gemv_i8(&w6, &x, &mut y).unwrap();
    let rewritten = download(&hip, &y, rows);
    let x2 = upload(&hip, bytemuck::cast_slice(&xb));
    hip.launch_gemv_i8(&w6, &x2, &mut y).unwrap();
    assert_eq!(rewritten, download(&hip, &y, rows));
    assert_ne!(reused, rewritten);
}

#[test]
fn packed_decode_and_prefill_share_growing_scratch() {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K] {
        let hip = HipBackend::new().unwrap();
        let (rows, k) = (9, 512);
        let raw = weights(dtype, rows, k);
        let w = QWeight { dtype, shape: vec![rows, k], buf: upload(&hip, &raw) };
        let cpu_w = QWeight { dtype, shape: vec![rows, k], buf: CpuBuf(raw) };
        // Upload everything first so only matmuls change the cache below.
        let mut cases: Vec<_> = [1, 16, 1, 64, 1, 2, 1]
            .into_iter()
            .enumerate()
            .map(|(phase, tokens)| {
                let x: Vec<f32> = (0..tokens * k).map(|i| ((i * 17 + phase * 31) % 255) as f32 / 32.0 - 4.0).collect();
                let x_buf = upload(&hip, bytemuck::cast_slice(&x));
                let y = hip.alloc(tokens * rows * 4, Pool::Scratch).unwrap();
                (tokens, x, x_buf, y)
            })
            .collect();
        let mut max_tokens = 0;
        let mut previous_allocation = None;
        for (tokens, x, x_buf, y) in &mut cases {
            hip.qmatmul(&w, x_buf, y).unwrap();
            let got = download(&hip, y, *tokens * rows);
            let mut cpu_y = CpuBuf(vec![0; *tokens * rows * 4]);
            CpuBackend.qmatmul(&cpu_w, &CpuBuf(bytemuck::cast_slice(&requantize(x)).to_vec()), &mut cpu_y).unwrap();
            let want: &[f32] = bytemuck::cast_slice(&cpu_y.0);
            for (i, (&g, &w)) in got.iter().zip(want).enumerate() {
                assert!((g - w).abs() <= 2e-4 + 2e-5 * w.abs(), "{dtype:?} T={tokens} index={i}: {g} != {w}");
            }
            max_tokens = max_tokens.max(*tokens);
            let scratch = hip.q8_scratch.lock().unwrap();
            // Prefill may reserve split-K partials after the Q8 metadata.
            assert!(scratch.cap >= max_tokens * (k + k / 32 * size_of::<[f32; 2]>()));
            let allocation = (scratch.mem.as_ref().unwrap().as_ptr(), scratch.cap);
            if let Some((ptr, cap)) = previous_allocation {
                assert!(scratch.cap >= cap);
                if *tokens == 1 {
                    assert_eq!(allocation, (ptr, cap), "decode must reuse the prefill allocation");
                }
            }
            previous_allocation = Some(allocation);
            assert_eq!(scratch.cached, Some((x_buf.as_device_ptr() as usize, *tokens as i32, k as i32)));
        }
    }
}

fn time_launches(mut launch: impl FnMut()) -> Duration {
    launch();
    rocm_rs::hip::device_synchronize().unwrap();
    let start = Instant::now();
    for _ in 0..40 {
        launch();
    }
    rocm_rs::hip::device_synchronize().unwrap();
    start.elapsed() / 40
}

#[test]
fn fused_gate_up_matches_requantized_cpu_and_masks_tail() {
    let hip = HipBackend::new().unwrap();
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K] {
        for rows in [1, 7, 9, 20] {
            for k in [256, 512, 768] {
                let gate_raw = weights(dtype, rows, k);
                let mut up_raw = weights(dtype, rows + 1, k);
                let up_raw = up_raw.split_off(up_raw.len() / (rows + 1));
                let gate = QWeight { dtype, shape: vec![rows, k], buf: upload(&hip, &gate_raw) };
                let up = QWeight { dtype, shape: vec![rows, k], buf: upload(&hip, &up_raw) };
                let cpu_gate = QWeight { dtype, shape: vec![rows, k], buf: CpuBuf(gate_raw) };
                let cpu_up = QWeight { dtype, shape: vec![rows, k], buf: CpuBuf(up_raw) };
                let mut gs = hip.alloc(rows * 4, Pool::Scratch).unwrap();
                let mut us = hip.alloc(rows * 4, Pool::Scratch).unwrap();
                for kind in 0..3 {
                    let x: Vec<f32> = (0..k).map(|i| match kind {
                        0 => ((i * 17 % 255) as f32 - 127.0) / 1024.0,
                        1 => -((i * 7 % 31 + 1) as f32) / 1024.0,
                        _ => 0.0,
                    }).collect();
                    let x_buf = upload(&hip, bytemuck::cast_slice(&x));
                    let mut y = upload(&hip, bytemuck::cast_slice(&vec![123456.25f32; rows + 8]));
                    hip.ffn_gate_up(&gate, &up, &x_buf, &mut gs, &mut us, &mut y).unwrap();
                    let got = download(&hip, &y, rows + 8);
                    let mut cpu_y = CpuBuf(vec![0; rows * 4]);
                    CpuBackend.ffn_gate_up(
                        &cpu_gate, &cpu_up, &CpuBuf(bytemuck::cast_slice(&requantize(&x)).to_vec()),
                        &mut CpuBuf(vec![0; rows * 4]), &mut CpuBuf(vec![0; rows * 4]), &mut cpu_y,
                    ).unwrap();
                    let want: &[f32] = bytemuck::cast_slice(&cpu_y.0);
                    for (i, (&g, &w)) in got[..rows].iter().zip(want).enumerate() {
                        assert!((g - w).abs() <= 4e-4 + 4e-5 * w.abs(), "{dtype:?} rows={rows} k={k} kind={kind} row={i}: {g} != {w}");
                    }
                    assert!(got[rows..].iter().all(|&v| v == 123456.25), "fused output overrun");
                    assert_eq!(hip.q8_scratch.lock().unwrap().cached, Some((x_buf.as_device_ptr() as usize, 1, k as i32)));
                    hip.ffn_gate_up(&gate, &up, &x_buf, &mut gs, &mut us, &mut y).unwrap();
                    assert_eq!(got, download(&hip, &y, rows + 8));
                    hip.qmatmul(&gate, &x_buf, &mut gs).unwrap();
                    hip.qmatmul(&up, &x_buf, &mut us).unwrap();
                    hip.silu_mul(&gs, &us, &mut y).unwrap();
                    for (&g, w) in got[..rows].iter().zip(download(&hip, &y, rows)) {
                        assert!((g - w).abs() <= 2e-5 + 2e-5 * w.abs(), "fused vs unfused: {g} != {w}");
                    }
                }
            }
        }
    }
}

#[test]
fn gate_up_falls_back_for_prefill_float_and_mixed_weights() {
    use GgmlDType::{F16, F32, Q4K, Q6K};
    let hip = HipBackend::new().unwrap();
    let (rows, k) = (9, 256);
    for (gd, ud, tokens) in [(Q4K, Q6K, 1), (Q6K, Q4K, 1), (Q4K, Q4K, 3), (Q6K, Q6K, 16), (F32, F32, 1), (F16, F16, 1), (F32, Q4K, 1)] {
        let make_weight = |dtype, phase| {
            let values: Vec<f32> = (0..rows * k).map(|i| ((i * 19 + phase * 7) % 31) as f32 / 512.0 - 0.03).collect();
            let raw = match dtype {
                F32 => bytemuck::cast_slice(&values).to_vec(),
                F16 => values.iter().flat_map(|v| f16::from_f32(*v).to_le_bytes()).collect(),
                _ => {
                    let mut raw = weights(dtype, rows + phase, k);
                    raw.split_off(phase * (raw.len() / (rows + phase)))
                }
            };
            QWeight { dtype, shape: vec![rows, k], buf: upload(&hip, &raw) }
        };
        let gate = make_weight(gd, 0);
        let mut up = make_weight(ud, 1);
        let x: Vec<f32> = (0..k * tokens).map(|i| ((i * 17 % 255) as f32 - 127.0) / 1024.0).collect();
        let x = upload(&hip, bytemuck::cast_slice(&x));
        let mut gs = hip.alloc(rows * tokens * 4, Pool::Scratch).unwrap();
        let mut us = hip.alloc(rows * tokens * 4, Pool::Scratch).unwrap();
        let mut y = hip.alloc(rows * tokens * 4, Pool::Scratch).unwrap();
        hip.ffn_gate_up(&gate, &up, &x, &mut gs, &mut us, &mut y).unwrap();
        let got = download(&hip, &y, rows * tokens);
        hip.qmatmul(&gate, &x, &mut gs).unwrap();
        hip.qmatmul(&up, &x, &mut us).unwrap();
        hip.silu_mul(&gs, &us, &mut y).unwrap();
        assert_eq!(got, download(&hip, &y, rows * tokens), "{gd:?}/{ud:?} T={tokens}");
        up.shape[0] += 1;
        assert!(hip.ffn_gate_up(&gate, &up, &x, &mut gs, &mut us, &mut y).is_err());
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_fused_gate_up() {
    let hip = HipBackend::new().unwrap();
    let mut gguf = GgufModel::open(fixture()).unwrap();
    for (gate_name, up_name) in [
        ("blk.0.ffn_gate.weight", "blk.0.ffn_up.weight"),
        ("blk.0.ffn_down.weight", "blk.3.ffn_down.weight"),
    ] {
        let mut load = |name| {
            let desc = gguf.tensor_desc(name).unwrap().clone();
            let raw = gguf.read_tensor_bytes(name).unwrap();
            QWeight { dtype: desc.dtype, shape: desc.shape, buf: upload(&hip, &raw) }
        };
        let gate = load(gate_name);
        let up = load(up_name);
        assert_eq!(gate.dtype, up.dtype);
        assert_eq!(gate.shape, up.shape);
        let [rows, k] = gate.shape[..] else { panic!("rank-2 weight") };
        let x: Vec<f32> = (0..k).map(|i| ((i % 13) as f32 - 6.0) * 0.1).collect();
        let x = upload(&hip, bytemuck::cast_slice(&x));
        let mut gs = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        let mut us = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        let mut y = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        hip.qmatmul(&gate, &x, &mut gs).unwrap();
        hip.qmatmul(&up, &x, &mut us).unwrap();
        hip.silu_mul(&gs, &us, &mut y).unwrap();
        let want = download(&hip, &y, rows);
        for trial in 1..=3 {
            let unfused = time_launches(|| {
                hip.q8_invalidate();
                hip.qmatmul(&gate, &x, &mut gs).unwrap();
                hip.qmatmul(&up, &x, &mut us).unwrap();
                hip.silu_mul(&gs, &us, &mut y).unwrap();
            });
            for rows_per_block in [1, 2, 4, 8, 16] {
                let fused = time_launches(|| {
                    hip.q8_invalidate();
                    hip.launch_decode_i8(&gate, Some(&up), &x, &mut y, rows_per_block).unwrap();
                });
                for (g, &w) in download(&hip, &y, rows).iter().zip(&want) {
                    assert!((g - w).abs() <= 2e-5 + 2e-5 * w.abs(), "real fused vs unfused: {g} != {w}");
                }
                eprintln!("FFN dtype={:?} rows={rows} k={k} trial={trial} rows_per_block={rows_per_block} unfused_ns={} fused_ns={}", gate.dtype, unfused.as_nanos(), fused.as_nanos());
            }
        }
    }
}

#[test]
fn q4k_decode_covers_all_six_bit_scales_and_minima() {
    let hip = HipBackend::new().unwrap();
    let (rows, k) = (64, 256);
    let mut raw = weights(GgmlDType::Q4K, rows, k);
    for (row, block) in raw.chunks_exact_mut(144).enumerate() {
        let scales: [u8; 8] = std::array::from_fn(|g| ((row + 11 * g) % 64) as u8);
        let minima: [u8; 8] = std::array::from_fn(|g| ((3 * row + 7 * g) % 64) as u8);
        for g in 0..4 {
            block[4 + g] = scales[g] | ((scales[g + 4] >> 4) << 6);
            block[8 + g] = minima[g] | ((minima[g + 4] >> 4) << 6);
            block[12 + g] = (scales[g + 4] & 15) | ((minima[g + 4] & 15) << 4);
        }
    }
    let w = QWeight { dtype: GgmlDType::Q4K, shape: vec![rows, k], buf: upload(&hip, &raw) };
    let cpu_w = QWeight { dtype: w.dtype, shape: w.shape.clone(), buf: CpuBuf(raw) };
    for group in 0..=8 {
        let x: Vec<f32> = (0..k).map(|i| {
            if group == 8 || i / 32 == group { ((i * 17 % 255) as f32 - 127.0) / 32.0 } else { 0.0 }
        }).collect();
        let mut cpu_y = CpuBuf(vec![0; rows * 4]);
        CpuBackend.qmatmul(&cpu_w, &CpuBuf(bytemuck::cast_slice(&requantize(&x)).to_vec()), &mut cpu_y).unwrap();
        let want: &[f32] = bytemuck::cast_slice(&cpu_y.0);
        let x = upload(&hip, bytemuck::cast_slice(&x));
        let mut y = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        hip.qmatmul(&w, &x, &mut y).unwrap();
        for (row, (&g, &v)) in download(&hip, &y, rows).iter().zip(want).enumerate() {
            assert!((g - v).abs() <= 2e-4 + 2e-5 * v.abs(), "group={group} row={row}: {g} != {v}");
        }
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle ROCm GPU"]
fn bench_packed_decode_shapes() {
    let hip = HipBackend::new().unwrap();
    let mut gguf = GgufModel::open(fixture()).unwrap();
    for name in ["blk.0.attn_q.weight", "blk.0.attn_k.weight", "blk.0.attn_v.weight", "blk.0.ffn_gate.weight", "blk.0.ffn_down.weight", "output.weight"] {
        let desc = gguf.tensor_desc(name).unwrap().clone();
        let [n_out, n_in] = desc.shape[..] else { panic!("rank-2 tensor") };
        let raw = gguf.read_tensor_bytes(name).unwrap();
        let w = QWeight { dtype: desc.dtype, shape: desc.shape, buf: upload(&hip, &raw) };
        let x: Vec<f32> = (0..n_in).map(|i| ((i % 13) as f32 - 6.0) * 0.1).collect();
        let x = upload(&hip, bytemuck::cast_slice(&x));
        let mut y = hip.alloc(n_out * 4, Pool::Scratch).unwrap();
        for trial in 1..=3 {
            let reference = time_launches(|| {
                hip.launch_gemv(w.buf.as_device_ptr(), crate::backend::dtype_code::code(w.dtype).unwrap(), n_out as i32, n_in as i32, x.as_device_ptr(), y.as_device_ptr()).unwrap();
            });
            let uncached = time_launches(|| {
                hip.q8_invalidate();
                hip.launch_gemv_i8(&w, &x, &mut y).unwrap();
            });
            let cached = time_launches(|| hip.launch_gemv_i8(&w, &x, &mut y).unwrap());
            eprintln!(
                "DECODE tensor={name} dtype={:?} rows={n_out} k={n_in} trial={trial} reference_ns={} uncached_ns={} cached_ns={}",
                w.dtype, reference.as_nanos(), uncached.as_nanos(), cached.as_nanos()
            );
        }
    }
}
