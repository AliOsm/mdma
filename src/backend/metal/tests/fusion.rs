use super::*;
use std::assert_matches;

#[test]
fn fused_residual_norm_matches_cpu_with_tails_and_cancellation() {
    let gpu = gpu();
    let cpu = CpuBackend;
    let lanes = gpu.pipeline("add_rmsnorm_f32").unwrap().threadExecutionWidth();
    for width in [1, lanes - 1, lanes, lanes + 1, 4096, 4097] {
        for tokens in [0, 1, 5] {
            let n = width * tokens;
            let a = data(n, 3);
            let mut b = data(n, 7);
            // Last row cancels exactly, testing the epsilon-only norm.
            if tokens > 1 {
                for i in n - width..n {
                    b[i] = -a[i];
                }
            }
            let w = data(width, 11);
            let (wc, ac, bc) = (fbuf(&cpu, &w), fbuf(&cpu, &a), fbuf(&cpu, &b));
            let (wg, ag, bg) = (fbuf(gpu, &w), fbuf(gpu, &a), fbuf(gpu, &b));
            let (mut sc, mut yc) = (fbuf(&cpu, &vec![123.0; n]), fbuf(&cpu, &vec![456.0; n]));
            let (mut sg, mut yg) = (fbuf(gpu, &vec![123.0; n]), fbuf(gpu, &vec![456.0; n]));
            // Nonempty rows have a numerical CPU oracle; empty rows test no-op behavior.
            if tokens > 0 {
                cpu.add_rmsnorm(&wc, &ac, &bc, &mut sc, &mut yc, 1e-5).unwrap();
            }
            gpu.add_rmsnorm(&wg, &ag, &bg, &mut sg, &mut yg, 1e-5).unwrap();
            close(&read(gpu, &sg, n), &read(&cpu, &sc, n), 0.0, 0.0);
            close(&read(gpu, &yg, n), &read(&cpu, &yc, n), 1e-5, 1e-5);
        }
    }
}

fn ffn_case(gate_dtype: GgmlDType, up_dtype: GgmlDType, cols: usize) {
    ffn_shape_case(gate_dtype, up_dtype, 5, cols);
}

fn ffn_shape_case(gate_dtype: GgmlDType, up_dtype: GgmlDType, rows: usize, cols: usize) {
    let gpu = gpu();
    let cpu = CpuBackend;
    let gate = packed(gate_dtype, rows, cols);
    let mut up = packed(up_dtype, rows, cols);
    up.rotate_left(up_dtype.type_size()); // distinct, still valid packed weights
    let (gc, uc) = (weight(&cpu, gate_dtype, rows, cols, &gate), weight(&cpu, up_dtype, rows, cols, &up));
    let (gg, ug) = (weight(gpu, gate_dtype, rows, cols, &gate), weight(gpu, up_dtype, rows, cols, &up));
    for tokens in [1, 3] {
        let x: Vec<f32> = data(tokens * cols, 5).iter().map(|v| v * 0.01).collect();
        let (xc, xg) = (fbuf(&cpu, &x), fbuf(gpu, &x));
        let n = tokens * rows;
        let (mut gsc, mut usc, mut yc) =
            (fbuf(&cpu, &vec![0.0; n]), fbuf(&cpu, &vec![0.0; n]), fbuf(&cpu, &vec![0.0; n]));
        let (mut gsg, mut usg, mut yg) =
            (fbuf(gpu, &vec![f32::NAN; n]), fbuf(gpu, &vec![f32::NAN; n]), fbuf(gpu, &vec![f32::NAN; n]));
        cpu.ffn_gate_up(&gc, &uc, &xc, &mut gsc, &mut usc, &mut yc).unwrap();
        gpu.ffn_gate_up(&gg, &ug, &xg, &mut gsg, &mut usg, &mut yg).unwrap();
        close(&read(gpu, &yg, n), &read(&cpu, &yc, n), 2e-3, 2e-5);
        // Scratch must not be read, but its final contents are unspecified.
    }
}

#[test]
fn tinyllama_ffn_geometry_matches_cpu() {
    ffn_shape_case(GgmlDType::Q4K, GgmlDType::Q6K, 5632, 2048);
}

#[test]
fn fused_ffn_matches_cpu_for_every_dtype_pair() {
    let dtypes = [GgmlDType::F32, GgmlDType::F16, GgmlDType::Q4K, GgmlDType::Q6K];
    for gate in dtypes {
        for up in dtypes {
            ffn_case(gate, up, 512);
        }
    }
}

#[test]
fn fused_ffn_handles_long_rows_and_lane_tails() {
    let lanes = gpu().pipeline("ffn_gate_up").unwrap().threadExecutionWidth();
    for cols in [1, lanes - 1, lanes, lanes + 1, 4097] {
        ffn_case(GgmlDType::F32, GgmlDType::F16, cols);
    }
    ffn_case(GgmlDType::Q4K, GgmlDType::Q6K, 4096);
}

#[test]
fn fused_ffn_handles_saturated_silu() {
    let gpu = gpu();
    let g = weight(gpu, GgmlDType::F32, 3, 1, bytemuck::cast_slice(&[-100.0f32, 0.0, 100.0]));
    let u = weight(gpu, GgmlDType::F32, 3, 1, bytemuck::cast_slice(&[2.0f32, 3.0, -2.0]));
    let x = fbuf(gpu, &[1.0]);
    let (mut gs, mut us, mut y) = (fbuf(gpu, &[0.0; 3]), fbuf(gpu, &[0.0; 3]), fbuf(gpu, &[0.0; 3]));
    gpu.ffn_gate_up(&g, &u, &x, &mut gs, &mut us, &mut y).unwrap();
    close(&read(gpu, &y, 3), &[0.0, 0.0, -200.0], 1e-5, 1e-5);
}

#[test]
fn fused_ops_reject_invalid_shapes_before_writing() {
    let gpu = gpu();
    let w = fbuf(gpu, &[1.0; 4]);
    let bad = gpu.alloc(3, Pool::Scratch).unwrap();
    let short = fbuf(gpu, &[1.0; 3]);
    let mut sum = fbuf(gpu, &[123.0; 4]);
    let mut y = fbuf(gpu, &[456.0; 4]);
    for eps in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert!(gpu.add_rmsnorm(&w, &w, &w, &mut sum, &mut y, eps).is_err());
    }
    assert!(gpu.add_rmsnorm(&bad, &w, &w, &mut sum, &mut y, 1e-5).is_err());
    assert!(gpu.add_rmsnorm(&w, &short, &w, &mut sum, &mut y, 1e-5).is_err());
    assert!(gpu.add_rmsnorm(&w, &w, &short, &mut sum, &mut y, 1e-5).is_err());
    let mut short_out = fbuf(gpu, &[0.0; 3]);
    assert!(gpu.add_rmsnorm(&w, &w, &w, &mut short_out, &mut y, 1e-5).is_err());
    assert!(gpu.add_rmsnorm(&w, &w, &w, &mut sum, &mut short_out, 1e-5).is_err());
    close(&read(gpu, &sum, 4), &[123.0; 4], 0.0, 0.0);
    close(&read(gpu, &y, 4), &[456.0; 4], 0.0, 0.0);

    let g = weight(gpu, GgmlDType::F32, 4, 4, &[0; 64]);
    let mut u = weight(gpu, GgmlDType::F32, 4, 4, &[0; 64]);
    let (mut gs, mut us) = (fbuf(gpu, &[12.0; 4]), fbuf(gpu, &[34.0; 4]));
    assert!(gpu.ffn_gate_up(&g, &u, &short, &mut gs, &mut us, &mut y).is_err());
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut short_out, &mut us, &mut y).is_err());
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut short_out, &mut y).is_err());
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut us, &mut short_out).is_err());
    u.shape = vec![2, 8];
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut us, &mut y).is_err());
    u.shape = vec![4, 4];
    u.dtype = GgmlDType::Q8K;
    assert_matches!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut us, &mut y), Err(Error::NotImplemented(_)));
    u.dtype = GgmlDType::Q4K;
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut us, &mut y).is_err());
    u.dtype = GgmlDType::F32;
    u.buf = gpu.alloc(4, Pool::Weights).unwrap(); // truncated packed storage
    assert!(gpu.ffn_gate_up(&g, &u, &w, &mut gs, &mut us, &mut y).is_err());
    close(&read(gpu, &gs, 4), &[12.0; 4], 0.0, 0.0);
    close(&read(gpu, &us, 4), &[34.0; 4], 0.0, 0.0);
    close(&read(gpu, &y, 4), &[456.0; 4], 0.0, 0.0);
}
