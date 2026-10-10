//! Opt-in real 512-token-window and near-2048-context reference gates.
use super::*;
use crate::{backend::cpu::CpuBackend, sampler::greedy};
use serde::Deserialize;

#[derive(Deserialize)]
struct Point {
    length: usize,
    logprobs: Vec<(usize, f64)>,
    output: Vec<u32>,
}

#[derive(Deserialize)]
#[expect(clippy::struct_field_names, reason = "mirrors the fixture JSON keys")]
struct Reference {
    architecture: String,
    model_sha256: String,
    reference: String,
    context: usize,
    microbatch: usize,
    kv: String,
    input: Vec<u32>,
    points: Vec<Point>,
}

fn reference(gemma: bool) -> Reference {
    serde_json::from_str(if gemma {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/fixtures/model/gemma4-long.json"))
    } else {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/fixtures/model/tinyllama-long.json"))
    }).unwrap()
}

fn close(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    let drift = a.iter().zip(b).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(a.iter().chain(b).all(|v| v.is_finite()) && drift < 0.005,
        "prefill/decode maximum logit drift {drift}");
}

fn logprobs(logits: &[f32], point: &Point) {
    assert!(logits.iter().all(|v| v.is_finite()));
    let max = f64::from(logits.iter().copied().fold(f32::NEG_INFINITY, f32::max));
    let log_z = max + logits.iter().map(|&v| (f64::from(v) - max).exp()).sum::<f64>().ln();
    for &(token, expected) in &point.logprobs {
        let got = f64::from(logits[token]) - log_z;
        assert!((got - expected).abs() < 0.01,
            "prefix {} reference logprob token {token}: {got} != {expected}", point.length);
    }
    assert_eq!(greedy(logits), point.output[0], "prefix {}", point.length);
}

#[test]
fn long_reference_fixture_invariants() {
    for gemma in [false, true] {
        let r = reference(gemma);
        assert_eq!(r.reference, "552f18f912a32ea86edf82e2b76431cb7131538d");
        assert_eq!((r.context, r.microbatch, &*r.kv), (2048, 1, "f32"));
        assert_eq!(r.architecture, if gemma { "gemma4" } else { "tinyllama" });
        assert_eq!(r.model_sha256, if gemma {
            "740185b21d22ceb83a11c3aa62ad5842ef32c70f6096d756bbee85a1e4ec34b8"
        } else {
            "9fecc3b3cd76bba89d504f29b616eedf7da85b96540e490ca5824d3f7d2776a0"
        });
        assert_eq!(r.input[0], if gemma { 2 } else { 1 });
        assert_eq!(r.points.iter().map(|p| p.length).collect::<Vec<_>>(),
            if gemma { vec![511, 512, 513, 528] } else { vec![2036] });
        let last = r.points.last().unwrap();
        assert_eq!(r.input.len(), last.length);
        assert!(last.length + last.output.len() <= r.context);
        for p in r.points {
            assert!(!p.output.is_empty());
            assert_eq!(p.logprobs.len(), 5);
            assert!(p.logprobs.iter().all(|(_, v)| v.is_finite()));
        }
    }
}

fn check<B: Backend>(backend: &B, gemma: bool) {
    check_execution(backend, gemma, false);
}

fn check_execution<B: Backend>(backend: &B, gemma: bool, executor: bool) {
    let r = reference(gemma);
    let variable = if gemma { "MDMA_TEST_GEMMA4_GGUF" } else { "MDMA_TEST_TINYLLAMA_GGUF" };
    let path = std::env::var(variable).unwrap_or_else(|_| panic!("set {variable} to the pinned checkpoint"));
    let mut gguf = GgufModel::open(path).unwrap();
    assert_eq!(gguf.metadata_str("general.architecture"), Some(if gemma { "gemma4" } else { "llama" }));
    let model = Model::load(backend, &mut gguf).unwrap();
    drop(gguf);
    let final_point = r.points.last().unwrap();
    let capacity = r.input.len() + final_point.output.len();
    let mut kv = model.new_kv_cache(backend, capacity).unwrap();
    assert_eq!(kv.capacity(), capacity);
    let mut decode = model.alloc_workspace(backend, 1).unwrap();

    // Invalid inputs must leave the prefix empty and reusable.
    {
        let mut decoder = checkpoint_tests::test_decoder(backend, &model, &mut kv, &mut decode, executor);
        assert!(decoder.forward(u32::MAX, 0).is_err());
        assert!(decoder.forward(r.input[0], 1).is_err());
    }
    assert_eq!(kv.len(), 0);
    let mut sequential = Vec::new();
    let mut point_index = 0;
    {
        let mut decoder = checkpoint_tests::test_decoder(backend, &model, &mut kv, &mut decode, executor);
        for (pos, &token) in r.input.iter().enumerate() {
            sequential = decoder.forward(token, pos as u32).unwrap();
            if pos + 1 == r.points[point_index].length {
                logprobs(&sequential, &r.points[point_index]);
                point_index += 1;
            }
        }
    }
    let mut batched_kv = model.new_kv_cache(backend, capacity).unwrap();
    let mut prefill = model.alloc_workspace(backend, r.input.len()).unwrap();
    let mut batched = model.prefill(backend, &mut batched_kv, &mut prefill, &r.input, 0).unwrap();
    close(&batched, &sequential);
    logprobs(&batched, final_point);
    drop(prefill);

    // Incremental prefill checks boundary logits without resetting the KV.
    let mut chunked_kv = model.new_kv_cache(backend, capacity).unwrap();
    let mut start = 0;
    for point in &r.points {
        let tokens = &r.input[start..point.length];
        let mut ws = model.alloc_workspace(backend, tokens.len()).unwrap();
        let logits = model.prefill(backend, &mut chunked_kv, &mut ws, tokens, start as u32).unwrap();
        logprobs(&logits, point);
        if point.length == r.input.len() { close(&batched, &logits); }
        start = point.length;
    }
    drop(chunked_kv);
    for (i, &expected) in final_point.output.iter().enumerate() {
        assert_eq!(greedy(&batched), expected, "batched continuation {i}");
        assert_eq!(greedy(&sequential), expected, "sequential continuation {i}");
        // Ignore EOS only here, matching this fixed-length reference.
        let pos = (r.input.len() + i) as u32;
        batched = checkpoint_tests::test_decoder(backend, &model, &mut batched_kv, &mut decode, executor)
            .forward(expected, pos).unwrap();
        sequential = checkpoint_tests::test_decoder(backend, &model, &mut kv, &mut decode, executor)
            .forward(expected, pos).unwrap();
        close(&batched, &sequential);
    }
    assert_eq!(kv.len(), capacity);
    assert!(checkpoint_tests::test_decoder(backend, &model, &mut kv, &mut decode, executor)
        .forward(r.input[0], capacity as u32).is_err());
    assert_eq!(kv.len(), capacity);
}

#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; near-2048-context checkpoint"]
fn tinyllama_cpu_long_context() { check(&CpuBackend, false); }

#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; real 512-token-window checkpoint"]
fn gemma4_cpu_long_context() { check(&CpuBackend, true); }

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; near-2048-context checkpoint"]
fn tinyllama_metal_long_context() { check(&crate::backend::metal::MetalBackend::new().unwrap(), false); }

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; real 512-token-window checkpoint"]
fn gemma4_metal_long_context() { check(&crate::backend::metal::MetalBackend::new().unwrap(), true); }

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF; batched executor near-2048-context checkpoint"]
fn tinyllama_metal_executor_long_context() {
    check_execution(&crate::backend::metal::MetalBackend::new().unwrap(), false, true);
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF; batched executor 512-token-window checkpoint"]
fn gemma4_metal_executor_long_context() {
    check_execution(&crate::backend::metal::MetalBackend::new().unwrap(), true, true);
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and an NVIDIA GPU; f32 activations, near-2048-context checkpoint"]
fn tinyllama_cuda_long_context() { check(&crate::backend::cuda::CudaBackend::new().unwrap().without_int8(), false); }

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and an NVIDIA GPU; f32 activations, graph-replay near-2048-context checkpoint"]
fn tinyllama_cuda_executor_long_context() {
    check_execution(&crate::backend::cuda::CudaBackend::new().unwrap().without_int8(), false, true);
}

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an NVIDIA GPU; real 512-token-window checkpoint"]
fn gemma4_cuda_long_context() { check(&crate::backend::cuda::CudaBackend::new().unwrap(), true); }

#[cfg(feature = "cuda")]
#[test]
#[ignore = "requires MDMA_TEST_GEMMA4_GGUF and an NVIDIA GPU; backend-decoder 512-token-window checkpoint"]
fn gemma4_cuda_executor_long_context() {
    check_execution(&crate::backend::cuda::CudaBackend::new().unwrap(), true, true);
}
