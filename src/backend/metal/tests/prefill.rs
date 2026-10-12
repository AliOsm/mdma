use super::*;

fn tiled_backend() -> MetalBackend {
    let mut backend = gpu().request_backend().unwrap();
    backend.pending = None;
    // Explicit kernel checks exercise available tiles even when the process
    // requests a production fallback. Unsupported devices still test matvec.
    backend.tile_prefill = backend.pipelines.contains_key("qmatmul_tile");
    backend
}

#[test]
fn shader_compiles_without_matrix_support() {
    let options = MTLCompileOptions::new();
    options.setLanguageVersion(MTLLanguageVersion::Version2_4);
    let library = gpu()
        .device
        .newLibraryWithSource_options_error(
            &NSString::from_str(&abi::source(false)),
            Some(&options),
        )
        .unwrap();
    assert!(
        library
            .newFunctionWithName(&NSString::from_str("qmatmul_tile"))
            .is_none()
    );
    for &name in KERNELS.iter().filter(|&&name| name != "qmatmul_tile") {
        let function = library
            .newFunctionWithName(&NSString::from_str(name))
            .unwrap();
        gpu()
            .device
            .newComputePipelineStateWithFunction_error(&function)
            .unwrap();
    }
}

#[test]
fn f32_matrix_tiles_match_cpu_for_all_formats_and_tails() {
    let backend = tiled_backend();
    let gpu = &backend;
    for dtype in [
        GgmlDType::F32,
        GgmlDType::F16,
        GgmlDType::BF16,
        GgmlDType::Q4K,
        GgmlDType::Q5K,
        GgmlDType::Q6K,
    ] {
        let cols = if dtype.block_size() == 1 { 37 } else { 512 };
        for rows in [32, 33, 67] {
            let raw = packed(dtype, rows, cols);
            for tokens in [7, 8, 9, 17] {
                let x = data(tokens * cols, 11);
                let want = cpu_qmatmul(dtype, &raw, &[rows, cols], &x);
                let got = gpu_qmatmul(gpu, dtype, &raw, &[rows, cols], &x);
                close(&got, &want, 2e-3, 2e-5);
            }
        }
    }
}

#[test]
fn matrix_tiles_handle_long_rows_and_tinyllama_ffn_shapes() {
    let backend = tiled_backend();
    let gpu = &backend;
    for (dtype, rows, cols) in [
        (GgmlDType::F32, 33, 4097),
        (GgmlDType::Q4K, 5632, 2048),
        (GgmlDType::Q6K, 2048, 5632),
    ] {
        let raw = packed(dtype, rows, cols);
        let x = data(9 * cols, 11);
        let want = cpu_qmatmul(dtype, &raw, &[rows, cols], &x);
        close(
            &gpu_qmatmul(gpu, dtype, &raw, &[rows, cols], &x),
            &want,
            2e-3,
            2e-5,
        );
    }
}

#[test]
fn tiled_ffn_matches_cpu_for_mixed_formats() {
    let backend = tiled_backend();
    let gpu = &backend;
    let cpu = CpuBackend;
    for (gate, up) in [
        (GgmlDType::Q4K, GgmlDType::Q6K),
        (GgmlDType::Q5K, GgmlDType::BF16),
        (GgmlDType::F32, GgmlDType::F16),
    ] {
        let (rows, cols, tokens) = (33, 512, 9);
        let g = packed(gate, rows, cols);
        let u = packed(up, rows, cols);
        let x: Vec<_> = data(tokens * cols, 5).iter().map(|v| v * 0.01).collect();
        let gc = weight(&cpu, gate, rows, cols, &g);
        let uc = weight(&cpu, up, rows, cols, &u);
        let gg = weight(gpu, gate, rows, cols, &g);
        let ug = weight(gpu, up, rows, cols, &u);
        let xc = fbuf(&cpu, &x);
        let xg = fbuf(gpu, &x);
        let n = rows * tokens;
        let (mut gs, mut us, mut y) = (
            fbuf(gpu, &vec![f32::NAN; n]),
            fbuf(gpu, &vec![f32::NAN; n]),
            fbuf(gpu, &vec![f32::NAN; n]),
        );
        let (mut gsc, mut usc, mut yc) = (
            fbuf(&cpu, &vec![0.0; n]),
            fbuf(&cpu, &vec![0.0; n]),
            fbuf(&cpu, &vec![0.0; n]),
        );
        cpu.ffn_gate_up(&gc, &uc, &xc, &mut gsc, &mut usc, &mut yc)
            .unwrap();
        gpu.ffn_gate_up(&gg, &ug, &xg, &mut gs, &mut us, &mut y)
            .unwrap();
        close(&read(gpu, &y, n), &read(&cpu, &yc, n), 2e-3, 2e-5);
    }
}
