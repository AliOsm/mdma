use super::*;
use std::{assert_matches, sync::Barrier};

#[test]
fn exact_limit_split_and_last_owner_refund() {
    let budget = NativeMemoryBudget::new(100);
    let mut all = budget.reserve(100, MemoryCategory::Workspace).unwrap();
    assert_matches!(budget.reserve(1, MemoryCategory::Workspace), Err(Error::MemoryBudget { requested: 1, used: 100, limit: 100 }));
    let part = all.split_as(40, MemoryCategory::Workspace);
    assert_eq!(budget.used(), 100);
    drop(all);
    assert_eq!(budget.used(), 40);
    assert!(part.covers(&budget, 40));
    assert!(!part.covers(&NativeMemoryBudget::new(100), 40));
    assert!(!part.covers(&budget, 41));
    drop(part);
    assert_eq!(budget.used(), 0);
    assert!(budget.reserve(101, MemoryCategory::Workspace).is_err());
    assert_eq!(budget.used(), 0);
}

#[test]
fn byte_counts_do_not_truncate_to_u32_or_wrap() {
    let budget = NativeMemoryBudget::new(usize::MAX);
    let all = budget.reserve(usize::MAX, MemoryCategory::Workspace).unwrap();
    assert!(budget.reserve(1, MemoryCategory::Workspace).is_err());
    assert_eq!(budget.used(), usize::MAX);
    drop(all);
    assert_eq!(budget.used(), 0);
    assert_matches!(product(&[usize::MAX, 2]), Err(Error::SizeOverflow));
    assert_matches!(sum([usize::MAX, 1]), Err(Error::SizeOverflow));
}

#[test]
fn competing_reservations_are_all_or_nothing() {
    let budget = NativeMemoryBudget::new(100);
    let start = Barrier::new(3);
    let held = Barrier::new(3);
    let release = Barrier::new(3);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2).map(|_| scope.spawn(|| {
            start.wait();
            let memory = budget.reserve(80, MemoryCategory::Workspace);
            held.wait();
            release.wait();
            memory.is_ok()
        })).collect();
        start.wait();
        held.wait();
        let used = budget.used();
        release.wait();
        let admitted = workers.into_iter().map(|w| w.join().unwrap()).filter(|&ok| ok).count();
        assert_eq!(used, 80);
        assert_eq!(admitted, 1);
    });
    assert_eq!(budget.used(), 0);
}

#[test]
fn unwinding_refunds_reservations() {
    let budget = NativeMemoryBudget::new(100);
    let result = std::panic::catch_unwind(|| {
        let _memory = budget.reserve(100, MemoryCategory::Workspace).unwrap();
        panic!("injected panic");
    });
    assert!(result.is_err());
    assert_eq!(budget.used(), 0);
}

#[test]
fn categories_follow_reservations_and_splits() {
    let budget = NativeMemoryBudget::new(1000);
    let weights = budget.reserve(400, MemoryCategory::Weights).unwrap();
    // A request reserves KV and workspace together, then carves out the KV.
    let mut request = budget.reserve(300, MemoryCategory::Workspace).unwrap();
    let kv = request.split_as(200, MemoryCategory::Kv);
    let snapshot = |budget: &NativeMemoryBudget| budget.snapshot();
    assert_eq!(snapshot(&budget), BudgetSnapshot { limit: 1000, used: 700, weights: 400, kv: 200, workspace: 100 });
    assert_eq!((kv.bytes(), request.bytes(), weights.bytes()), (200, 100, 400));
    let same = request.split_as(40, MemoryCategory::Workspace);
    assert_eq!(snapshot(&budget).workspace, 100, "a same-category split moves nothing");
    drop((kv, same));
    assert_eq!(snapshot(&budget), BudgetSnapshot { limit: 1000, used: 460, weights: 400, kv: 0, workspace: 60 });
    drop((request, weights));
    assert_eq!(snapshot(&budget), BudgetSnapshot { limit: 1000, ..BudgetSnapshot::default() });
    assert!(budget.reserve(1001, MemoryCategory::Kv).is_err());
    assert_eq!(snapshot(&budget).kv, 0, "a refused reservation is not tallied");
}
