use super::*;
use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::backend::test_support::{bytes, close, f32s};

fn upload(gpu: &VulkanBackend, x: &[f32]) -> VkBuf {
    let mut b = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut b, 0, &bytes(x)).unwrap();
    b
}

#[test]
fn tiled_selection_keeps_decode_and_wide_heads_on_original_kernel() {
    for (dim, queries) in [(32, 4), (33, 7), (64, 8), (96, 9), (128, 17)] {
        assert!(eligible(dim, queries));
    }
    for (dim, queries) in [(8, 8), (64, 1), (128, 3), (129, 8), (256, 64), (512, 64)] {
        assert!(!eligible(dim, queries));
    }
    let gpu = VulkanBackend::new().unwrap().with_tiled_attention(true);
    assert_eq!(gpu.tiled_attention(), gpu.tiled_attention_available());
    gpu.set_tiled_attention(false);
    assert!(!gpu.tiled_attention());
}

#[test]
fn tiled_attention_matches_cpu_for_prefixes_windows_gqa_and_partial_tiles() {
    let gpu = VulkanBackend::new().unwrap().with_tiled_attention(true);
    if !gpu.tiled_attention_available() {
        eprintln!("wave32 tiled attention unavailable");
        return;
    }
    for (dim, queries, prefix) in [
        (32, 4, 0),
        (33, 7, 1),
        (63, 8, 31),
        (64, 9, 37),
        (65, 15, 33),
        (96, 16, 0),
        (97, 17, 129),
        (127, 7, 1025),
        (128, 9, 2049),
    ] {
        let spec = AttnSpec {
            head_dim: dim,
            n_q_heads: 6,
            n_kv_heads: 2,
        };
        let len = prefix + queries;
        let q: Vec<_> = (0..queries * 6 * dim)
            .map(|i| (i % 29) as f32 / 16.0 - 0.875)
            .collect();
        let k: Vec<_> = (0..len * 2 * dim)
            .map(|i| (i % 31) as f32 / 32.0 - 0.5)
            .collect();
        let v: Vec<_> = (0..k.len())
            .map(|i| (i % 23) as f32 / 8.0 - 1.375)
            .collect();
        let (gq, gk, gv) = (upload(&gpu, &q), upload(&gpu, &k), upload(&gpu, &v));
        let (cq, ck, cv) = (CpuBuf(bytes(&q)), CpuBuf(bytes(&k)), CpuBuf(bytes(&v)));
        for (window, scale) in [(0, 0.25), (1, 0.25), (7, -0.75), (33, 0.0), (512, 16.0)] {
            let mut expected = CpuBackend.alloc(q.len() * 4, Pool::Scratch).unwrap();
            CpuBackend
                .attn_windowed(
                    &cq,
                    &KvView {
                        k: &ck,
                        v: &cv,
                        len,
                    },
                    &spec,
                    scale,
                    window,
                    &mut expected,
                )
                .unwrap();
            for tiled in [false, true] {
                gpu.set_tiled_attention(tiled);
                let mut out = upload(&gpu, &vec![12345.0; q.len() + 7]);
                gpu.attn_windowed(
                    &gq,
                    &KvView {
                        k: &gk,
                        v: &gv,
                        len,
                    },
                    &spec,
                    scale,
                    window,
                    &mut out,
                )
                .unwrap();
                let mut got = vec![0; out.len];
                gpu.download(&out, 0, &mut got).unwrap();
                eprintln!(
                    "attention dim={dim}, queries={queries}, prefix={prefix}, window={window}, scale={scale}, tiled={tiled}"
                );
                close(&f32s(&got)[..q.len()], &f32s(&expected.0), 3e-4);
                assert_eq!(&f32s(&got)[q.len()..], &[12345.0; 7]);
            }
        }
    }
}

#[test]
fn tiled_online_softmax_rescales_when_later_chunks_have_larger_scores() {
    let gpu = VulkanBackend::new().unwrap().with_tiled_attention(true);
    if !gpu.tiled_attention_available() {
        return;
    }
    let spec = AttnSpec {
        head_dim: 128,
        n_q_heads: 4,
        n_kv_heads: 2,
    };
    let (queries, len) = (17, 97);
    let q = vec![0.25; queries * 4 * 128];
    let k: Vec<_> = (0..len * 2 * 128)
        .map(|i| (i / (2 * 128)) as f32 * 0.125)
        .collect();
    let v: Vec<_> = (0..k.len())
        .map(|i| (i % 19) as f32 * 0.0625 - 0.5)
        .collect();
    let (gq, gk, gv) = (upload(&gpu, &q), upload(&gpu, &k), upload(&gpu, &v));
    for scale in [1.0, 100.0] {
        let mut expected = CpuBackend.alloc(q.len() * 4, Pool::Scratch).unwrap();
        CpuBackend
            .attn_windowed(
                &CpuBuf(bytes(&q)),
                &KvView {
                    k: &CpuBuf(bytes(&k)),
                    v: &CpuBuf(bytes(&v)),
                    len,
                },
                &spec,
                scale,
                0,
                &mut expected,
            )
            .unwrap();
        let mut out = gpu.alloc(q.len() * 4, Pool::Scratch).unwrap();
        gpu.attn_windowed(
            &gq,
            &KvView {
                k: &gk,
                v: &gv,
                len,
            },
            &spec,
            scale,
            0,
            &mut out,
        )
        .unwrap();
        let mut got = vec![0; out.len];
        gpu.download(&out, 0, &mut got).unwrap();
        close(&f32s(&got), &f32s(&expected.0), 1e-5);
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and wave32 Vulkan hardware"]
fn tiled_prefill_with_fused_norm_matches_llama_cpp_greedy() {
    let gpu = VulkanBackend::new()
        .unwrap()
        .with_tiled_attention(true)
        .with_fused_norm(true);
    assert!(gpu.tiled_attention(), "requires wave32 tiled attention");
    super::super::tests::prefill_greedy(&gpu);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle wave32 Vulkan GPU"]
fn cached_prefix_prefill_matches_original_logits_and_benchmarks_both_paths() {
    use crate::backend::test_support::fixture;
    use crate::gguf::GgufModel;
    use crate::model::Model;
    let gpu = VulkanBackend::new()
        .unwrap()
        .with_int8(false)
        .with_f16_weights(false)
        .with_tiled_attention(true)
        .with_fused_norm(true);
    assert!(
        gpu.tiled_attention_available(),
        "requires wave32 tiled attention"
    );
    let mut gguf = GgufModel::open(fixture()).unwrap();
    let model = Model::load(&gpu, &mut gguf).unwrap();
    let (prefix, queries) = (1024, 64);
    let mut kv = model.new_kv_cache(&gpu, prefix + queries).unwrap();
    let prefix_tokens: Vec<_> = (0..prefix as u32).map(|i| 1 + i * 7919 % 30000).collect();
    let tokens: Vec<_> = (0..queries as u32)
        .map(|i| 1 + (i * 7919 + 13) % 30000)
        .collect();
    let mut prefix_ws = model.alloc_workspace(&gpu, prefix).unwrap();
    model
        .prefill(&gpu, &mut kv, &mut prefix_ws, &prefix_tokens, 0)
        .unwrap();
    drop(prefix_ws);
    let mut ws = model.alloc_workspace(&gpu, queries).unwrap();
    let mut reference: Option<Vec<f32>> = None;
    for (fused, tiled) in [(false, false), (true, false), (false, true), (true, true)] {
        gpu.set_fused_norm(fused);
        gpu.set_tiled_attention(tiled);
        kv.set_len(prefix);
        let logits = model
            .prefill(&gpu, &mut kv, &mut ws, &tokens, prefix as u32)
            .unwrap();
        if let Some(expected) = &reference {
            close(&logits, expected, 1e-3);
            let max = logits
                .iter()
                .zip(expected)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            eprintln!("cached-prefix logits fused={fused}, tiled={tiled}: max abs error {max:.2e}");
        } else {
            reference = Some(logits);
        }
        let mut times = [0.0f64; 3];
        for elapsed in &mut times {
            kv.set_len(prefix);
            let start = std::time::Instant::now();
            let logits = model
                .prefill(&gpu, &mut kv, &mut ws, &tokens, prefix as u32)
                .unwrap();
            *elapsed = start.elapsed().as_secs_f64();
            close(&logits, reference.as_ref().unwrap(), 1e-3);
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "cached-prefix prefill {queries} tokens after {prefix}, fused={fused}, tiled={tiled}: {:.1} tok/s",
            queries as f64 / times[1]
        );
    }
}

#[test]
#[ignore = "requires an idle Vulkan GPU"]
fn bench_tiled_attention() {
    let gpu = VulkanBackend::new().unwrap();
    assert!(
        gpu.tiled_attention_available(),
        "requires wave32 tiled attention"
    );
    for (dim, queries, prefix) in [(64, 64, 0), (128, 64, 0), (128, 64, 1024), (128, 256, 4096)] {
        let spec = AttnSpec {
            head_dim: dim,
            n_q_heads: 32,
            n_kv_heads: 8,
        };
        let len = queries + prefix;
        let q = upload(&gpu, &vec![0.25; queries * 32 * dim]);
        let k = upload(&gpu, &vec![0.5; len * 8 * dim]);
        let v = upload(&gpu, &vec![0.125; len * 8 * dim]);
        let mut out = gpu.alloc(q.len, Pool::Scratch).unwrap();
        for tiled in [false, true] {
            gpu.set_tiled_attention(tiled);
            for _ in 0..3 {
                gpu.attn(&q, &KvView { k: &k, v: &v, len }, &spec, &mut out)
                    .unwrap();
            }
            gpu.sync().unwrap();
            let start = std::time::Instant::now();
            for _ in 0..10 {
                gpu.attn(&q, &KvView { k: &k, v: &v, len }, &spec, &mut out)
                    .unwrap();
            }
            gpu.sync().unwrap();
            eprintln!(
                "attention dim={dim}, queries={queries}, prefix={prefix}, tiled={tiled}: {:.3} ms",
                start.elapsed().as_secs_f64() * 1e3 / 10.0
            );
            let mut got = vec![0; out.len];
            gpu.download(&out, 0, &mut got).unwrap();
            close(&f32s(&got), &vec![0.125; q.len / 4], 1e-5);
        }
    }
}
