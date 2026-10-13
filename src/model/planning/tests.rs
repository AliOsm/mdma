use super::*;
use crate::gguf::tests::Fixture;
use std::assert_matches;

#[test]
fn estimates_match_loaded_tensor_geometry_without_loading_weights() {
    let file = Fixture::new(3, 8).write();
    let gemma = crate::model::compat_tests::decoder_fixture(true);
    // Gemma's local producer keeps its 2-row window, the global one all 16.
    let gemma_kv = (4 * 2 + 8 * 16) * 4 * 2;
    for (path, context, kv) in [
        (file.path(), 32, None),
        (gemma.0.as_path(), 16, Some(gemma_kv)),
    ] {
        let mut gguf = GgufModel::open(path).unwrap();
        let estimate = estimate(&CpuBackend, &gguf, context, 5).unwrap();
        let budget = crate::memory::NativeMemoryBudget::new(usize::MAX);
        let model = Model::load_with_budget(&CpuBackend, &mut gguf, &budget).unwrap();
        assert_eq!(estimate.weights_bytes, budget.used());
        assert_eq!(
            estimate.kv_bytes,
            model.kv_bytes(&CpuBackend, context).unwrap()
        );
        assert_eq!(
            estimate.prefill_workspace_bytes,
            model.workspace_bytes(5).unwrap()
        );
        assert_eq!(
            estimate.decode_workspace_bytes,
            model.workspace_bytes(1).unwrap()
        );
        assert!(estimate.peak_bytes >= estimate.weights_bytes + estimate.load_staging_bytes);
        if let Some(kv) = kv {
            assert_eq!(estimate.kv_bytes, kv);
        }
    }
    let gguf = GgufModel::open(file.path()).unwrap();
    for (context, prompt) in [(0, 1), (65, 1), (32, 0), (32, 33)] {
        assert_matches!(
            super::estimate(&CpuBackend, &gguf, context, prompt),
            Err(Error::ShapeMismatch(_))
        );
    }
}

#[test]
fn exact_suffix_budgets_and_headroom_select_the_largest_fit() {
    let file = Fixture::new(3, 8).write();
    let gguf = GgufModel::open(file.path()).unwrap();
    let one = offload(&CpuBackend, &gguf, 32, 4, 1).unwrap();
    assert_eq!(one.cpu_layers, 2);
    assert_eq!(
        one.host.weights_bytes + one.device.weights_bytes,
        memory::weight_bytes(&gguf).unwrap()
    );
    let headroom = 256;
    let plan = auto_offload(
        &CpuBackend,
        &gguf,
        32,
        4,
        usize::MAX,
        one.device.peak_bytes + headroom,
        headroom,
    )
    .unwrap();
    assert_eq!(plan.gpu_layers, 1);
    let plan = auto_offload(&CpuBackend, &gguf, 32, 4, usize::MAX, 0, headroom).unwrap();
    assert_eq!(plan.gpu_layers, 0);
    assert_eq!(plan.device.peak_bytes, 0);
    assert_eq!(plan.bridge_bytes, 0);
    assert!(auto_offload(&CpuBackend, &gguf, 32, 4, 1, 0, headroom).is_err());
    assert_matches!(
        offload(&CpuBackend, &gguf, 32, 4, 4),
        Err(Error::ShapeMismatch(_))
    );
}

#[test]
fn unified_capacity_is_one_pool_not_ram_plus_vram() {
    let file = Fixture::new(3, 8).write();
    let g = GgufModel::open(file.path()).unwrap();
    let mut p = offload(&CpuBackend, &g, 32, 4, 1).unwrap();
    let reserve = 256;
    let combined = p.host.peak_bytes + p.device.peak_bytes + reserve;
    assert!(
        p.fits(p.host.peak_bytes, p.device.peak_bytes + reserve, reserve)
            .unwrap()
    );
    p.unified_memory = true;
    assert!(
        !p.fits(p.host.peak_bytes, p.device.peak_bytes + reserve, reserve)
            .unwrap()
    );
    assert!(
        !p.fits(combined - 1, p.device.peak_bytes + reserve, reserve)
            .unwrap()
    );
    assert!(
        p.fits(combined, p.device.peak_bytes + reserve, reserve)
            .unwrap()
    );
    assert_matches!(
        p.fits(usize::MAX, usize::MAX, usize::MAX),
        Err(Error::SizeOverflow)
    );
}
