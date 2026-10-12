use super::super::{Backend, HipBackend, HipBuf, Pool, QWeight};
use crate::{
    cache::{ModelCache, WeightLayout, layout::pack_q4k},
    gguf::{
        GgmlDType, GgufModel,
        tests::{Fixture, weight},
    },
    model::Model,
};

fn upload(hip: &HipBackend, values: &[f32]) -> HipBuf {
    let mut buf = hip.alloc(values.len() * 4, Pool::Scratch).unwrap();
    hip.upload(&mut buf, 0, bytemuck::cast_slice(values))
        .unwrap();
    buf
}

fn read(hip: &HipBackend, buf: &HipBuf, n: usize) -> Vec<f32> {
    let mut out = vec![0.0; n];
    hip.download(buf, 0, bytemuck::cast_slice_mut(&mut out))
        .unwrap();
    out
}

fn raw_bytes(rows: usize, k: usize, dtype: GgmlDType) -> Vec<u8> {
    let f = Fixture {
        metadata: Default::default(),
        tensors: vec![("w".into(), weight(&[rows, k], dtype, 11))],
    };
    let file = f.write();
    GgufModel::open(file.path())
        .unwrap()
        .read_tensor_bytes("w")
        .unwrap()
}

fn pair(hip: &HipBackend, rows: usize, k: usize) -> (QWeight<HipBuf>, QWeight<HipBuf>) {
    let raw = raw_bytes(rows, k, GgmlDType::Q4K);
    let make = |bytes: &[u8], layout| QWeight {
        dtype: GgmlDType::Q4K,
        shape: vec![rows, k],
        buf: hip.upload_weight(bytes, layout).unwrap(),
    };
    (
        make(&raw, WeightLayout::Gguf),
        make(&pack_q4k(&raw).unwrap(), WeightLayout::RocmQ4kV3),
    )
}

fn close(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    for (i, (&a, &b)) in a.iter().zip(b).enumerate() {
        assert!(
            (a - b).abs() <= 2e-5 + 2e-5 * a.abs(),
            "index {i}: {a} != {b}"
        );
    }
}

#[test]
fn prefill_tail_allocates_the_full_compiled_tile() {
    for prefix in [
        "qmatmul_gemm_wmma_i8_k256_nt",
        "qmatmul_gemm_wmma_i8_k256_packed_nt",
        "qmatmul_gemm_wmma_i8_q6k_k256_nt",
    ] {
        for (nt, tokens) in [(1, 2), (2, 17), (4, 33), (8, 65)] {
            assert_eq!(
                super::super::gemm_token_tile(&format!("{prefix}{nt}"), nt * 16, tokens),
                nt * 16
            );
        }
    }
    assert_eq!(
        super::super::gemm_token_tile("qmatmul_gemm_wmma_i8_k256", 64, 16),
        16
    );
}

#[test]
#[ignore = "requires ROCm hardware; tests prepared decode, all prefill tiles, F32 mode and embedding"]
fn prepared_q4k_matches_raw_kernels_and_masks_tails() {
    for hip in [HipBackend::new().unwrap(), HipBackend::new_f32().unwrap()] {
        for (rows, k) in [(9, 256), (17, 768), (129, 1024)] {
            let (raw, packed) = pair(&hip, rows, k);
            for tokens in [1, 2, 16, 17, 33, 65, 128, 193] {
                let x = upload(
                    &hip,
                    &(0..tokens * k)
                        .map(|i| ((i * 17 % 255) as f32 - 127.0) / 128.0)
                        .collect::<Vec<_>>(),
                );
                let mut a = upload(&hip, &vec![12345.0; rows * tokens + 8]);
                let mut b = upload(&hip, &vec![12345.0; rows * tokens + 8]);
                hip.qmatmul(&raw, &x, &mut a).unwrap();
                hip.qmatmul(&packed, &x, &mut b).unwrap();
                let got = read(&hip, &b, rows * tokens + 8);
                close(&read(&hip, &a, rows * tokens + 8), &got);
                assert!(got[rows * tokens..].iter().all(|&x| x == 12345.0));
            }
            let mut a = upload(&hip, &vec![0.0; k * 2]);
            let mut b = upload(&hip, &vec![0.0; k * 2]);
            hip.embed(&raw, &[0, rows as u32 - 1], &mut a).unwrap();
            hip.embed(&packed, &[0, rows as u32 - 1], &mut b).unwrap();
            assert_eq!(read(&hip, &a, k * 2), read(&hip, &b, k * 2));
        }
    }
}

#[test]
#[ignore = "requires ROCm hardware; validates fused and mixed-layout projection paths"]
fn prepared_fused_gate_and_grouped_qkv_match_raw() {
    let hip = HipBackend::new().unwrap();
    let (raw, packed) = pair(&hip, 9, 768);
    let x = upload(
        &hip,
        &(0..768)
            .map(|i| ((i % 31) as f32 - 15.0) / 32.0)
            .collect::<Vec<_>>(),
    );
    let mut a = upload(&hip, &[0.0; 9]);
    let mut b = upload(&hip, &[0.0; 9]);
    hip.launch_gated_gemv_i8(&raw, &raw, &x, &mut a).unwrap();
    hip.launch_gated_gemv_i8(&packed, &packed, &x, &mut b)
        .unwrap();
    close(&read(&hip, &a, 9), &read(&hip, &b, 9));
    let mut q = upload(&hip, &[0.0; 9]);
    let mut k = upload(&hip, &[0.0; 9]);
    let mut v = upload(&hip, &[0.0; 9]);
    hip.qmatmul_qkv(&packed, &raw, &packed, &x, &mut q, &mut k, &mut v)
        .unwrap();
    hip.qmatmul(&raw, &x, &mut a).unwrap();
    for out in [&q, &k, &v] {
        close(&read(&hip, &a, 9), &read(&hip, out, 9));
    }
    let q6 = QWeight {
        dtype: GgmlDType::Q6K,
        shape: vec![7, 768],
        buf: hip
            .upload_weight(&raw_bytes(7, 768, GgmlDType::Q6K), WeightLayout::Gguf)
            .unwrap(),
    };
    hip.qmatmul_qkv(&packed, &raw, &q6, &x, &mut q, &mut k, &mut v)
        .unwrap();
    for out in [&q, &k] {
        close(&read(&hip, &a, 9), &read(&hip, out, 9));
    }
    hip.qmatmul(&q6, &x, &mut a).unwrap();
    close(&read(&hip, &a, 7), &read(&hip, &v, 7));
}

#[test]
#[ignore = "requires ROCm hardware"]
fn prepared_decode_preserves_rounding_with_nonuniform_scales() {
    let hip = HipBackend::new().unwrap();
    for k in [4096, 14336] {
        let rows = 9;
        let mut bytes: Vec<u8> = (0..rows * k / 256 * 144)
            .map(|i| (i * 73 + i / 144 * 31) as u8)
            .collect();
        for (i, block) in bytes.chunks_exact_mut(144).enumerate() {
            block[..2].copy_from_slice(
                &half::f16::from_f32((1 + i % 11) as f32 * 0.000173).to_le_bytes(),
            );
            block[2..4]
                .copy_from_slice(&half::f16::from_f32((1 + i % 7) as f32 * 0.000319).to_le_bytes());
        }
        let make = |bytes: &[u8], layout| QWeight {
            dtype: GgmlDType::Q4K,
            shape: vec![rows, k],
            buf: hip.upload_weight(bytes, layout).unwrap(),
        };
        let raw = make(&bytes, WeightLayout::Gguf);
        let packed = make(&pack_q4k(&bytes).unwrap(), WeightLayout::RocmQ4kV3);
        let x = upload(
            &hip,
            &(0..k).map(|i| (i as f32 * 0.017).sin()).collect::<Vec<_>>(),
        );
        let mut a = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        let mut b = hip.alloc(rows * 4, Pool::Scratch).unwrap();
        hip.qmatmul(&raw, &x, &mut a).unwrap();
        hip.qmatmul(&packed, &x, &mut b).unwrap();
        assert_eq!(read(&hip, &a, rows), read(&hip, &b, rows), "decode K={k}");
        hip.launch_gated_gemv_i8(&raw, &raw, &x, &mut a).unwrap();
        hip.launch_gated_gemv_i8(&packed, &packed, &x, &mut b)
            .unwrap();
        assert_eq!(
            read(&hip, &a, rows),
            read(&hip, &b, rows),
            "fused gate/up K={k}"
        );
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and ROCm hardware"]
fn prepared_real_decode_matrices_match_raw() {
    let hip = HipBackend::new().unwrap();
    let mut source = GgufModel::open(crate::backend::test_support::fixture()).unwrap();
    let mut differences = 0;
    for name in [
        "blk.0.attn_q.weight",
        "blk.0.attn_k.weight",
        "blk.0.attn_output.weight",
        "blk.0.ffn_gate.weight",
        "blk.0.ffn_up.weight",
    ] {
        let desc = source.tensor_desc(name).unwrap();
        assert_eq!(desc.dtype, GgmlDType::Q4K, "{name}");
        let shape = desc.shape.clone();
        let bytes = source.read_tensor_bytes(name).unwrap();
        let make = |bytes: &[u8], layout| QWeight {
            dtype: GgmlDType::Q4K,
            shape: shape.clone(),
            buf: hip.upload_weight(bytes, layout).unwrap(),
        };
        let raw = make(&bytes, WeightLayout::Gguf);
        let packed = make(&pack_q4k(&bytes).unwrap(), WeightLayout::RocmQ4kV3);
        for scale in [0.125, 1.0] {
            let x = upload(
                &hip,
                &(0..shape[1])
                    .map(|i| (i as f32 * 0.017).sin() * scale)
                    .collect::<Vec<_>>(),
            );
            let mut a = hip.alloc(shape[0] * 4, Pool::Scratch).unwrap();
            let mut b = hip.alloc(shape[0] * 4, Pool::Scratch).unwrap();
            hip.qmatmul(&raw, &x, &mut a).unwrap();
            hip.qmatmul(&packed, &x, &mut b).unwrap();
            let a = read(&hip, &a, shape[0]);
            let b = read(&hip, &b, shape[0]);
            let count = a.iter().zip(&b).filter(|(a, b)| a != b).count();
            let max = a
                .iter()
                .zip(&b)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            eprintln!("{name} scale={scale}: {count} differing rows, max_diff={max}");
            differences += count;
        }
    }
    assert_eq!(
        differences, 0,
        "lossless prepared decode must preserve real-matrix results"
    );
}

#[test]
#[ignore = "requires ROCm hardware; validates disk cache through prefill, eager and graph decode"]
fn cached_model_matches_raw_prefill_and_graph_decode() {
    let hip = HipBackend::new().unwrap();
    let mut fixture = Fixture::new(1, 256).with_tokenizer();
    for (name, tensor) in &mut fixture.tensors {
        if tensor.shape().rank() == 2 {
            *tensor = weight(
                tensor.shape().dims(),
                if name.contains("attn_v") {
                    GgmlDType::Q6K
                } else {
                    GgmlDType::Q4K
                },
                7,
            );
        }
    }
    let file = fixture.write();
    let mut source = GgufModel::open(file.path()).unwrap();
    // Bypass environment cache lookup for the raw baseline.
    let raw = Model::load_reserved(&hip, &mut source, None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cache = ModelCache::new(dir.path());
    cache.prepare(&hip, &mut source, false).unwrap();
    cache.attach(&hip, &mut source).unwrap();
    let packed = Model::load(&hip, &mut source).unwrap();
    let mut raw_kv = raw.new_kv_cache(&hip, 32).unwrap();
    let mut packed_kv = packed.new_kv_cache(&hip, 32).unwrap();
    let mut raw_ws = raw.alloc_workspace(&hip, 3).unwrap();
    let mut packed_ws = packed.alloc_workspace(&hip, 3).unwrap();
    close(
        &raw.prefill(&hip, &mut raw_kv, &mut raw_ws, &[1, 2, 3], 0)
            .unwrap(),
        &packed
            .prefill(&hip, &mut packed_kv, &mut packed_ws, &[1, 2, 3], 0)
            .unwrap(),
    );
    let mut raw_ws = raw.alloc_workspace(&hip, 1).unwrap();
    let mut packed_ws = packed.alloc_workspace(&hip, 1).unwrap();
    let mut decoder = hip
        .decoder(&packed, &mut packed_kv, &mut packed_ws)
        .unwrap();
    assert!(
        decoder.is_graph(),
        "prepared graph test must not silently fall back to eager"
    );
    for pos in 3..8 {
        close(
            &raw.forward(&hip, &mut raw_kv, &mut raw_ws, pos + 1, pos)
                .unwrap(),
            &decoder.forward(pos + 1, pos).unwrap(),
        );
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF, its prepared MDMA_CACHE_DIR entry, and ROCm hardware"]
fn real_cached_model_matches_raw_continuation() {
    use crate::model::decoder::{Decoder, EagerDecoder};
    const STEPS: usize = 64;
    let _guard = super::super::tests::GPU_MODEL_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let hip = HipBackend::new().unwrap();
    let mut source = GgufModel::open(crate::backend::test_support::fixture()).unwrap();
    let tokenizer = crate::tokenizer::Tokenizer::from_gguf(&source).unwrap();
    let raw = Model::load_reserved(&hip, &mut source, None).unwrap();
    assert!(
        ModelCache::from_env()
            .unwrap()
            .attach(&hip, &mut source)
            .unwrap(),
        "prepare the fixture with mdma-cache before running this test",
    );
    let packed = Model::load(&hip, &mut source).unwrap();
    for prompt in [
        "The capital of France is",
        "The history of artificial intelligence began in antiquity, with myths and legends of artificial beings endowed with intelligence or consciousness by master craftsmen.",
    ] {
        let tokens = tokenizer.encode(prompt);
        for graph in [false, true] {
            let mut raw_kv = raw.new_kv_cache(&hip, tokens.len() + STEPS).unwrap();
            let mut packed_kv = packed.new_kv_cache(&hip, tokens.len() + STEPS).unwrap();
            let mut raw_ws = raw.alloc_workspace(&hip, tokens.len()).unwrap();
            let mut packed_ws = packed.alloc_workspace(&hip, tokens.len()).unwrap();
            let mut raw_logits = raw
                .prefill(&hip, &mut raw_kv, &mut raw_ws, &tokens, 0)
                .unwrap();
            let mut packed_logits = packed
                .prefill(&hip, &mut packed_kv, &mut packed_ws, &tokens, 0)
                .unwrap();
            let mut raw_ws = raw.alloc_workspace(&hip, 1).unwrap();
            let mut packed_ws = packed.alloc_workspace(&hip, 1).unwrap();
            let mut decoder: Box<dyn Decoder + '_> = if graph {
                hip.decoder(&packed, &mut packed_kv, &mut packed_ws)
                    .unwrap()
            } else {
                Box::new(EagerDecoder {
                    backend: &hip,
                    model: &packed,
                    kv: &mut packed_kv,
                    ws: &mut packed_ws,
                })
            };
            assert_eq!(
                decoder.is_graph(),
                graph,
                "graph test must not fall back to eager decode"
            );
            for step in 0..STEPS {
                let want = crate::sampler::greedy(&raw_logits);
                let got = crate::sampler::greedy(&packed_logits);
                assert_eq!(
                    got,
                    want,
                    "graph={graph}, prepared continuation diverged at generated token {}",
                    step + 1
                );
                assert_eq!(raw_logits.len(), packed_logits.len());
                for (i, (a, b)) in raw_logits.iter().zip(&packed_logits).enumerate() {
                    assert_eq!(a, b, "graph={graph}, step={step}, logit={i}");
                }
                if step + 1 < STEPS {
                    let pos = (tokens.len() + step) as u32;
                    raw_logits = raw
                        .forward(&hip, &mut raw_kv, &mut raw_ws, want, pos)
                        .unwrap();
                    packed_logits = decoder.forward(want, pos).unwrap();
                }
            }
            eprintln!(
                "prompt_tokens={} graph={graph}: all {STEPS} continuation logits match",
                tokens.len()
            );
        }
    }
}

#[test]
#[ignore = "requires ROCm hardware; report raw/prepared kernel timings, no speedup assertion"]
fn bench_prepared_q4k() {
    let hip = HipBackend::new().unwrap();
    let (raw, packed) = pair(&hip, 4096, 4096);
    for tokens in [1, 16, 64, 128] {
        let x = upload(&hip, &vec![0.25; tokens * 4096]);
        let mut y = hip.alloc(tokens * 4096 * 4, Pool::Scratch).unwrap();
        let mut samples = [Vec::new(), Vec::new()];
        for round in 0..5 {
            // Alternate order to avoid consistently favoring the second, warmer run.
            for index in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                let w = [&raw, &packed][index];
                for _ in 0..20 {
                    hip.qmatmul(w, &x, &mut y).unwrap();
                }
                rocm_rs::hip::device_synchronize().unwrap();
                let start = std::time::Instant::now();
                for _ in 0..200 {
                    hip.qmatmul(w, &x, &mut y).unwrap();
                }
                rocm_rs::hip::device_synchronize().unwrap();
                samples[index].push(start.elapsed() / 200);
            }
        }
        for (name, times) in ["raw", "prepared"].into_iter().zip(&mut samples) {
            times.sort();
            eprintln!(
                "{name} T={tokens}: median {:?}/matmul, range {:?}..{:?}",
                times[2], times[0], times[4]
            );
        }
    }
}
