use super::*;
use crate::backend::{Error, cpu::CpuBackend};
use crate::gguf::tests::Fixture;
use std::assert_matches;

#[test]
fn direct_allocations_obey_the_models_budget() {
    let file = Fixture::new(1, 8).write();
    let mut gguf = GgufModel::open(file.path()).unwrap();
    let weights = weight_bytes(&gguf).unwrap();
    let budget = NativeMemoryBudget::new(weights);
    let model = Model::load_with_budget(&CpuBackend, &mut gguf, &budget).unwrap();
    assert_matches!(
        model.new_kv_cache(&CpuBackend, 1).map(|_| ()),
        Err(Error::MemoryBudget { .. })
    );
    assert_matches!(
        model.alloc_workspace(&CpuBackend, 1).map(|_| ()),
        Err(Error::MemoryBudget { .. })
    );
    assert_eq!(budget.used(), weights);
    drop(model);
    assert_eq!(budget.used(), 0);
    let small = NativeMemoryBudget::new(weights - 1);
    assert_matches!(
        Model::load_with_budget(&CpuBackend, &mut gguf, &small).map(|_| ()),
        Err(Error::MemoryBudget { .. })
    );
    assert_eq!(small.used(), 0);
}

#[test]
fn allocation_sizes_fail_closed_on_overflow_even_without_a_budget() {
    let model = crate::model::tests::tiny_model(&CpuBackend);
    assert!(model.alloc_workspace(&CpuBackend, usize::MAX).is_err());
    assert_matches!(
        workspace_sizes(model.config(), 128, usize::MAX),
        Err(Error::SizeOverflow)
    );
    assert_matches!(
        KvCache::<CpuBackend>::new(&CpuBackend, 2, usize::MAX, 1, 1).map(|_| ()),
        Err(Error::SizeOverflow)
    );
    assert_eq!(
        model.kv_bytes(&CpuBackend, usize::MAX).unwrap(),
        model.kv_bytes(&CpuBackend, 64).unwrap()
    );
}
