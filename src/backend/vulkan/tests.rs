// Every kernel against CpuBackend on asymmetric data (the Level Zero
// backend's suite, on the Vulkan device). A transposed index
// or a swapped nibble does not survive these shapes.
use super::VulkanBackend;
use crate::backend::test_support::{generation::{fixture_greedy_continuation, greedy_continuation}, parity};
use crate::backend::test_support::{fixture, bytes, cpu_qmatmul, f32s, find_tensor, qmatmul as gpu_qmatmul, qmatmul_matches_cpu, real_tensor_qmatmul_matches_cpu, real_tensor_rows};
use crate::backend::{AttnSpec, Backend, KvView, Pool, QWeight};
use crate::gguf::{GgmlDType, GgufModel};

fn gpu() -> VulkanBackend {
    VulkanBackend::new().expect("Vulkan GPU")
}

#[test]
fn f16_staging_requires_an_available_pipeline() {
    let gpu = gpu();
    gpu.set_f16_weights(true);
    assert_eq!(gpu.f16_weights(), gpu.f16_weights_available());
    gpu.set_f16_weights(false);
    assert!(!gpu.f16_weights());
}

#[test]
fn qmatmul_matches_cpu_on_asymmetric_f32() {
    parity::asymmetric_matmul(&gpu());
}

#[test]
fn qmatmul_matches_cpu_when_rows_are_not_a_multiple_of_the_group() {
    parity::matmul_row_tail(&gpu());
}

#[test]
fn add_rmsnorm_silu_and_rope_match_cpu() {
    parity::elementwise_norm_rope(&gpu());
}

#[test]
fn binary_dispatch_boundaries_match_cpu() {
    parity::binary_dispatch_tails(&gpu());
}

#[test]
fn attn_matches_cpu_with_a_cached_prefix() {
    parity::attention_f32_prefix(&gpu());
}

#[test]
fn embed_matches_cpu_on_f32_table() {
    parity::embed_f32(&gpu());
}

fn qmatmul_matches_cpu_on_real_tensor(dtype: GgmlDType) {
    let gpu = gpu();
    real_tensor_qmatmul_matches_cpu(&gpu, dtype, 1e-3);
    // Several tokens take the batched kernel; 67 covers a full 64-token
    // tile and a partial one.
    let (raw, shape) = real_tensor_rows(dtype, 32);
    let n_in = shape[1];
    let xt: Vec<f32> = (0..67 * n_in)
        .map(|i| (((i * 31 + i / n_in * 7) % 255) as f32 - 127.0) / 128.0)
        .collect();
    qmatmul_matches_cpu(&gpu, dtype, &raw, &shape, &xt, 1e-3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn qmatmul_matches_cpu_on_real_q4k_rows() {
    qmatmul_matches_cpu_on_real_tensor(GgmlDType::Q4K);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn qmatmul_matches_cpu_on_real_q6k_rows() {
    qmatmul_matches_cpu_on_real_tensor(GgmlDType::Q6K);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn greedy_continuation_matches_llama_cpp() {
    fixture_greedy_continuation(&gpu());
}

/// Decode throughput on this GPU: the greedy test's prompt, then 16 steps.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle Vulkan GPU"]
fn bench_decode() {
    use crate::model::Model;
    use crate::sampler::greedy;
    let backend = gpu();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let mut kv = model.new_kv_cache(&backend, 64).expect("kv");
    let mut ws = model.alloc_workspace(&backend, 1).expect("workspace");
    let mut pos = 0u32;
    let mut logits = Vec::new();
    for &tok in &[1u32, 918, 6872, 446, 9760, 505] {
        logits = model
            .forward(&backend, &mut kv, &mut ws, tok, pos)
            .expect("forward");
        pos += 1;
    }
    let steps = 16u32;
    let start = std::time::Instant::now();
    for _ in 0..steps {
        let next = greedy(&logits);
        logits = model
            .forward(&backend, &mut kv, &mut ws, next, pos)
            .expect("forward");
        pos += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "vulkan decode: {steps} steps in {secs:.2}s, {:.2} tok/s",
        f64::from(steps) / secs
    );
}

/// The 7B decode matvec shapes, each timed alone: the bandwidth bound is
/// what a decode step is made of.
#[test]
#[ignore]
fn bench_qmatmul() {
    let gpu = gpu();
    for (dtype, n_out, n_in) in [
        (GgmlDType::Q4K, 4096usize, 4096usize),
        (GgmlDType::Q4K, 11008, 4096),
        (GgmlDType::Q4K, 4096, 11008),
        (GgmlDType::Q6K, 4096, 4096),
        (GgmlDType::Q6K, 4096, 11008),
        (GgmlDType::Q6K, 32000, 4096),
    ] {
        let bb = crate::backend::dtype_code::block_bytes(dtype) as usize;
        let w: Vec<u8> = (0..n_out * n_in / 256 * bb)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let mut w_buf = gpu.alloc(w.len(), Pool::Weights).unwrap();
        gpu.upload(&mut w_buf, 0, &w).unwrap();
        let w_gpu = QWeight {
            dtype,
            shape: vec![n_out, n_in],
            buf: w_buf,
        };
        let x: Vec<f32> = (0..n_in).map(|i| (i as f32 * 0.001).sin()).collect();
        let mut x_buf = gpu.alloc(n_in * 4, Pool::Scratch).unwrap();
        gpu.upload(&mut x_buf, 0, &bytes(&x)).unwrap();
        let mut y_buf = gpu.alloc(n_out * 4, Pool::Scratch).unwrap();
        let mut out = vec![0u8; 4];
        gpu.qmatmul(&w_gpu, &x_buf, &mut y_buf).unwrap();
        gpu.download(&y_buf, 0, &mut out).unwrap();
        let iters = 50;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            gpu.qmatmul(&w_gpu, &x_buf, &mut y_buf).unwrap();
        }
        gpu.download(&y_buf, 0, &mut out).unwrap();
        let us = start.elapsed().as_secs_f64() * 1e6 / f64::from(iters);
        eprintln!(
            "vulkan {dtype:?} gemv {n_out}x{n_in}: {us:.0} us, {:.0} GB/s",
            w.len() as f64 / us / 1e3
        );
    }
}

/// The same matvec over 32 distinct weight buffers in rotation (the
/// model's access pattern: nothing stays cached between layers).
#[test]
#[ignore]
fn bench_qmatmul_cold() {
    let gpu = gpu();
    let (n_out, n_in) = (11008usize, 4096usize);
    let n_bufs: usize = std::env::var("MDMA_BENCH_BUFS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let w: Vec<u8> = (0..n_out * n_in / 256 * 144)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let ws: Vec<_> = (0..n_bufs)
        .map(|_| {
            let mut w_buf = gpu.alloc(w.len(), Pool::Weights).unwrap();
            gpu.upload(&mut w_buf, 0, &w).unwrap();
            QWeight {
                dtype: GgmlDType::Q4K,
                shape: vec![n_out, n_in],
                buf: w_buf,
            }
        })
        .collect();
    for card in std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
    {
        let dev = card.path().join("device");
        if let (Ok(v), Ok(g)) = (
            std::fs::read_to_string(dev.join("mem_info_vram_used")),
            std::fs::read_to_string(dev.join("mem_info_gtt_used")),
        ) {
            eprintln!(
                "{}: vram used {} MB, gtt used {} MB",
                card.path().display(),
                v.trim().parse::<u64>().unwrap() >> 20,
                g.trim().parse::<u64>().unwrap() >> 20
            );
        }
    }
    let x: Vec<f32> = (0..n_in).map(|i| (i as f32 * 0.001).sin()).collect();
    let mut x_buf = gpu.alloc(n_in * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut x_buf, 0, &bytes(&x)).unwrap();
    let mut y_buf = gpu.alloc(n_out * 4, Pool::Scratch).unwrap();
    let mut out = vec![0u8; 4];
    for w in &ws {
        gpu.qmatmul(w, &x_buf, &mut y_buf).unwrap();
    }
    gpu.download(&y_buf, 0, &mut out).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..4 {
        for w in &ws {
            gpu.qmatmul(w, &x_buf, &mut y_buf).unwrap();
        }
    }
    gpu.download(&y_buf, 0, &mut out).unwrap();
    let us = start.elapsed().as_secs_f64() * 1e6 / (4 * n_bufs) as f64;
    let mb = n_bufs * w.len() >> 20;
    eprintln!(
        "vulkan Q4K gemv {n_out}x{n_in} cold over {n_bufs} bufs ({mb} MB): {us:.0} us, {:.0} GB/s",
        w.len() as f64 / us / 1e3
    );
}

/// Prefill throughput: a 64-token prompt through Model::prefill, which
/// is where the batched matmul (gemm_q) earns its keep.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and an idle Vulkan GPU"]
fn bench_prefill() {
    use crate::model::Model;
    let backend = gpu();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let n = 64usize;
    let tokens: Vec<u32> = (0..n as u32).map(|i| 1 + (i * 7919) % 30000).collect();
    let mut kv = model.new_kv_cache(&backend, 128).expect("kv");
    let mut ws = model.alloc_workspace(&backend, n).expect("workspace");
    model.prefill(&backend, &mut kv, &mut ws, &tokens, 0).expect("prefill");
    let mut kv = model.new_kv_cache(&backend, 128).expect("kv");
    let start = std::time::Instant::now();
    model.prefill(&backend, &mut kv, &mut ws, &tokens, 0).expect("prefill");
    let secs = start.elapsed().as_secs_f64();
    eprintln!("vulkan prefill: {n} tokens in {secs:.2}s, {:.1} tok/s", n as f64 / secs);
}

/// A buffer dropped after an op was recorded but before anything forced
/// a submission must stay alive until that submission has run. The
/// freed memory is re-allocated and overwritten in between, so an early
/// destroy would (likely) read the overwrite.
#[test]
fn buffers_dropped_before_submission_stay_alive() {
    let gpu = gpu();
    let n = 1 << 16;
    let a_v: Vec<f32> = (0..n).map(|i| i as f32).collect();
    let b_v: Vec<f32> = (0..n).map(|i| 0.5 - i as f32).collect();
    let mut a = gpu.alloc(n * 4, Pool::Scratch).unwrap();
    let mut b = gpu.alloc(n * 4, Pool::Scratch).unwrap();
    let mut y = gpu.alloc(n * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut a, 0, &bytes(&a_v)).unwrap();
    gpu.upload(&mut b, 0, &bytes(&b_v)).unwrap();
    gpu.add(&a, &b, &mut y).unwrap();
    drop(a);
    drop(b);
    let junk = vec![7.0f32; n];
    let mut others: Vec<_> = (0..4).map(|_| gpu.alloc(n * 4, Pool::Scratch).unwrap()).collect();
    for o in &mut others {
        // Each upload submits whatever is pending first; the add must
        // still see a and b.
        gpu.upload(o, 0, &bytes(&junk)).unwrap();
    }
    let mut out = vec![0u8; n * 4];
    gpu.download(&y, 0, &mut out).unwrap();
    let got = f32s(&out);
    assert!(got.iter().all(|v| *v == 0.5), "add read overwritten inputs: {:?}", &got[..4]);
}

/// A buffer may outlive the backend that made it.
#[test]
fn buffer_outlives_backend() {
    let buf = {
        let gpu = gpu();
        let mut b = gpu.alloc(64, Pool::Scratch).unwrap();
        gpu.upload(&mut b, 0, &[1u8; 64]).unwrap();
        b
    };
    drop(buf);
}

/// Payloads that end mid-word: one Q6_K block (210 bytes) and nine f16
/// weights (18 bytes). The kernels read whole words.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn qmatmul_matches_cpu_on_odd_sized_payloads() {
    let (name, _) = find_tensor(fixture(), GgmlDType::Q6K);
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let raw = gguf.read_tensor_bytes(&name).expect("read tensor");
    let x: Vec<f32> = (0..256).map(|i| ((i * 13 % 101) as f32 - 50.0) / 64.0).collect();
    qmatmul_matches_cpu(&gpu(), GgmlDType::Q6K, &raw[..210], &[1, 256], &x, 1e-3);

    let w: Vec<half::f16> = (0..9).map(|i| half::f16::from_f32(i as f32 * 0.31 - 1.2)).collect();
    let x = vec![1.0f32, -2.0, 0.5];
    qmatmul_matches_cpu(&gpu(), GgmlDType::F16, &bytes(&w), &[3, 3], &x, 1e-2);
}

/// Empty shapes are errors, not dispatches of a clamped 1x1x1 grid.
#[test]
fn empty_shapes_are_rejected() {
    let gpu = gpu();
    let w4 = gpu.alloc(16, Pool::Weights).unwrap();
    let empty = gpu.alloc(0, Pool::Scratch).unwrap();
    let mut out = gpu.alloc(0, Pool::Scratch).unwrap();
    assert!(gpu.rmsnorm(&w4, &empty, &mut out, 1e-5).is_err());
    let wq = QWeight { dtype: GgmlDType::F32, shape: vec![4, 4], buf: gpu.alloc(64, Pool::Weights).unwrap() };
    assert!(gpu.qmatmul(&wq, &empty, &mut out).is_err());
    let spec = AttnSpec { head_dim: 4, n_q_heads: 1, n_kv_heads: 1 };
    let k = gpu.alloc(64, Pool::Kv).unwrap();
    let v = gpu.alloc(64, Pool::Kv).unwrap();
    assert!(gpu.attn(&empty, &KvView { k: &k, v: &v, len: 1 }, &spec, &mut out).is_err());
    assert!(gpu.embed(&wq, &[], &mut out).is_err());
}

/// The batched matmul path, gated the way decode is: the prompt goes in
/// as one prefill, then the same greedy continuation must come out.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn prefill_then_greedy_matches_llama_cpp() {
    prefill_greedy(&gpu());
}

pub(super) fn prefill_greedy(backend: &VulkanBackend) {
    use crate::model::Model;
    const PROMPT: &[u32] = &[1, 918, 6872, 446, 9760, 505];
    const EXPECTED: &[u32] = &[12247, 281, 12247, 505, 395];
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(backend, &mut gguf).expect("load model");
    let mut kv = model.new_kv_cache(backend, 64).expect("kv");
    let mut ws_p = model.alloc_workspace(backend, PROMPT.len()).expect("prefill workspace");
    let logits = model.prefill(backend, &mut kv, &mut ws_p, PROMPT, 0).expect("prefill");
    let mut ws = model.alloc_workspace(backend, 1).expect("workspace");
    let pos = PROMPT.len() as u32;
    let generated = greedy_continuation(backend, &model, &mut kv, &mut ws, logits, pos, EXPECTED.len());
    assert_eq!(generated, EXPECTED);
}

/// The f16-staged prefill tile against the CPU reference on real rows:
/// reports the worst absolute error over 67 tokens and how many outputs
/// miss the 1e-3 gate. Not asserted at 1e-3: it is an experiment.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn f16_staged_prefill_error_on_real_rows() {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K] {
        let (name, shape) = find_tensor(fixture(), dtype);
        let &[_, n_in] = shape.as_slice() else { panic!("rank {shape:?}") };
        let n_out = 32;
        let mut gguf = GgufModel::open(fixture()).expect("open gguf");
        let raw = gguf.read_tensor_bytes(&name).expect("read tensor");
        let row_bytes = raw.len() / shape[0];
        let raw = raw[..n_out * row_bytes].to_vec();
        let n_tokens = 67;
        let x: Vec<f32> = (0..n_tokens * n_in)
            .map(|i| (((i * 31 + i / n_in * 7) % 255) as f32 - 127.0) / 128.0)
            .collect();
        let want = cpu_qmatmul(dtype, &raw, &[n_out, n_in], &x);
        let got = gpu_qmatmul(&gpu().with_f16_weights(true), dtype, &raw, &[n_out, n_in], &x);
        let (mut worst, mut worst_rel, mut over) = (0f32, 0f32, 0usize);
        for (g, w) in got.iter().zip(&want) {
            let e = (g - w).abs();
            worst = worst.max(e);
            worst_rel = worst_rel.max(e / w.abs().max(1e-6));
            if e >= 1e-3 {
                over += 1;
            }
        }
        eprintln!(
            "f16-staged {dtype:?} {n_out}x{n_in} x {n_tokens} tokens: max abs err {worst:.2e}, max rel {worst_rel:.2e}, {over}/{} outputs over 1e-3",
            got.len()
        );
        // Sanity: it is still the right matmul.
        assert!(worst < 0.5, "f16 path is broken, not merely imprecise: {worst}");
    }
}

/// One prefill matmul of the 7B FFN shape on 64 tokens, timed alone:
/// the batched kernel's rate without the rest of the model.
#[test]
#[ignore]
fn bench_gemm() {
    let gpu = gpu().with_f16_weights(std::env::var_os("MDMA_VK_F16_WEIGHTS").is_some_and(|v| v == "1"));
    // MDMA_BENCH_Q6K=1: the Q6_K down-projection shape instead.
    let q6 = std::env::var_os("MDMA_BENCH_Q6K").is_some_and(|v| v == "1");
    let (dtype, n_out, n_in, n_tokens) = if q6 { (GgmlDType::Q6K, 4096usize, 14336usize, 64usize) } else { (GgmlDType::Q4K, 14336, 4096, 64) };
    let bb = crate::backend::dtype_code::block_bytes(dtype) as usize;
    let w: Vec<u8> = (0..n_out * n_in / 256 * bb).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let mut w_buf = gpu.alloc(w.len(), Pool::Weights).unwrap();
    gpu.upload(&mut w_buf, 0, &w).unwrap();
    let w_gpu = QWeight { dtype, shape: vec![n_out, n_in], buf: w_buf };
    let x: Vec<f32> = (0..n_tokens * n_in).map(|i| (i as f32 * 0.001).sin()).collect();
    let mut x_buf = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut x_buf, 0, &bytes(&x)).unwrap();
    let mut y_buf = gpu.alloc(n_tokens * n_out * 4, Pool::Scratch).unwrap();
    let mut out = vec![0u8; 4];
    gpu.qmatmul(&w_gpu, &x_buf, &mut y_buf).unwrap();
    gpu.download(&y_buf, 0, &mut out).unwrap();
    let iters = 10;
    let start = std::time::Instant::now();
    for _ in 0..iters {
        gpu.qmatmul(&w_gpu, &x_buf, &mut y_buf).unwrap();
    }
    gpu.download(&y_buf, 0, &mut out).unwrap();
    let ms = start.elapsed().as_secs_f64() * 1e3 / f64::from(iters);
    let gmac = (n_out * n_in * n_tokens) as f64 / ms / 1e6;
    eprintln!("vulkan gemm {dtype:?} {n_out}x{n_in} x {n_tokens}: {ms:.2} ms, {gmac:.0} GMAC/s");
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn f16_prefill_logits_vs_f32() {
    use crate::model::Model;
    let n = 128usize;
    let tokens: Vec<u32> = (0..n as u32).map(|i| 1 + (i * 7919 + 13) % 30000).collect();
    let mut logits = Vec::new();
    for f16 in [false, true] {
        let backend = gpu().with_f16_weights(f16);
        let mut gguf = GgufModel::open(fixture()).expect("open gguf");
        let model = Model::load(&backend, &mut gguf).expect("load model");
        let mut kv = model.new_kv_cache(&backend, 256).expect("kv");
        let mut ws = model.alloc_workspace(&backend, n).expect("workspace");
        logits.push(model.prefill(&backend, &mut kv, &mut ws, &tokens, 0).expect("prefill"));
    }
    let (a, b) = (&logits[0], &logits[1]);
    let argmax = |v: &[f32]| v.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1)).unwrap().0;
    let top = |v: &[f32], k: usize| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&i, &j| v[j].total_cmp(&v[i]));
        idx[..k].to_vec()
    };
    let max_diff = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
    let scale = a.iter().map(|x| x.abs()).fold(0f32, f32::max);
    let t5a = top(a, 5);
    let t5b = top(b, 5);
    let overlap = t5a.iter().filter(|i| t5b.contains(i)).count();
    eprintln!(
        "f16 vs f32 prefill logits over {n} tokens: argmax {} vs {} ({}), max |diff| {max_diff:.3e} on logits up to {scale:.2}, top-5 overlap {overlap}/5, top-5 f32 {t5a:?} f16 {t5b:?}",
        argmax(a), argmax(b), if argmax(a) == argmax(b) { "same" } else { "DIFFERENT" }
    );
    assert_eq!(argmax(a), argmax(b));
}

/// The load-time decision on this GPU and model, printed.
#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn f16_calibration_decision() {
    use crate::model::Model;
    let backend = gpu();
    let mut gguf = GgufModel::open(fixture()).expect("open gguf");
    let model = Model::load(&backend, &mut gguf).expect("load model");
    let c = backend.calibrate_f16(&model).expect("calibrate");
    eprintln!("vulkan calibration: {}", c.summary());
    assert_eq!(c.f16, backend.f16_weights());
}
