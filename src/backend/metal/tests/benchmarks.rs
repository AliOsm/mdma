//! Warm, synchronous operator timings, not GPU-only or model tokens/s.
//! Run alone: cargo test --features metal --lib bench_metal_kernels -- --ignored --nocapture
use super::*;
use std::time::Instant;

fn measure(label: &str, iterations: usize, mut op: impl FnMut()) {
    for _ in 0..5 {
        op();
    }
    let mut micros = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        op();
        micros.push(start.elapsed().as_secs_f64() * 1e6);
    }
    report(label, &mut micros);
}

fn report(label: &str, micros: &mut [f64]) {
    micros.sort_by(f64::total_cmp);
    let iterations = micros.len();
    eprintln!(
        "METAL_BENCH {label} samples={iterations} median_us={:.2} p90_us={:.2}",
        micros[iterations / 2],
        micros[(iterations - 1) * 9 / 10],
    );
}

fn setup() -> (&'static MetalBackend, usize) {
    for name in ["MTL_DEBUG_LAYER", "MTL_SHADER_VALIDATION"] {
        assert!(std::env::var(name).map_or(true, |v| v == "0"), "disable {name} for timings");
    }
    let iterations = std::env::var("MDMA_METAL_BENCH_ITERS")
        .map_or(31, |s| s.parse::<usize>().expect("MDMA_METAL_BENCH_ITERS must be an integer"));
    assert!((1..=1000).contains(&iterations));
    let gpu = gpu();
    eprintln!(
        "METAL_BENCH device={} debug_assertions={} includes=host_checks,encoding,submit,wait excludes=compile,alloc,upload",
        gpu.device_name(),
        cfg!(debug_assertions),
    );
    (gpu, iterations)
}

#[test]
#[ignore = "operator microbenchmark; run alone on an idle GPU"]
fn bench_metal_kernels() {
    let (gpu, iterations) = setup();
    let (rows, cols) = (1024, 4096);
    for dtype in [GgmlDType::F32, GgmlDType::F16, GgmlDType::Q4K, GgmlDType::Q6K] {
        let raw = packed(dtype, rows, cols);
        let w = weight(gpu, dtype, rows, cols, &raw);
        for tokens in [1, 16] {
            let x = fbuf(gpu, &data(tokens * cols, 11));
            let mut y = gpu.alloc(tokens * rows * 4, Pool::Scratch).unwrap();
            measure(&format!("qmatmul dtype={dtype:?} rows={rows} cols={cols} tokens={tokens}"), iterations, || {
                gpu.qmatmul(&w, &x, &mut y).unwrap();
            });
            assert!(read(gpu, &y, tokens * rows).iter().all(|v| v.is_finite()));
        }
    }
    let width = 4096;
    let w = fbuf(gpu, &data(width, 13));
    for tokens in [1, 16, 256] {
        let x = fbuf(gpu, &data(tokens * width, 5));
        let mut y = gpu.alloc(tokens * width * 4, Pool::Scratch).unwrap();
        measure(&format!("rmsnorm width={width} tokens={tokens}"), iterations, || {
            gpu.rmsnorm(&w, &x, &mut y, 1e-5).unwrap();
        });
        assert!(read(gpu, &y, tokens * width).iter().all(|v| v.is_finite()));
    }
    let spec = AttnSpec { head_dim: 128, n_q_heads: 32, n_kv_heads: 8 };
    for (tokens, kv_len) in [(1, 128), (1, 1024), (16, 128)] {
        let q = fbuf(gpu, &data(tokens * spec.n_q_heads * spec.head_dim, 7));
        let k = fbuf(gpu, &data(kv_len * spec.n_kv_heads * spec.head_dim, 3));
        let v = fbuf(gpu, &data(kv_len * spec.n_kv_heads * spec.head_dim, 11));
        let kv = KvView { k: &k, v: &v, len: kv_len };
        let mut o = gpu.alloc(tokens * spec.n_q_heads * spec.head_dim * 4, Pool::Scratch).unwrap();
        measure(
            &format!("attention head_dim=128 q_heads=32 kv_heads=8 tokens={tokens} kv_len={kv_len}"),
            iterations,
            || {
                gpu.attn(&q, &kv, &spec, &mut o).unwrap();
            },
        );
        assert!(read(gpu, &o, tokens * spec.n_q_heads * spec.head_dim).iter().all(|v| v.is_finite()));
    }
}

// Pair samples and alternate which implementation goes first to reduce
// order bias. Each operation still synchronizes before the next begins.
fn measure_pair(label: &str, iterations: usize, mut reference: impl FnMut(), mut fused: impl FnMut()) {
    fn sample(samples: &mut Vec<f64>, op: &mut impl FnMut()) {
        let start = Instant::now();
        op();
        samples.push(start.elapsed().as_secs_f64() * 1e6);
    }
    for _ in 0..5 {
        reference();
        fused();
    }
    let mut old = Vec::with_capacity(iterations);
    let mut new = Vec::with_capacity(iterations);
    for i in 0..iterations {
        if i % 2 == 0 {
            sample(&mut old, &mut reference);
            sample(&mut new, &mut fused);
        } else {
            sample(&mut new, &mut fused);
            sample(&mut old, &mut reference);
        }
    }
    report(&format!("{label} path=separate"), &mut old);
    report(&format!("{label} path=fused"), &mut new);
}

#[test]
#[ignore = "paired operator microbenchmark; run alone on an idle GPU"]
fn bench_metal_fusions() {
    let (gpu, iterations) = setup();
    let width = 4096;
    let w = fbuf(gpu, &data(width, 13));
    for tokens in [1, 16, 256] {
        let n = tokens * width;
        let a = fbuf(gpu, &data(n, 5));
        let b = fbuf(gpu, &data(n, 7));
        let (mut sr, mut yr) = (gpu.alloc(n * 4, Pool::Scratch).unwrap(), gpu.alloc(n * 4, Pool::Scratch).unwrap());
        let (mut sf, mut yf) = (gpu.alloc(n * 4, Pool::Scratch).unwrap(), gpu.alloc(n * 4, Pool::Scratch).unwrap());
        measure_pair(
            &format!("add_rmsnorm width={width} tokens={tokens}"),
            iterations,
            || {
                gpu.add(&a, &b, &mut sr).unwrap();
                gpu.rmsnorm(&w, &sr, &mut yr, 1e-5).unwrap();
            },
            || gpu.add_rmsnorm(&w, &a, &b, &mut sf, &mut yf, 1e-5).unwrap(),
        );
        close(&read(gpu, &sf, n), &read(gpu, &sr, n), 0.0, 0.0);
        close(&read(gpu, &yf, n), &read(gpu, &yr, n), 1e-5, 1e-5);
    }
    let (rows, cols) = (1024, 4096);
    for dtype in [GgmlDType::F32, GgmlDType::F16, GgmlDType::Q4K, GgmlDType::Q6K] {
        let raw = packed(dtype, rows, cols);
        let g = weight(gpu, dtype, rows, cols, &raw);
        let mut raw_up = raw;
        raw_up.rotate_left(dtype.type_size());
        let u = weight(gpu, dtype, rows, cols, &raw_up);
        for tokens in [1, 16] {
            let x = fbuf(gpu, &data(tokens * cols, 11).iter().map(|v| v * 0.01).collect::<Vec<_>>());
            let n = tokens * rows;
            let alloc = || gpu.alloc(n * 4, Pool::Scratch).unwrap();
            let (mut gr, mut ur, mut yr) = (alloc(), alloc(), alloc());
            let (mut gf, mut uf, mut yf) = (alloc(), alloc(), alloc());
            measure_pair(
                &format!("ffn_gate_up dtype={dtype:?} rows={rows} cols={cols} tokens={tokens}"),
                iterations,
                || {
                    gpu.qmatmul(&g, &x, &mut gr).unwrap();
                    gpu.qmatmul(&u, &x, &mut ur).unwrap();
                    gpu.silu_mul(&gr, &ur, &mut yr).unwrap();
                },
                || gpu.ffn_gate_up(&g, &u, &x, &mut gf, &mut uf, &mut yf).unwrap(),
            );
            close(&read(gpu, &yf, n), &read(gpu, &yr, n), 2e-3, 2e-5);
        }
    }
}
