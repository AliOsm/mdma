use super::*;
use crate::backend::{Backend, Pool};
use crate::backend::test_support::{bytes, close, cpu_qmatmul, qmatmul};
use crate::gguf::GgmlDType;

#[test]
fn unknown_fragment_layouts_are_not_enabled() {
    assert_eq!(generation(0x1002, vk::DriverId::MESA_RADV, "GPU (RADV GFX1201)"), Some(Generation::Rdna4));
    assert_eq!(generation(0x1002, vk::DriverId::MESA_RADV, "GPU (RADV GFX1100)"), Some(Generation::Rdna3));
    for (vendor, driver, name) in [
        (0x1002, vk::DriverId::MESA_RADV, "GPU (RADV GFX1030)"),
        (0x1002, vk::DriverId::MESA_RADV, "GPU (RADV GFX1300)"),
        (0x1002, vk::DriverId::AMD_PROPRIETARY, "GPU (RADV GFX1201)"),
        (0x10de, vk::DriverId::MESA_RADV, "GPU (RADV GFX1201)"),
    ] {
        assert_eq!(generation(vendor, driver, name), None);
    }
}

#[test]
fn matrix_shape_types_and_saturation_are_checked() {
    let p = vk::CooperativeMatrixPropertiesKHR::default().scope(vk::ScopeKHR::SUBGROUP)
        .m_size(16).n_size(16).k_size(16)
        .a_type(vk::ComponentTypeKHR::SINT8).b_type(vk::ComponentTypeKHR::SINT8)
        .c_type(vk::ComponentTypeKHR::SINT32).result_type(vk::ComponentTypeKHR::SINT32);
    assert!(matrix_supported(&p));
    assert!(!matrix_supported(&p.k_size(32)));
    assert!(!matrix_supported(&p.a_type(vk::ComponentTypeKHR::UINT8)));
    assert!(!matrix_supported(&p.saturating_accumulation(true)));
    assert!(!matrix_supported(&p.scope(vk::ScopeKHR::WORKGROUP)));
}

#[test]
fn scratch_size_is_checked() {
    assert_eq!(scratch_bytes(256, 67).unwrap(), 8 * 67 * 40);
    assert!(scratch_bytes(usize::MAX, usize::MAX).is_err());
    assert_eq!(quantize_grid(17), [2, 1, 1]);
    assert_eq!(quantize_grid(65535 * 16 + 1), [65535, 2, 1]);
    let [x, y, z] = quantize_grid(u32::MAX);
    assert!(x <= 65535 && y <= 65535 && z == 1);
}

fn gpu() -> super::super::VulkanBackend {
    super::super::VulkanBackend::new().unwrap().with_int8(true)
}

fn requantize(x: &[f32]) -> Vec<f32> {
    x.as_chunks::<32>().0.iter().flat_map(|block| {
        let amax = block.iter().map(|v| v.abs()).fold(0f32, f32::max);
        let inv = if amax > 0.0 { 127.0 / amax } else { 0.0 };
        let d = half::f16::from_f32(amax / 127.0).to_f32();
        block.iter().map(move |v| (v * inv).round() * d)
    }).collect()
}

#[test]
fn quantize_matches_q8_scale_rounding_and_integer_sums() {
    let gpu = gpu();
    if !gpu.int8_available() {
        eprintln!("int8 cooperative matrices unavailable");
        return;
    }
    let mut x = vec![0f32; 32 * 17];
    for (b, block) in x.as_chunks_mut::<32>().0.iter_mut().enumerate().skip(1) {
        let scale = match b {
            2 => 1e-9,
            3 => 1e-5,
            _ => 1.0,
        };
        for (i, v) in block.iter_mut().enumerate() {
            *v = ((i as f32 - 19.0) + 0.5) * scale;
        }
        if b == 1 { block[31] = 127.0; }
        if b == 4 { block[31] = 127.0 * (1.0 + 1.0 / 2048.0); }
        if b == 5 { block[31] = 127.0 * (1.0 + 3.0 / 2048.0); }
    }
    x.extend((0..768 * 3).map(|i| {
        if i / 32 % 13 == 0 { 0.0 } else {
            (((i * 31 + i / 768 * 7) % 255) as f32 - 127.0) / 128.0
        }
    }));
    let mut input = gpu.alloc(x.len() * 4, Pool::Scratch).unwrap();
    gpu.upload(&mut input, 0, &bytes(&x)).unwrap();
    let output = gpu.alloc(x.len() / 32 * Q8_BLOCK_BYTES, Pool::Scratch).unwrap();
    let n_blocks = (x.len() / 32) as u32;
    let pc = PcQuantize { x: input.addr, q8: output.addr, n_blocks, _pad: 0 };
    // A two-dimensional grid exercises flattening and a partial final group.
    gpu.launch("quantize_q8", &super::super::push(&pc), [2, n_blocks.div_ceil(32), 1]).unwrap();
    let mut raw = vec![0; output.len];
    gpu.download(&output, 0, &mut raw).unwrap();
    for (b, block) in x.as_chunks::<32>().0.iter().enumerate() {
        let amax = block.iter().map(|v| v.abs()).fold(0f32, f32::max);
        let inv = if amax > 0.0 { 127.0 / amax } else { 0.0 };
        let q: Vec<i8> = block.iter().map(|v| (v * inv).round() as i8).collect();
        let got = &raw[b * 40..(b + 1) * 40];
        assert_eq!(&got[..32], bytemuck::cast_slice::<i8, u8>(&q), "Q8 block {b}");
        let d = f32::from_le_bytes(got[32..36].try_into().unwrap());
        assert_eq!(d, half::f16::from_f32(amax / 127.0).to_f32(), "scale block {b}");
        let sum = i32::from_le_bytes(got[36..40].try_into().unwrap());
        assert_eq!(sum, q.iter().map(|v| i32::from(*v)).sum::<i32>(), "sum block {b}");
    }
}

fn weights(dtype: GgmlDType, rows: usize, n_in: usize) -> Vec<u8> {
    let bb = crate::backend::dtype_code::block_bytes(dtype) as usize;
    let mut raw: Vec<u8> = (0..rows * n_in / 256 * bb)
        .map(|i| ((i.wrapping_mul(2654435761) >> 13) & 255) as u8).collect();
    for (i, block) in raw.chunks_exact_mut(bb).enumerate() {
        let d = half::f16::from_f32(0.0001 * (1 + i % 7) as f32).to_le_bytes();
        if dtype == GgmlDType::Q4K {
            block[..2].copy_from_slice(&d);
            block[2..4].copy_from_slice(&half::f16::from_f32(0.0001).to_le_bytes());
        } else {
            block[208..210].copy_from_slice(&d);
        }
    }
    raw
}

#[test]
fn int8_matmul_matches_requantized_cpu_across_tiles_and_odd_q6_rows() {
    let gpu = gpu();
    if !gpu.int8_coopmat_available() {
        eprintln!("int8 cooperative matrices unavailable");
        return;
    }
    assert!(!gpu.int8_dot());
    matmul_cases(&gpu);
}

pub(in crate::backend::vulkan) fn matmul_cases(gpu: &super::super::VulkanBackend) {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K] {
        for (rows, n_in, tokens) in [
            (1, 256, 2), (3, 256, 3), (31, 256, 31), (33, 512, 33), (63, 768, 63),
            (64, 256, 64), (65, 512, 67), (129, 256, 128),
        ] {
            let raw = weights(dtype, rows, n_in);
            let x: Vec<f32> = (0..n_in * tokens).map(|i| {
                if i / 32 % 13 == 0 { 0.0 } else {
                    (((i * 31 + i / n_in * 7) % 255) as f32 - 127.0) / 128.0
                }
            }).collect();
            let want = cpu_qmatmul(dtype, &raw, &[rows, n_in], &requantize(&x));
            let got = qmatmul(gpu, dtype, &raw, &[rows, n_in], &x);
            eprintln!("int8 {dtype:?} {rows}x{n_in} x {tokens}");
            close(&got, &want, 1e-3);
        }
    }
}

#[test]
fn int8_keeps_decode_and_non_kquant_weights_on_existing_paths() {
    let gpu = gpu().with_f16_weights(false);
    assert_eq!(gpu.int8(), gpu.int8_available());
    fallback_cases(&gpu);
}

pub(in crate::backend::vulkan) fn fallback_cases(gpu: &super::super::VulkanBackend) {
    let raw = weights(GgmlDType::Q6K, 3, 256);
    let x: Vec<f32> = (0..256).map(|i| (i as f32 * 0.31).sin()).collect();
    close(&qmatmul(gpu, GgmlDType::Q6K, &raw, &[3, 256], &x),
        &cpu_qmatmul(GgmlDType::Q6K, &raw, &[3, 256], &x), 1e-3);
    let raw = bytes(&[1.0f32, -2.0, 3.0, 4.0, 5.0, -6.0]);
    let x = [2.0, -1.0, 0.5, -1.0, 2.0, 1.0];
    assert_eq!(qmatmul(gpu, GgmlDType::F32, &raw, &[2, 3], &x), [5.5, 0.0, -2.0, 0.0]);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn int8_matches_requantized_cpu_on_real_rows() {
    let gpu = gpu();
    assert!(gpu.int8_coopmat_available(), "requires supported RDNA3/4 cooperative matrices");
    assert!(!gpu.int8_dot());
    real_rows(&gpu);
}

pub(in crate::backend::vulkan) fn real_rows(gpu: &super::super::VulkanBackend) {
    for dtype in [GgmlDType::Q4K, GgmlDType::Q6K] {
        let (raw, shape) = crate::backend::test_support::real_tensor_rows(dtype, 65);
        let x: Vec<f32> = (0..67 * shape[1])
            .map(|i| (((i * 31 + i / shape[1] * 7) % 255) as f32 - 127.0) / 128.0).collect();
        let want = cpu_qmatmul(dtype, &raw, &shape, &requantize(&x));
        let got = qmatmul(gpu, dtype, &raw, &shape, &x);
        close(&got, &want, 1e-3);
    }
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and Vulkan hardware"]
fn int8_prefill_then_greedy_matches_llama_cpp() {
    let gpu = gpu();
    assert!(gpu.int8_coopmat_available(), "requires supported RDNA3/4 cooperative matrices");
    assert!(!gpu.int8_dot());
    super::super::tests::prefill_greedy(&gpu);
}
