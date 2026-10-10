// Every kernel against CpuBackend on asymmetric data. A transposed index
// or a swapped nibble does not survive these shapes.
use super::Level0Backend;
use crate::backend::cpu::CpuBackend;
use crate::backend::test_support::{close, generation::fixture_greedy_continuation, parity, real_tensor_qmatmul_matches_cpu};
use crate::backend::{Backend, Pool};
use crate::gguf::GgmlDType;

fn gpu() -> Level0Backend {
    Level0Backend::new().expect("Level Zero GPU")
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

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Level Zero hardware"]
fn qmatmul_matches_cpu_on_real_q4k_rows() {
    real_tensor_qmatmul_matches_cpu(&gpu(), GgmlDType::Q4K, 1e-3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Level Zero hardware"]
fn qmatmul_matches_cpu_on_real_q6k_rows() {
    real_tensor_qmatmul_matches_cpu(&gpu(), GgmlDType::Q6K, 1e-3);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Level Zero hardware"]
fn greedy_continuation_matches_llama_cpp() {
    fixture_greedy_continuation(&gpu());
}

#[test]
fn byte_ranges_reject_overflow_before_touching_memory() {
    let gpu = gpu();
    let mut src = gpu.alloc(8, Pool::Scratch).unwrap();
    let mut dst = gpu.alloc(8, Pool::Scratch).unwrap();
    gpu.upload(&mut src, 0, &[1; 8]).unwrap();
    gpu.upload(&mut dst, 0, &[2; 8]).unwrap();
    assert!(gpu.upload(&mut dst, usize::MAX, &[3]).is_err());
    assert!(gpu.download(&src, usize::MAX, &mut [0]).is_err());
    assert!(gpu.copy(&src, 1, &mut dst, 0, usize::MAX).is_err());
    assert!(gpu.copy(&src, 0, &mut dst, usize::MAX, 1).is_err());
    gpu.copy(&src, 8, &mut dst, 8, 0).unwrap();
    let mut actual = [0; 8];
    gpu.download(&dst, 0, &mut actual).unwrap();
    assert_eq!(actual, [2; 8]);
}


fn fbuf<B: Backend>(backend: &B, values: &[f32]) -> B::Buf {
    let mut buf = backend.alloc(values.len() * 4, Pool::Scratch).unwrap();
    backend.upload(&mut buf, 0, bytemuck::cast_slice(values)).unwrap();
    buf
}

fn read<B: Backend>(backend: &B, buf: &B::Buf, n: usize) -> Vec<f32> {
    let mut out = vec![0f32; n];
    backend.download(buf, 0, bytemuck::cast_slice_mut(&mut out)).unwrap();
    out
}

/// Batched prefill then decode, against the CPU oracle and against
/// token-at-a-time decode on the same device.
#[test]
fn tiny_model_prefill_matches_cpu_and_sequential_decode() {
    let tokens = [1u32, 7, 13, 19, 23, 29];
    fn run<B: Backend>(backend: &B, tokens: &[u32], batched: bool) -> Vec<Vec<f32>> {
        let model = crate::model::tests::tiny_model(backend);
        let mut kv = model.new_kv_cache(backend, 16).unwrap();
        if batched {
            let mut prefill = model.alloc_workspace(backend, 3).unwrap();
            let mut decode = model.alloc_workspace(backend, 1).unwrap();
            let mut outputs = vec![model.prefill(backend, &mut kv, &mut prefill, &tokens[..3], 0).unwrap()];
            for (i, &token) in tokens[3..].iter().enumerate() {
                outputs.push(model.forward(backend, &mut kv, &mut decode, token, (i + 3) as u32).unwrap());
            }
            outputs
        } else {
            let mut ws = model.alloc_workspace(backend, 1).unwrap();
            let mut logits = Vec::new();
            for (i, &token) in tokens.iter().enumerate() {
                logits = model.forward(backend, &mut kv, &mut ws, token, i as u32).unwrap();
            }
            vec![logits]
        }
    }
    let backend = gpu();
    let batched = run(&backend, &tokens, true);
    let cpu_prefill = run(&CpuBackend, &tokens, true);
    for (got, want) in batched.iter().zip(&cpu_prefill) {
        close(got, want, 2e-4);
        assert_eq!(crate::sampler::greedy(got), crate::sampler::greedy(want));
    }
    close(run(&backend, &tokens, false).last().unwrap(), batched.last().unwrap(), 2e-4);
}

#[test]
fn fused_add_rmsnorm_matches_cpu_and_rejects_bad_shapes() {
    let gpu = gpu();
    let cpu = CpuBackend;
    const WIDTH: usize = 8;
    let tokens = 3usize;
    let n = WIDTH * tokens;
    let a: Vec<f32> = (0..n).map(|i| (i as f32) * 0.3 - 2.0).collect();
    let mut b: Vec<f32> = (0..n).map(|i| (i as f32) * -0.2 + 0.7).collect();
    for i in n - WIDTH..n {
        b[i] = -a[i];
    }
    let w: Vec<f32> = (0..WIDTH).map(|i| 0.5 + (i as f32) * 0.1).collect();
    let (wc, ac, bc) = (fbuf(&cpu, &w), fbuf(&cpu, &a), fbuf(&cpu, &b));
    let (wg, ag, bg) = (fbuf(&gpu, &w), fbuf(&gpu, &a), fbuf(&gpu, &b));
    let (mut sc, mut yc) = (fbuf(&cpu, &vec![123.0; n]), fbuf(&cpu, &vec![456.0; n]));
    let (mut sg, mut yg) = (fbuf(&gpu, &vec![123.0; n]), fbuf(&gpu, &vec![456.0; n]));
    cpu.add_rmsnorm(&wc, &ac, &bc, &mut sc, &mut yc, 1e-5).unwrap();
    gpu.add_rmsnorm(&wg, &ag, &bg, &mut sg, &mut yg, 1e-5).unwrap();
    close(&read(&gpu, &sg, n), &read(&cpu, &sc, n), 1e-4);
    close(&read(&gpu, &yg, n), &read(&cpu, &yc, n), 1e-5);

    let bad = gpu.alloc(3, Pool::Scratch).unwrap();
    let short = fbuf(&gpu, &[1.0; 3]);
    let mut sum = fbuf(&gpu, &[123.0; WIDTH]);
    let mut y = fbuf(&gpu, &[456.0; WIDTH]);
    let weights = fbuf(&gpu, &[1.0; WIDTH]);
    assert!(gpu.add_rmsnorm(&bad, &weights, &weights, &mut sum, &mut y, 1e-5).is_err());
    assert!(gpu.add_rmsnorm(&weights, &short, &weights, &mut sum, &mut y, 1e-5).is_err());
    assert!(gpu.add_rmsnorm(&weights, &weights, &weights, &mut sum, &mut y, 0.0).is_err());
    close(&read(&gpu, &sum, WIDTH), &[123.0; WIDTH], 1e-6);
    close(&read(&gpu, &y, WIDTH), &[456.0; WIDTH], 1e-6);
}
