use super::*;
use crate::backend::test_support::close;
use crate::gguf::tests::Fixture;
use std::assert_matches;

fn parity<G: Backend>(gpu: &G, width: usize, tolerance: f32) {
    let file = Fixture::new(3, width).write();
    for count in 0..=3 {
        let mut g = GgufModel::open(file.path()).unwrap();
        let reference = Model::load(&CpuBackend, &mut g).unwrap();
        let hybrid = HybridModel::load(gpu, &mut g, count).unwrap();
        assert_eq!(hybrid.gpu_layers(), count);
        let mut a = reference.new_kv_cache(&CpuBackend, 32).unwrap();
        let mut b = hybrid.new_kv_cache(gpu, 32).unwrap();
        for (pos, tokens) in [(0, vec![1, 3, 7]), (3, vec![11, 13])] {
            let mut aw = reference.alloc_workspace(&CpuBackend, tokens.len()).unwrap();
            let mut bw = hybrid.alloc_workspace(gpu, tokens.len()).unwrap();
            let want = reference.prefill(&CpuBackend, &mut a, &mut aw, &tokens, pos).unwrap();
            let got = hybrid.prefill(gpu, &mut b, &mut bw, &tokens, pos).unwrap();
            close(&got, &want, tolerance);
            assert_eq!(a.len(), b.len());
        }
        let mut aw = reference.alloc_workspace(&CpuBackend, 1).unwrap();
        let mut bw = hybrid.alloc_workspace(gpu, 1).unwrap();
        for pos in 5..8 {
            let want = reference.forward(&CpuBackend, &mut a, &mut aw, 17, pos).unwrap();
            close(&hybrid.forward(gpu, &mut b, &mut bw, 17, pos).unwrap(), &want, tolerance);
        }
        assert_matches!(hybrid.forward(gpu, &mut b, &mut bw, 128, 8), Err(Error::ShapeMismatch(_)));
        assert_matches!(hybrid.forward(gpu, &mut b, &mut bw, 1, 9), Err(Error::ShapeMismatch(_)));
        assert_matches!(hybrid.alloc_workspace(gpu, 0).map(|_| ()), Err(Error::ShapeMismatch(_)));
        assert_matches!(hybrid.new_kv_cache(gpu, 65).map(|_| ()), Err(Error::ShapeMismatch(_)));
    }
}

#[test]
fn every_partition_matches_cpu_prefill_and_decode_with_retained_kv() {
    parity(&CpuBackend, 8, 1e-6);
}

#[test]
fn zero_offload_never_allocates_or_calls_the_device() {
    let gpu = crate::backend::test_support::NoUpload;
    let file = Fixture::new(3, 8).write();
    let mut g = GgufModel::open(file.path()).unwrap();
    let model = HybridModel::load(&gpu, &mut g, 0).unwrap();
    let mut kv = model.new_kv_cache(&gpu, 16).unwrap();
    let mut ws = model.alloc_workspace(&gpu, 2).unwrap();
    let got = model.prefill(&gpu, &mut kv, &mut ws, &[1, 3], 0).unwrap();
    assert_eq!(got.len(), 128);
    assert_eq!(kv.len(), 2);
}

#[cfg(feature = "vulkan")]
#[test]
#[ignore = "requires a Vulkan GPU; bounded three-layer generated fixture"]
fn vulkan_suffix_matches_cpu_and_preserves_prefix() {
    let gpu = crate::backend::vulkan::VulkanBackend::new().unwrap().with_f16_weights(false);
    parity(&gpu, 32, 1e-3);
}

#[cfg(feature = "rocm")]
#[test]
#[ignore = "requires a matching HIP GPU; bounded three-layer generated fixture"]
fn rocm_suffix_matches_cpu_and_preserves_prefix() {
    let gpu = crate::backend::rocm::HipBackend::new().unwrap();
    parity(&gpu, 32, 1e-3);
}
