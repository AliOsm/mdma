//! Every encoder op on a backend against CpuBackend, on asymmetric data.
//! A GPU port of the Laya ops adds one test calling
//! `encoder_ops_match_cpu(&gpu, tol)` and inherits the whole suite.
use super::{bytes, close, f32s};
use crate::backend::cpu::CpuBackend;
use crate::backend::{Backend, EncoderSpec, Pool};

fn upload<B: Backend>(b: &B, data: &[f32]) -> B::Buf {
    let mut buf = b.alloc(data.len() * 4, Pool::Scratch).unwrap();
    b.upload(&mut buf, 0, &bytes(data)).unwrap();
    buf
}

fn download<B: Backend>(b: &B, buf: &B::Buf, n: usize) -> Vec<f32> {
    let mut out = vec![0u8; n * 4];
    b.download(buf, 0, &mut out).unwrap();
    f32s(&out)
}

fn ramp(n: usize, scale: f32, shift: f32) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7 % 11) as f32 - 5.0) * scale + shift)
        .collect()
}

pub(crate) fn encoder_ops_match_cpu<B: Backend>(gpu: &B, tol: f32) {
    let cpu = CpuBackend;
    let (batch, seq_len, n_heads, head_dim) = (2usize, 5usize, 2usize, 4usize);
    let dim = n_heads * head_dim;
    let n = batch * seq_len * dim;
    let lens = [5u32, 3];
    let x = ramp(n, 0.37, 0.1);
    let w = ramp(dim, 0.2, 1.0);
    let bias = ramp(dim, 0.05, -0.3);

    // layernorm, with and without bias
    for with_bias in [false, true] {
        let (xc, wc, bc) = (upload(&cpu, &x), upload(&cpu, &w), upload(&cpu, &bias));
        let (xg, wg, bg) = (upload(gpu, &x), upload(gpu, &w), upload(gpu, &bias));
        let mut yc = cpu.alloc(n * 4, Pool::Scratch).unwrap();
        let mut yg = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        cpu.layernorm(&wc, with_bias.then_some(&bc), &xc, &mut yc, 1e-5)
            .unwrap();
        gpu.layernorm(&wg, with_bias.then_some(&bg), &xg, &mut yg, 1e-5)
            .unwrap();
        close(&download(gpu, &yg, n), &download(&cpu, &yc, n), tol);
    }

    // add_bias (in place), gelu, relu, gelu_mul
    let (mut yc, mut yg) = (upload(&cpu, &x), upload(gpu, &x));
    cpu.add_bias(&upload(&cpu, &bias), &mut yc).unwrap();
    gpu.add_bias(&upload(gpu, &bias), &mut yg).unwrap();
    close(&download(gpu, &yg, n), &download(&cpu, &yc, n), tol);

    let gate = ramp(n, 0.5, -0.2);
    let (xc, xg, gc, gg) = (
        upload(&cpu, &x),
        upload(gpu, &x),
        upload(&cpu, &gate),
        upload(gpu, &gate),
    );
    let mut yc = cpu.alloc(n * 4, Pool::Scratch).unwrap();
    let mut yg = gpu.alloc(n * 4, Pool::Scratch).unwrap();
    cpu.gelu(&xc, &mut yc).unwrap();
    gpu.gelu(&xg, &mut yg).unwrap();
    close(&download(gpu, &yg, n), &download(&cpu, &yc, n), tol);
    cpu.relu(&xc, &mut yc).unwrap();
    gpu.relu(&xg, &mut yg).unwrap();
    close(&download(gpu, &yg, n), &download(&cpu, &yc, n), tol);
    cpu.gelu_mul(&xc, &gc, &mut yc).unwrap();
    gpu.gelu_mul(&xg, &gg, &mut yg).unwrap();
    close(&download(gpu, &yg, n), &download(&cpu, &yc, n), tol);

    // rope_neox at both ModernBERT thetas, then attention full and windowed
    let k = ramp(n, 0.29, 0.4);
    let v = ramp(n, 0.41, -0.6);
    for (theta, window) in [(160000.0f32, None), (10000.0, Some(1usize))] {
        let spec = EncoderSpec {
            seq_len,
            n_heads,
            head_dim,
            window,
        };
        let (mut qc, mut kc) = (upload(&cpu, &x), upload(&cpu, &k));
        let (mut qg, mut kg) = (upload(gpu, &x), upload(gpu, &k));
        cpu.rope_neox(&mut qc, &mut kc, &spec, theta).unwrap();
        gpu.rope_neox(&mut qg, &mut kg, &spec, theta).unwrap();
        close(&download(gpu, &qg, n), &download(&cpu, &qc, n), tol);
        close(&download(gpu, &kg, n), &download(&cpu, &kc, n), tol);

        let (vc, vg) = (upload(&cpu, &v), upload(gpu, &v));
        let mut oc = cpu.alloc(n * 4, Pool::Scratch).unwrap();
        let mut og = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        cpu.attn_bidir(&qc, &kc, &vc, &lens, &spec, &mut oc)
            .unwrap();
        gpu.attn_bidir(&qg, &kg, &vg, &lens, &spec, &mut og)
            .unwrap();
        // Padded query rows are unspecified; compare the valid rows only.
        let (got, want) = (download(gpu, &og, n), download(&cpu, &oc, n));
        for (b, &len) in lens.iter().enumerate() {
            let rows = b * seq_len * dim..(b * seq_len + len as usize) * dim;
            close(&got[rows.clone()], &want[rows], tol);
        }
    }
}
