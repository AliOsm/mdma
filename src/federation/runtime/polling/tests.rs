use super::*;
use crate::federation::tls::tests::Fixture;

#[test]
fn jitter_and_failure_backoff_are_bounded_and_reset_after_success() {
    let now = Instant::now();
    for _ in 0..100 {
        let mut entry = Schedule::initial(now).unwrap();
        assert!(entry.due.duration_since(now) <= Duration::from_secs(2));
        for base in [5000, 10_000, 20_000, 40_000, 60_000, 60_000, 60_000] {
            entry.running = true;
            entry.finish(false, now).unwrap();
            assert!(!entry.running);
            let delay = entry.due.duration_since(now).as_millis();
            assert!(delay >= base * 4 / 5 && delay <= base);
        }
        entry.finish(true, now).unwrap();
        assert_eq!(entry.failures, 0);
        assert!((Duration::from_secs(18)..=Duration::from_secs(22)).contains(&entry.due.duration_since(now)));
    }
}

#[test]
fn synchronize_never_enrolls_staged_suspended_or_revoked_peers() {
    let fixture = Fixture::new();
    let mut staged = fixture.store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    staged.server_id = "staged.mdma.test".into();
    fixture.store.stage(staged).unwrap();
    let mut schedule = BTreeMap::new();
    synchronize(&mut schedule, &fixture.store.snapshot().unwrap(), Instant::now()).unwrap();
    assert_eq!(schedule.keys().cloned().collect::<Vec<_>>(), ["remote.mdma.test"]);
    let due = schedule["remote.mdma.test"].due;
    synchronize(&mut schedule, &fixture.store.snapshot().unwrap(), Instant::now()).unwrap();
    assert_eq!(schedule["remote.mdma.test"].due, due);
    fixture.store.suspend("remote.mdma.test").unwrap();
    synchronize(&mut schedule, &fixture.store.snapshot().unwrap(), Instant::now()).unwrap();
    assert!(schedule.is_empty());
}

#[test]
fn scheduling_is_global_bounded_single_flight_and_earliest_due_first() {
    let now = Instant::now();
    let mut schedule: BTreeMap<_, _> = (0..128).map(|i| (
        format!("peer-{i:03}.mdma.test"),
        Schedule { due: now.checked_sub(Duration::from_millis(i)).expect("clock past test offsets"), failures: 0, running: false },
    )).collect();
    assert!(due_peers(&mut schedule, now, 0).is_empty());
    let first = due_peers(&mut schedule, now, 8);
    assert_eq!(first.len(), MAX_INFLIGHT);
    assert_eq!(first[0], "peer-127.mdma.test");
    // Already running peers cannot be picked again.
    let next = due_peers(&mut schedule, now, 3);
    assert_eq!(next.len(), 3);
    assert!(next.iter().all(|id| !first.contains(id)));
    // Even a caller supplying an oversized budget cannot exceed the ceiling.
    assert_eq!(due_peers(&mut schedule, now, usize::MAX).len(), MAX_INFLIGHT);
}
