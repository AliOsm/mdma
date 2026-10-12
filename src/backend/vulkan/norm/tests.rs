use super::*;
use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::backend::test_support::{bytes, close, f32s};

fn upload(gpu: &VulkanBackend, x: &[f32]) -> VkBuf {
    let mut b = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut b, 0, &bytes(x)).unwrap();
    b
}

#[test]
fn fused_norm_matches_cpu_and_preserves_both_output_tails() {
    let gpu = VulkanBackend::new().unwrap();
    for (dim, tokens) in [
        (1, 3),
        (3, 1),
        (255, 3),
        (256, 1),
        (257, 3),
        (4096, 64),
        (8192, 3),
        (16385, 1),
    ] {
        let a: Vec<_> = (0..dim * tokens)
            .map(|i| (i % 31) as f32 * 0.125 - 1.875)
            .collect();
        let b: Vec<_> = a
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i / dim == 1 {
                    -*a
                } else {
                    (i % 17) as f32 * 0.0625 - 0.5
                }
            })
            .collect();
        let w: Vec<_> = (0..dim).map(|i| (i % 11) as f32 * 0.1 - 0.3).collect();
        let (ga, gb, gw) = (upload(&gpu, &a), upload(&gpu, &b), upload(&gpu, &w));
        let mut cs = CpuBackend.alloc(a.len() * 4, Pool::Scratch).unwrap();
        let mut cy = CpuBackend.alloc(a.len() * 4, Pool::Scratch).unwrap();
        CpuBackend
            .add_rmsnorm(
                &CpuBuf(bytes(&w)),
                &CpuBuf(bytes(&a)),
                &CpuBuf(bytes(&b)),
                &mut cs,
                &mut cy,
                1e-5,
            )
            .unwrap();
        for fused in [false, true] {
            gpu.set_fused_norm(fused);
            let mut gs = upload(&gpu, &vec![12345.0; a.len() + 7]);
            let mut gy = upload(&gpu, &vec![12345.0; a.len() + 7]);
            gpu.add_rmsnorm(&gw, &ga, &gb, &mut gs, &mut gy, 1e-5)
                .unwrap();
            let mut out = vec![0; gy.len];
            gpu.download(&gs, 0, &mut out).unwrap();
            assert_eq!(&out[..cs.0.len()], &cs.0, "residual f32 rounding");
            assert_eq!(&f32s(&out)[a.len()..], &[12345.0; 7]);
            gpu.download(&gy, 0, &mut out).unwrap();
            close(&f32s(&out)[..a.len()], &f32s(&cy.0), 2e-5);
            assert_eq!(&f32s(&out)[a.len()..], &[12345.0; 7]);
        }
    }
}

#[test]
fn fused_norm_rejects_invalid_shapes_before_dispatch() {
    let gpu = VulkanBackend::new().unwrap();
    let w = upload(&gpu, &[1.0; 3]);
    let a = upload(&gpu, &[1.0; 6]);
    let short = upload(&gpu, &[1.0; 5]);
    let mut sum = upload(&gpu, &[0.0; 6]);
    let mut y = upload(&gpu, &[0.0; 6]);
    for fused in [false, true] {
        gpu.set_fused_norm(fused);
        assert!(
            gpu.add_rmsnorm(&w, &a, &short, &mut sum, &mut y, 1e-5)
                .is_err()
        );
        assert!(
            gpu.add_rmsnorm(&w, &short, &short, &mut sum, &mut y, 1e-5)
                .is_err()
        );
        let mut small = upload(&gpu, &[0.0; 5]);
        assert!(
            gpu.add_rmsnorm(&w, &a, &a, &mut small, &mut y, 1e-5)
                .is_err()
        );
        assert!(
            gpu.add_rmsnorm(&w, &a, &a, &mut sum, &mut small, 1e-5)
                .is_err()
        );
        let odd = gpu.alloc(5, Pool::Scratch).unwrap();
        assert!(
            gpu.add_rmsnorm(&odd, &a, &a, &mut sum, &mut y, 1e-5)
                .is_err()
        );
    }
}

#[test]
#[ignore = "requires an idle Vulkan GPU"]
fn bench_residual_norm() {
    let gpu = VulkanBackend::new().unwrap();
    for (dim, tokens) in [(4096, 1), (4096, 64), (16384, 1), (16384, 64)] {
        let n = dim * tokens;
        let a = upload(&gpu, &vec![0.25; n]);
        let b = upload(&gpu, &vec![0.5; n]);
        let w = upload(&gpu, &vec![1.0; dim]);
        let mut sum = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        let mut y = gpu.alloc(n * 4, Pool::Scratch).unwrap();
        for fused in [false, true] {
            gpu.set_fused_norm(fused);
            for _ in 0..10 {
                gpu.add_rmsnorm(&w, &a, &b, &mut sum, &mut y, 1e-5).unwrap();
            }
            gpu.sync().unwrap();
            let start = std::time::Instant::now();
            for _ in 0..200 {
                gpu.add_rmsnorm(&w, &a, &b, &mut sum, &mut y, 1e-5).unwrap();
            }
            gpu.sync().unwrap();
            eprintln!(
                "residual RMSNorm {tokens}x{dim}, fused={fused}: {:.2} us",
                start.elapsed().as_secs_f64() * 1e6 / 200.0
            );
        }
    }
}
