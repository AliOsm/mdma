use super::{CpuBackend, CpuBuf, Matmul, qmatmul_in};
use crate::backend::{AttnSpec, Backend, KvView, Pool, QWeight, RopeSpec};
use crate::gguf::GgmlDType;
use candle_core::quantized::{GgmlType, k_quants::BlockQ4K};

fn f32_buf(vals: &[f32]) -> CpuBuf {
    CpuBuf(bytemuck::cast_slice(vals).to_vec())
}

fn as_f32(buf: &CpuBuf) -> &[f32] {
    bytemuck::cast_slice(&buf.0)
}

#[test]
fn upload_rejects_oversized_source() {
    let backend = CpuBackend;
    let mut buf = backend.alloc(2, Pool::Scratch).expect("alloc");
    assert!(backend.upload(&mut buf, 0, &[1, 2, 3]).is_err());
}

#[test]
fn upload_rejects_offset_past_end() {
    let backend = CpuBackend;
    let mut buf = backend.alloc(4, Pool::Scratch).expect("alloc");
    assert!(backend.upload(&mut buf, 3, &[1, 2]).is_err());
}

#[test]
fn upload_at_offset_writes_in_place() {
    let backend = CpuBackend;
    let mut buf = backend.alloc(4, Pool::Scratch).expect("alloc");
    backend.upload(&mut buf, 2, &[9, 9]).expect("upload");

    let mut out = [0u8; 2];
    backend.download(&buf, 2, &mut out).expect("download");
    assert_eq!(out, [9, 9]);
}

#[test]
fn add_sums_elementwise() {
    let backend = CpuBackend;
    let a = f32_buf(&[1.0, 2.0, 3.0]);
    let b = f32_buf(&[10.0, 20.0, 30.0]);
    let mut y = backend.alloc(12, Pool::Scratch).expect("alloc");
    backend.add(&a, &b, &mut y).expect("add");
    assert_eq!(as_f32(&y), &[11.0, 22.0, 33.0]);
}

#[test]
fn embed_gathers_the_requested_row() {
    let backend = CpuBackend;
    // table = [[1, 2], [3, 4], [5, 6]] (vocab=3, n_embd=2); row 1 = [3, 4]
    let table = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![3, 2],
        buf: f32_buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
    };
    let mut out = backend.alloc(8, Pool::Scratch).expect("alloc");
    backend.embed(&table, &[1], &mut out).expect("embed");
    assert_eq!(as_f32(&out), &[3.0, 4.0]);
}

#[test]
fn embed_rejects_out_of_range_token_id() {
    let backend = CpuBackend;
    let table = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![3, 2],
        buf: f32_buf(&[0.0; 6]),
    };
    let mut out = backend.alloc(8, Pool::Scratch).expect("alloc");
    assert!(backend.embed(&table, &[3], &mut out).is_err());
}

#[test]
fn qmatmul_computes_a_plain_dot_product_for_f32_weights() {
    let backend = CpuBackend;
    // W = [[1, 2, 3], [4, 5, 6]] (2x3, row-major), x = [1, 1, 1]
    // y = W @ x = [1+2+3, 4+5+6] = [6, 15]
    let w = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![2, 3],
        buf: f32_buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
    };
    let x = f32_buf(&[1.0, 1.0, 1.0]);
    let mut y = backend.alloc(2 * 4, Pool::Scratch).expect("alloc");
    backend.qmatmul(&w, &x, &mut y).expect("qmatmul");
    assert_eq!(as_f32(&y), &[6.0, 15.0]);
}

#[test]
fn rmsnorm_matches_hand_computed_value() {
    let backend = CpuBackend;
    let w = f32_buf(&[1.0, 1.0]);
    let x = f32_buf(&[3.0, 4.0]);
    let mut y = backend.alloc(8, Pool::Scratch).expect("alloc");
    backend.rmsnorm(&w, &x, &mut y, 0.0).expect("rmsnorm");
    // mean_sq = (9 + 16) / 2 = 12.5, scale = 1/sqrt(12.5)
    let scale = 1.0 / 12.5f32.sqrt();
    let got = as_f32(&y);
    assert!((got[0] - 3.0 * scale).abs() < 1e-6);
    assert!((got[1] - 4.0 * scale).abs() < 1e-6);
}

#[test]
fn rope_at_position_zero_is_identity() {
    let backend = CpuBackend;
    let mut q = f32_buf(&[1.0, 2.0, 3.0, 4.0]);
    let mut k = f32_buf(&[5.0, 6.0]);
    let spec = RopeSpec {
        head_dim: 2,
        theta: 10000.0,
    };
    backend.rope(&mut q, &mut k, 0, 1, &spec).expect("rope");
    assert_eq!(as_f32(&q), &[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(as_f32(&k), &[5.0, 6.0]);
}

#[test]
fn rope_rotates_by_the_expected_angle() {
    let backend = CpuBackend;
    // head_dim=2, single head, pos=1: freq = theta^0 = 1, angle = 1.
    let mut q = f32_buf(&[1.0, 0.0]);
    let mut k = f32_buf(&[1.0, 0.0]);
    let spec = RopeSpec {
        head_dim: 2,
        theta: 10000.0,
    };
    backend.rope(&mut q, &mut k, 1, 1, &spec).expect("rope");
    let got = as_f32(&q);
    assert!((got[0] - 1.0f32.cos()).abs() < 1e-6);
    assert!((got[1] - 1.0f32.sin()).abs() < 1e-6);
}

#[test]
fn silu_mul_matches_hand_computed_value() {
    let backend = CpuBackend;
    let gate = f32_buf(&[0.0, 2.0]);
    let up = f32_buf(&[1.0, 1.0]);
    let mut y = backend.alloc(8, Pool::Scratch).expect("alloc");
    backend.silu_mul(&gate, &up, &mut y).expect("silu_mul");
    let got = as_f32(&y);
    // silu(0) = 0; silu(2) = 2 * sigmoid(2)
    let silu2 = 2.0 / (1.0 + (-2.0f32).exp());
    assert!((got[0] - 0.0).abs() < 1e-6);
    assert!((got[1] - silu2).abs() < 1e-5);
}

#[test]
fn attn_decode_matches_hand_computed_softmax_weighted_sum() {
    let backend = CpuBackend;
    let q = f32_buf(&[1.0, 0.0]);
    let k = f32_buf(&[1.0, 0.0, 0.0, 1.0]); // pos0=[1,0], pos1=[0,1]
    let v = f32_buf(&[10.0, 20.0, 30.0, 40.0]); // pos0=[10,20], pos1=[30,40]
    let kv = KvView {
        k: &k,
        v: &v,
        len: 2,
    };
    let spec = AttnSpec {
        head_dim: 2,
        n_q_heads: 1,
        n_kv_heads: 1,
    };
    let mut o = backend.alloc(8, Pool::Scratch).expect("alloc");
    backend.attn(&q, &kv, &spec, &mut o).expect("attn");

    let scale = 1.0 / 2.0f32.sqrt();
    let raw = [1.0 * scale, 0.0 * scale];
    let max = raw[0].max(raw[1]);
    let exps = [(raw[0] - max).exp(), (raw[1] - max).exp()];
    let sum: f32 = exps.iter().sum();
    let w0 = exps[0] / sum;
    let w1 = exps[1] / sum;
    let expected = [w0 * 10.0 + w1 * 30.0, w0 * 20.0 + w1 * 40.0];

    let got = as_f32(&o);
    assert!((got[0] - expected[0]).abs() < 1e-5);
    assert!((got[1] - expected[1]).abs() < 1e-5);
}

#[test]
fn packed_matmul_tracks_the_exact_path_for_every_quantized_type() {
    use super::{Matmul, dequant_row_at, qmatmul_in};
    let (n_out, n_in, n_tokens) = (8, 512, 3);
    let x: Vec<f32> = (0..n_tokens * n_in)
        .map(|i| (i as f32 * 0.61).cos())
        .collect();
    let norm = |v: &[f32]| v.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    for dtype in [
        GgmlDType::Q4_0,
        GgmlDType::Q4_1,
        GgmlDType::Q5_0,
        GgmlDType::Q5_1,
        GgmlDType::Q8_0,
        GgmlDType::Q2K,
        GgmlDType::Q3K,
        GgmlDType::Q4K,
        GgmlDType::Q5K,
        GgmlDType::Q6K,
        GgmlDType::F32,
    ] {
        let tensor = crate::gguf::tests::weight(&[n_out, n_in], dtype, 7);
        let w = QWeight {
            dtype,
            shape: vec![n_out, n_in],
            buf: CpuBuf(tensor.data().unwrap().into_owned()),
        };
        let run = |mode, x: &[f32]| {
            let mut y = vec![0f32; x.len() / n_in * n_out];
            qmatmul_in(mode, &w, x, &mut y).unwrap();
            y
        };
        // Exact: the dequantized weights' product, to f32 rounding.
        let exact = run(Matmul::Exact, &x);
        let mut row = vec![0f32; n_in];
        for (t, x) in x.chunks_exact(n_in).enumerate() {
            for r in 0..n_out {
                dequant_row_at(&w, r, n_in, &mut row).unwrap();
                let want: f64 = row
                    .iter()
                    .zip(x)
                    .map(|(a, b)| f64::from(*a) * f64::from(*b))
                    .sum();
                let got = f64::from(exact[t * n_out + r]);
                assert!((got - want).abs() < 1e-4, "{dtype:?}: {got} vs {want}");
            }
        }
        // Packed requantizes the activations: close to exact, and each
        // token the same batched as alone.
        let packed = run(Matmul::Packed, &x);
        let error: Vec<f32> = packed.iter().zip(&exact).map(|(p, e)| p - e).collect();
        let relative = norm(&error) / norm(&exact);
        assert!(relative < 0.02, "{dtype:?}: packed is {relative:.4} off");
        let alone: Vec<f32> = x
            .chunks_exact(n_in)
            .flat_map(|x| run(Matmul::Packed, x))
            .collect();
        assert_eq!(packed, alone, "{dtype:?}");
        if dtype == GgmlDType::F32 {
            assert_eq!(packed, exact, "f32 weights always take the exact path");
        }
    }
}

#[test]
fn every_kernel_level_this_cpu_runs_gives_the_same_bits() {
    use super::Kernels;
    use super::isa::{Level, isa};
    // 1000 values: 62 full sets of 16 lanes and a tail of 8.
    let a: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.37).sin()).collect();
    let b: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.11).cos()).collect();
    let q4k = crate::gguf::tests::weight(&[1, 512], GgmlDType::Q4K, 3)
        .data()
        .unwrap()
        .into_owned();
    let x: Vec<f32> = (0..512).map(|i| (i as f32 * 0.23).sin()).collect();
    let baseline = Kernels::at(Level::Baseline).expect("every CPU runs its baseline");
    let (dot, fused) = (baseline.row_dot(&a, &b), baseline.q4k(&q4k, &x));
    let mut ran = Vec::new();
    for level in [Level::Baseline, Level::X86V3, Level::X86V4] {
        let Some(kernels) = Kernels::at(level) else {
            assert!(
                level > isa().level,
                "{level:?} is detected but has no kernels"
            );
            continue;
        };
        let got = kernels.row_dot(&a, &b);
        assert_eq!(got.to_bits(), dot.to_bits(), "row_dot at {level:?}");
        let got = kernels.q4k(&q4k, &x);
        assert_eq!(got.to_bits(), fused.to_bits(), "fused Q4_K at {level:?}");
        ran.push(level);
    }
    assert_eq!(ran.last(), Some(&isa().level), "{ran:?} up to {:?}", isa());
}

#[test]
fn qmatmul_batched_matches_sequential_single_token_calls() {
    let backend = CpuBackend;
    let w = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![2, 3],
        buf: f32_buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
    };
    let x_data: Vec<f32> = vec![1.0, 0.5, -1.0, 2.0, -0.5, 1.5, 0.0, 1.0, -2.0]; // 3 tokens x n_in=3

    let x_batched = f32_buf(&x_data);
    let mut y_batched = backend.alloc(3 * 2 * 4, Pool::Scratch).expect("alloc");
    backend
        .qmatmul(&w, &x_batched, &mut y_batched)
        .expect("qmatmul batched");
    let got = as_f32(&y_batched).to_vec();

    let mut expected = Vec::new();
    for t in 0..3 {
        let x_t = f32_buf(&x_data[t * 3..(t + 1) * 3]);
        let mut y_t = backend.alloc(2 * 4, Pool::Scratch).expect("alloc");
        backend.qmatmul(&w, &x_t, &mut y_t).expect("qmatmul single");
        expected.extend_from_slice(as_f32(&y_t));
    }
    for (a, b) in got.iter().zip(expected.iter()) {
        assert!((a - b).abs() < 1e-6, "got {a}, want {b}");
    }
}

#[allow(unsafe_code)]
fn q4k_bytes(blocks: &[BlockQ4K]) -> Vec<u8> {
    // Candle owns the Q4_K layout; the test only needs its serialized GGUF
    // representation to exercise the CPU backend's byte-oriented loader.
    unsafe {
        std::slice::from_raw_parts(blocks.as_ptr().cast::<u8>(), std::mem::size_of_val(blocks))
            .to_vec()
    }
}

#[test]
fn q4k_fused_dot_is_the_exact_path_bit_for_bit_and_packed_stays_close() {
    use super::{dequant_row_at, fused, row_dot};
    const ROWS: usize = 3;
    const N_IN: usize = 512;
    let weights: Vec<f32> = (0..ROWS * N_IN)
        .map(|i| {
            let centered = (i.wrapping_mul(37) % 257) as f32 - 128.0;
            centered / 91.0 + ((i % 11) as f32 - 5.0) / 113.0
        })
        .collect();
    let mut blocks = vec![BlockQ4K::zeros(); weights.len() / BlockQ4K::BLCK_SIZE];
    BlockQ4K::from_float(&weights, &mut blocks);
    let weight = QWeight {
        dtype: GgmlDType::Q4K,
        shape: vec![ROWS, N_IN],
        buf: CpuBuf(q4k_bytes(&blocks)),
    };
    let x: Vec<f32> = (0..N_IN)
        .map(|i| ((i.wrapping_mul(19) % 149) as f32 - 74.0) / 83.0)
        .collect();
    let mut packed = vec![0.0f32; ROWS];
    qmatmul_in(Matmul::Packed, &weight, &x, &mut packed).expect("packed Q4_K matmul");
    let mut row = vec![0.0f32; N_IN];
    for (r, packed) in packed.iter().enumerate() {
        dequant_row_at(&weight, r, N_IN, &mut row).unwrap();
        let exact = row_dot(&row, &x);
        let got = fused::row_dot(&weight, r, N_IN, &x).expect("Q4_K has a fused kernel");
        assert_eq!(got.to_bits(), exact.to_bits(), "row {r}: {got} vs {exact}");
        assert!(
            (packed - exact).abs() <= 0.5,
            "packed Q4_K dot drifted by {}, fixture tolerance is 0.5",
            (packed - exact).abs()
        );
    }
}

#[test]
fn rmsnorm_batched_matches_sequential_single_token_calls() {
    let backend = CpuBackend;
    let w = f32_buf(&[1.0, 0.5, 2.0]);
    let x_data: Vec<f32> = vec![3.0, -1.0, 2.0, 0.5, 4.0, -2.0, 1.0, 1.0, 1.0];

    let x_batched = f32_buf(&x_data);
    let mut y_batched = backend.alloc(9 * 4, Pool::Scratch).expect("alloc");
    backend
        .rmsnorm(&w, &x_batched, &mut y_batched, 1e-5)
        .expect("rmsnorm batched");
    let got = as_f32(&y_batched).to_vec();

    let mut expected = Vec::new();
    for t in 0..3 {
        let x_t = f32_buf(&x_data[t * 3..(t + 1) * 3]);
        let mut y_t = backend.alloc(3 * 4, Pool::Scratch).expect("alloc");
        backend
            .rmsnorm(&w, &x_t, &mut y_t, 1e-5)
            .expect("rmsnorm single");
        expected.extend_from_slice(as_f32(&y_t));
    }
    for (a, b) in got.iter().zip(expected.iter()) {
        assert!((a - b).abs() < 1e-6, "got {a}, want {b}");
    }
}

#[test]
fn embed_batched_matches_sequential_single_token_calls() {
    let backend = CpuBackend;
    let table = QWeight {
        dtype: GgmlDType::F32,
        shape: vec![4, 2],
        buf: f32_buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
    };
    let ids = [0u32, 2, 3];

    let mut out_batched = backend.alloc(3 * 2 * 4, Pool::Scratch).expect("alloc");
    backend
        .embed(&table, &ids, &mut out_batched)
        .expect("embed batched");
    let got = as_f32(&out_batched).to_vec();

    let mut expected = Vec::new();
    for &id in &ids {
        let mut out_t = backend.alloc(2 * 4, Pool::Scratch).expect("alloc");
        backend
            .embed(&table, &[id], &mut out_t)
            .expect("embed single");
        expected.extend_from_slice(as_f32(&out_t));
    }
    assert_eq!(got, expected);
}

#[test]
fn rope_batched_matches_sequential_single_token_calls_with_incrementing_pos() {
    let backend = CpuBackend;
    let spec = RopeSpec {
        head_dim: 2,
        theta: 10000.0,
    };
    let q_data: Vec<f32> = vec![1.0, 0.0, 0.5, 0.5, -1.0, 2.0]; // 3 tokens, 1 head, head_dim=2
    let k_data: Vec<f32> = vec![0.0, 1.0, 1.0, 1.0, 2.0, -1.0];
    let start_pos = 5u32;

    let mut q_batched = f32_buf(&q_data);
    let mut k_batched = f32_buf(&k_data);
    backend
        .rope(&mut q_batched, &mut k_batched, start_pos, 3, &spec)
        .expect("rope batched");
    let got_q = as_f32(&q_batched).to_vec();
    let got_k = as_f32(&k_batched).to_vec();

    let mut expected_q = Vec::new();
    let mut expected_k = Vec::new();
    for t in 0..3 {
        let mut q_t = f32_buf(&q_data[t * 2..(t + 1) * 2]);
        let mut k_t = f32_buf(&k_data[t * 2..(t + 1) * 2]);
        backend
            .rope(&mut q_t, &mut k_t, start_pos + t as u32, 1, &spec)
            .expect("rope single");
        expected_q.extend_from_slice(as_f32(&q_t));
        expected_k.extend_from_slice(as_f32(&k_t));
    }
    for (a, b) in got_q.iter().zip(expected_q.iter()) {
        assert!((a - b).abs() < 1e-6, "q: got {a}, want {b}");
    }
    for (a, b) in got_k.iter().zip(expected_k.iter()) {
        assert!((a - b).abs() < 1e-6, "k: got {a}, want {b}");
    }
}

#[test]
fn attn_batched_causal_matches_sequential_decode_calls() {
    let backend = CpuBackend;
    let head_dim = 2;
    let spec = AttnSpec {
        head_dim,
        n_q_heads: 1,
        n_kv_heads: 1,
    };

    let q_data: Vec<f32> = vec![1.0, 0.0, 0.3, 0.9, -0.5, 1.2];
    let k_data: Vec<f32> = vec![0.2, 0.8, 1.0, -1.0, 0.5, 0.5];
    let v_data: Vec<f32> = vec![10.0, 1.0, 2.0, 20.0, 5.0, 5.0];

    let k_buf = f32_buf(&k_data);
    let v_buf = f32_buf(&v_data);
    let kv_batched = KvView {
        k: &k_buf,
        v: &v_buf,
        len: 3,
    };
    let q_batched = f32_buf(&q_data);
    let mut o_batched = backend
        .alloc(3 * head_dim * 4, Pool::Scratch)
        .expect("alloc");
    backend
        .attn(&q_batched, &kv_batched, &spec, &mut o_batched)
        .expect("attn batched");
    let got = as_f32(&o_batched).to_vec();

    let mut expected = Vec::new();
    for t in 0..3 {
        let k_prefix = f32_buf(&k_data[..(t + 1) * head_dim]);
        let v_prefix = f32_buf(&v_data[..(t + 1) * head_dim]);
        let kv_t = KvView {
            k: &k_prefix,
            v: &v_prefix,
            len: t + 1,
        };
        let q_t = f32_buf(&q_data[t * head_dim..(t + 1) * head_dim]);
        let mut o_t = backend.alloc(head_dim * 4, Pool::Scratch).expect("alloc");
        backend
            .attn(&q_t, &kv_t, &spec, &mut o_t)
            .expect("attn single");
        expected.extend_from_slice(as_f32(&o_t));
    }
    for (a, b) in got.iter().zip(expected.iter()) {
        assert!((a - b).abs() < 1e-5, "got {a}, want {b}");
    }
}

mod encoder {
    use super::{CpuBackend, as_f32, f32_buf};
    use crate::backend::{Backend, EncoderSpec, Pool};

    /// The shared harness every GPU port runs; on the oracle itself the
    /// two sides are the same code, so any positive tolerance means exact.
    #[test]
    fn shared_encoder_harness_is_exact_on_the_oracle() {
        crate::backend::test_support::encoder::encoder_ops_match_cpu(
            &CpuBackend,
            f32::MIN_POSITIVE,
        );
    }

    fn close(got: &[f32], want: &[f32], tol: f32) {
        assert_eq!(got.len(), want.len());
        for (i, (a, b)) in got.iter().zip(want).enumerate() {
            assert!((a - b).abs() < tol, "index {i}: got {a}, want {b}");
        }
    }

    #[test]
    fn layernorm_normalizes_each_row_with_bias() {
        let backend = CpuBackend;
        let x = f32_buf(&[1.0, 2.0, 3.0, 4.0, 10.0, 10.0, 10.0, 10.0]);
        let w = f32_buf(&[1.0, 2.0, 1.0, 2.0]);
        let b = f32_buf(&[0.5, 0.0, 0.0, 0.0]);
        let mut y = backend.alloc(32, Pool::Scratch).unwrap();
        backend.layernorm(&w, Some(&b), &x, &mut y, 1e-5).unwrap();
        let s = 1.0 / (1.25f32 + 1e-5).sqrt();
        close(
            as_f32(&y),
            &[
                -1.5 * s + 0.5,
                -0.5 * s * 2.0,
                0.5 * s,
                1.5 * s * 2.0,
                0.5,
                0.0,
                0.0,
                0.0,
            ],
            1e-5,
        );
    }

    #[test]
    fn gelu_matches_erf_reference_values() {
        let backend = CpuBackend;
        let x = f32_buf(&[-3.0, -1.0, 0.0, 0.5, 1.0, 3.0]);
        let mut y = backend.alloc(24, Pool::Scratch).unwrap();
        backend.gelu(&x, &mut y).unwrap();
        // torch.nn.functional.gelu (exact)
        close(
            as_f32(&y),
            &[-0.0040496, -0.1586553, 0.0, 0.3457312, 0.8413447, 2.9959507],
            2e-6,
        );
    }

    #[test]
    fn gelu_mul_relu_and_add_bias() {
        let backend = CpuBackend;
        let a = f32_buf(&[1.0, -1.0, 2.0, 0.0]);
        let g = f32_buf(&[2.0, 2.0, 0.5, 7.0]);
        let mut y = backend.alloc(16, Pool::Scratch).unwrap();
        backend.gelu_mul(&a, &g, &mut y).unwrap();
        close(
            as_f32(&y),
            &[2.0 * 0.8413447, 2.0 * -0.1586553, 0.5 * 1.9544997, 0.0],
            2e-6,
        );
        backend.relu(&a, &mut y).unwrap();
        assert_eq!(as_f32(&y), &[1.0, 0.0, 2.0, 0.0]);
        let mut rows = f32_buf(&[1.0, 2.0, 3.0, 4.0]);
        backend
            .add_bias(&f32_buf(&[10.0, 20.0]), &mut rows)
            .unwrap();
        assert_eq!(as_f32(&rows), &[11.0, 22.0, 13.0, 24.0]);
    }

    #[test]
    fn rope_neox_rotates_split_halves_per_sequence_position() {
        let backend = CpuBackend;
        // batch 2, seq_len 2, one head of dim 4: row r has position r % 2.
        let spec = EncoderSpec {
            seq_len: 2,
            n_heads: 1,
            head_dim: 4,
            window: None,
        };
        let mut q = f32_buf(&[
            1.0, 2.0, 3.0, 4.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 5.0, 6.0, 7.0, 8.0,
        ]);
        let mut k = f32_buf(&[0.0; 16]);
        backend.rope_neox(&mut q, &mut k, &spec, 10000.0).unwrap();
        let got = as_f32(&q);
        assert_eq!(&got[..4], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(&got[8..12], &[5.0, 6.0, 7.0, 8.0]);
        let (s0, c0) = 1f32.sin_cos();
        let (s1, c1) = (10000f32.powf(-0.5)).sin_cos();
        close(
            &got[4..8],
            &[
                1.0 * c0 - 3.0 * s0,
                2.0 * c1 - 4.0 * s1,
                3.0 * c0 + 1.0 * s0,
                4.0 * c1 + 2.0 * s1,
            ],
            1e-5,
        );
        close(
            &got[12..16],
            &[
                5.0 * c0 - 7.0 * s0,
                6.0 * c1 - 8.0 * s1,
                7.0 * c0 + 5.0 * s0,
                8.0 * c1 + 6.0 * s1,
            ],
            1e-5,
        );
    }

    #[test]
    fn attn_bidir_sees_both_directions_but_not_padding_or_outside_window() {
        let backend = CpuBackend;
        // seq_len 3, head_dim 1: keys are one-hot-ish values so the output is a weighted average.
        let spec = EncoderSpec {
            seq_len: 3,
            n_heads: 1,
            head_dim: 1,
            window: None,
        };
        let q = f32_buf(&[0.0, 0.0, 0.0]);
        let k = f32_buf(&[0.0, 0.0, 0.0]);
        let v = f32_buf(&[1.0, 3.0, 100.0]);
        let mut o = backend.alloc(12, Pool::Scratch).unwrap();
        backend.attn_bidir(&q, &k, &v, &[2], &spec, &mut o).unwrap();
        // Zero scores: uniform over the 2 valid keys, for both valid queries; padded row unspecified.
        close(&as_f32(&o)[..2], &[2.0, 2.0], 1e-6);

        let windowed = EncoderSpec {
            window: Some(1),
            ..spec
        };
        backend
            .attn_bidir(&q, &k, &v, &[3], &windowed, &mut o)
            .unwrap();
        close(as_f32(&o), &[2.0, 104.0 / 3.0, 51.5], 1e-4);
    }
}
