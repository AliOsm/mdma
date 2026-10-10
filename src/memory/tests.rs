use super::*;
use std::{assert_matches, sync::Barrier};

#[test]
fn exact_limit_split_and_last_owner_refund() {
    let budget = NativeMemoryBudget::new(100);
    let mut all = budget.reserve(100).unwrap();
    assert_matches!(budget.reserve(1), Err(Error::MemoryBudget { requested: 1, used: 100, limit: 100 }));
    let part = all.split(40);
    assert_eq!(budget.used(), 100);
    drop(all);
    assert_eq!(budget.used(), 40);
    assert!(part.covers(&budget, 40));
    assert!(!part.covers(&NativeMemoryBudget::new(100), 40));
    assert!(!part.covers(&budget, 41));
    drop(part);
    assert_eq!(budget.used(), 0);
    assert!(budget.reserve(101).is_err());
    assert_eq!(budget.used(), 0);
}

#[test]
fn byte_counts_do_not_truncate_to_u32_or_wrap() {
    let budget = NativeMemoryBudget::new(usize::MAX);
    let all = budget.reserve(usize::MAX).unwrap();
    assert!(budget.reserve(1).is_err());
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
            let memory = budget.reserve(80);
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
        let _memory = budget.reserve(100).unwrap();
        panic!("injected panic");
    });
    assert!(result.is_err());
    assert_eq!(budget.used(), 0);
}
