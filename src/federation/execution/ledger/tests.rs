use super::*;

#[test]
fn dedicated_inference_does_not_queue_behind_shared_blocking_pool_and_join_stays_charged() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let ledger = Ledger::new();
        let (entered, started) = std::sync::mpsc::sync_channel(1);
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let blocker = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let (executed, receive) = tokio::sync::oneshot::channel();
        let claim = ledger
            .claim(
                "origin.mdma.test",
                RequestId::parse(&[101; 16]).unwrap(),
                &config::Limits::default(),
            )
            .unwrap();
        let worker = claim
            .spawn_dedicated(move || {
                let _ = executed.send(());
                Outcome::Completed
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ledger.claim(
                "origin.mdma.test",
                RequestId::parse(&[102; 16]).unwrap(),
                &config::Limits::default()
            ),
            Err(Error::Concurrency)
        ));
        release.send(()).unwrap();
        blocker.await.unwrap();
        assert_eq!(worker.await.unwrap(), Outcome::Completed);
        drop(
            ledger
                .claim(
                    "origin.mdma.test",
                    RequestId::parse(&[103; 16]).unwrap(),
                    &config::Limits::default(),
                )
                .unwrap(),
        );
    });
}

#[tokio::test]
async fn dropping_dedicated_join_handle_never_releases_running_worker_charge() {
    let ledger = Ledger::new();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::sync_channel(1);
    let (finished, finish) = tokio::sync::oneshot::channel();
    let worker = ledger
        .claim(
            "origin.mdma.test",
            RequestId::parse(&[104; 16]).unwrap(),
            &config::Limits::default(),
        )
        .unwrap()
        .spawn_dedicated(move || {
            let _ = entered.send(());
            held.recv_timeout(Duration::from_secs(2)).unwrap();
            let _ = finished.send(());
            Outcome::Cancelled
        })
        .unwrap();
    started.await.unwrap();
    drop(worker);
    assert!(matches!(
        ledger.claim(
            "origin.mdma.test",
            RequestId::parse(&[105; 16]).unwrap(),
            &config::Limits::default()
        ),
        Err(Error::Concurrency)
    ));
    release.send(()).unwrap();
    finish.await.unwrap();
    // Join/claim teardown follows the compute hint. Wait on observable ledger
    // state, not the hint itself, before treating capacity as reusable.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if ledger.state.lock().unwrap().active == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dedicated_panic_is_not_success_and_retains_failed_id_after_join() {
    let ledger = Ledger::new();
    let worker = ledger
        .claim(
            "origin.mdma.test",
            RequestId::parse(&[106; 16]).unwrap(),
            &config::Limits::default(),
        )
        .unwrap()
        .spawn_dedicated(|| panic!("injected native worker panic"))
        .unwrap();
    assert!(worker.await.is_err());
    assert!(matches!(
        ledger.claim(
            "origin.mdma.test",
            RequestId::parse(&[106; 16]).unwrap(),
            &config::Limits::default()
        ),
        Err(Error::Duplicate)
    ));
    assert_eq!(ledger.state.lock().unwrap().active, 0);
}
use std::sync::Barrier;

fn id(n: u128) -> RequestId {
    RequestId::parse(&n.to_le_bytes()).unwrap()
}

struct Fixture {
    ledger: Arc<Ledger>,
    now: Arc<Mutex<Instant>>,
}

impl Fixture {
    fn new() -> Self {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = now.clone();
        let ledger = Ledger::with_clock(Arc::new(move || *clock.lock().unwrap()));
        Self { ledger, now }
    }

    fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().unwrap();
        *now += duration;
    }

    fn claim(&self, n: u128) -> Result<Claim, Error> {
        self.ledger
            .claim("origin.mdma.test", id(n), &config::Limits::default())
    }

    fn outcome(&self, n: u128) -> Option<Outcome> {
        let state = self.ledger.state.lock().unwrap();
        match state.peers.get("origin.mdma.test")?.entries.get(&id(n))? {
            Entry::Active => None,
            Entry::Terminal { outcome, .. } => Some(*outcome),
        }
    }
}

fn error(result: Result<Claim, Error>) -> Error {
    result.err().expect("unexpected admitted claim")
}

#[test]
fn exact_binary_ids_and_validated_limits_are_required() {
    assert_eq!(RequestId::parse(&[1; 15]), Err(Error::InvalidId));
    assert_eq!(RequestId::parse(&[1; 17]), Err(Error::InvalidId));
    assert!(RequestId::parse(&[0; 16]).is_ok()); // Randomness isn't remotely provable.
    let ledger = Ledger::new();
    assert_eq!(
        error(ledger.claim("bad/id", id(1), &config::Limits::default())),
        Error::InvalidPolicy
    );
    let limits = config::Limits {
        concurrency: 0,
        ..Default::default()
    };
    assert_eq!(
        error(ledger.claim("origin.mdma.test", id(1), &limits)),
        Error::InvalidPolicy
    );
    assert!(ledger.state.lock().unwrap().peers.is_empty());
}

#[test]
fn atomic_concurrent_duplicates_share_one_id_across_operations() {
    let fixture = Fixture::new();
    let barrier = Arc::new(Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let ledger = fixture.ledger.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ledger.claim("origin.mdma.test", id(1), &config::Limits::default())
            })
        })
        .collect();
    let mut claims = Vec::new();
    for thread in threads {
        match thread.join().unwrap() {
            Ok(claim) => claims.push(claim),
            Err(error) => assert_eq!(error, Error::Duplicate),
        }
    }
    assert_eq!(claims.len(), 1);
    let state = fixture.ledger.state.lock().unwrap();
    assert_eq!(state.active, 1);
    assert_eq!(state.entries, 1);
    drop(state);
    drop(claims);
    assert_eq!(fixture.outcome(1), Some(Outcome::Failed));
    assert_eq!(error(fixture.claim(1)), Error::Duplicate);
}

#[test]
fn validation_failures_and_concurrency_denials_leave_terminal_ids() {
    let fixture = Fixture::new();
    let active = fixture.claim(1).unwrap();
    assert_eq!(error(fixture.claim(2)), Error::Concurrency);
    assert_eq!(fixture.outcome(2), Some(Outcome::Failed));
    drop(active);
    assert_eq!(error(fixture.claim(2)), Error::Duplicate);
    fixture.claim(3).unwrap().reject(Outcome::Denied);
    assert_eq!(fixture.outcome(3), Some(Outcome::Denied));
    fixture.claim(4).unwrap().reject(Outcome::Completed);
    assert_eq!(
        fixture.outcome(4),
        Some(Outcome::Failed),
        "validation cannot report success"
    );
}

#[test]
fn terminal_retention_starts_at_actual_finish_not_admission() {
    let fixture = Fixture::new();
    let claim = fixture.claim(1).unwrap();
    fixture.advance(Duration::from_mins(15));
    assert_eq!(
        error(fixture.claim(1)),
        Error::Duplicate,
        "active IDs never expire"
    );
    claim.reject(Outcome::Cancelled);
    fixture.advance(Duration::from_secs(299));
    assert_eq!(error(fixture.claim(1)), Error::Duplicate);
    fixture.advance(Duration::from_millis(999));
    assert_eq!(error(fixture.claim(1)), Error::Duplicate);
    fixture.advance(Duration::from_millis(1));
    assert!(fixture.claim(1).is_ok());
}

#[test]
fn clock_regression_and_changed_limits_cannot_evict_active_or_terminal_ids() {
    let fixture = Fixture::new();
    let original = *fixture.now.lock().unwrap();
    let claim = fixture.claim(1).unwrap();
    fixture.advance(Duration::from_secs(20));
    assert_eq!(error(fixture.claim(1)), Error::Duplicate); // Observe later monotonic time.
    *fixture.now.lock().unwrap() = original;
    claim.reject(Outcome::DeadlineExceeded);
    let state = fixture.ledger.state.lock().unwrap();
    let Entry::Terminal { finished, .. } = &state.peers["origin.mdma.test"].entries[&id(1)] else {
        panic!("terminal")
    };
    assert_eq!(*finished, original + Duration::from_secs(20));
    drop(state);
    *fixture.now.lock().unwrap() = original + Duration::from_secs(319);
    let limits = config::Limits {
        ledger_entries: 1,
        ..Default::default()
    };
    assert_eq!(
        error(fixture.ledger.claim("origin.mdma.test", id(2), &limits)),
        Error::Full
    );
    assert_eq!(
        error(fixture.ledger.claim("origin.mdma.test", id(1), &limits)),
        Error::Duplicate
    );
    *fixture.now.lock().unwrap() = original + Duration::from_secs(320);
    assert!(
        fixture
            .ledger
            .claim("origin.mdma.test", id(2), &limits)
            .is_ok()
    );
}

#[test]
fn full_peer_ledger_never_evicts_unexpired_entries_and_rate_has_its_own_window() {
    let fixture = Fixture::new();
    let limits = config::Limits {
        ledger_entries: 2,
        requests_per_minute: 4,
        ..Default::default()
    };
    let claim = |n| fixture.ledger.claim("origin.mdma.test", id(n), &limits);
    drop(claim(1).unwrap());
    drop(claim(2).unwrap());
    assert_eq!(error(claim(3)), Error::Full);
    assert_eq!(error(claim(1)), Error::Duplicate);
    assert_eq!(error(claim(4)), Error::Rate);
    fixture.advance(Duration::from_secs(60));
    assert_eq!(
        error(claim(3)),
        Error::Full,
        "rate refresh does not reset IDs"
    );
    fixture.advance(Duration::from_secs(240));
    assert!(claim(3).is_ok());
}

#[test]
fn independent_origins_share_node_capacity_but_not_request_ids() {
    let fixture = Fixture::new();
    let claim = fixture.claim(1).unwrap();
    assert_eq!(
        error(
            fixture
                .ledger
                .claim("other.mdma.test", id(1), &config::Limits::default())
        ),
        Error::Concurrency
    );
    drop(claim);
    assert_eq!(
        error(
            fixture
                .ledger
                .claim("other.mdma.test", id(1), &config::Limits::default())
        ),
        Error::Duplicate
    );
    assert!(
        fixture
            .ledger
            .claim("other.mdma.test", id(2), &config::Limits::default())
            .is_ok()
    );
}

#[test]
fn local_peer_and_entry_bounds_cannot_be_multiplied_by_policy_limits() {
    let fixture = Fixture::new();
    let limits = config::Limits {
        requests_per_minute: 60_000,
        ledger_entries: 65_536,
        concurrency: 1024,
        ..Default::default()
    };
    for n in 0..NODE_ENTRIES as u128 {
        drop(
            fixture
                .ledger
                .claim("origin.mdma.test", id(n), &limits)
                .unwrap(),
        );
    }
    assert_eq!(
        error(
            fixture
                .ledger
                .claim("origin.mdma.test", id(NODE_ENTRIES as u128), &limits)
        ),
        Error::Full
    );
    assert_eq!(fixture.ledger.state.lock().unwrap().entries, NODE_ENTRIES);
    fixture.advance(RETENTION);
    assert!(fixture.claim(1).is_ok());

    let fixture = Fixture::new();
    for n in 0..config::MAX_PEERS {
        drop(
            fixture
                .ledger
                .claim(&format!("p{n}.mdma.test"), id(1), &limits)
                .unwrap(),
        );
    }
    assert_eq!(
        error(fixture.ledger.claim("one-more.mdma.test", id(1), &limits)),
        Error::Full
    );
    fixture.advance(RETENTION);
    assert!(
        fixture
            .ledger
            .claim("one-more.mdma.test", id(1), &limits)
            .is_ok()
    );
}

#[test]
fn poisoned_accounting_never_rebuilds_an_empty_ledger() {
    let fixture = Fixture::new();
    let claim = fixture.claim(1).unwrap();
    let ledger = fixture.ledger.clone();
    assert!(
        std::thread::spawn(move || {
            let _state = ledger.state.lock().unwrap();
            panic!("poison accounting");
        })
        .join()
        .is_err()
    );
    drop(claim);
    assert_eq!(error(fixture.claim(1)), Error::Unavailable);
    assert_eq!(error(fixture.claim(2)), Error::Unavailable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_or_dropping_a_running_handle_keeps_charge_until_resource_drop() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Resource(Arc<AtomicBool>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    for abort in [false, true] {
        let fixture = Fixture::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = Resource(dropped.clone());
        let (started, start) = tokio::sync::oneshot::channel();
        let (finish, waiting) = std::sync::mpsc::channel();
        let handle = fixture.claim(1).unwrap().spawn_blocking(move || {
            let _resource = resource;
            started.send(()).unwrap();
            waiting.recv().unwrap();
            Outcome::Completed
        });
        tokio::time::timeout(Duration::from_secs(2), start)
            .await
            .unwrap()
            .unwrap();
        if abort {
            handle.abort();
        }
        drop(handle);
        assert!(!dropped.load(Ordering::SeqCst));
        assert_eq!(fixture.ledger.state.lock().unwrap().active, 1);
        assert_eq!(error(fixture.claim(1)), Error::Duplicate);
        finish.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fixture.outcome(1).is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            dropped.load(Ordering::SeqCst),
            "resource drop precedes accounting completion"
        );
        assert_eq!(fixture.outcome(1), Some(Outcome::Completed));
        assert_eq!(fixture.ledger.state.lock().unwrap().active, 0);
    }
}

#[tokio::test]
async fn worker_panics_release_after_unwind_and_leave_failed_tombstones() {
    let fixture = Fixture::new();
    let handle = fixture
        .claim(1)
        .unwrap()
        .spawn_blocking(|| panic!("worker panic"));
    assert!(handle.await.unwrap_err().is_panic());
    assert_eq!(fixture.outcome(1), Some(Outcome::Failed));
    assert_eq!(fixture.ledger.state.lock().unwrap().active, 0);
    assert_eq!(error(fixture.claim(1)), Error::Duplicate);
}

#[test]
fn queued_worker_abort_drops_captures_without_starting_inference_or_forgetting_id() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Resource(Arc<AtomicBool>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture::new();
        let (started, start) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), start)
            .await
            .unwrap()
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let resource = Resource(dropped.clone());
        let worker = fixture.claim(1).unwrap().spawn_blocking(move || {
            let _resource = resource;
            panic!("a queued-aborted worker must never start");
        });
        assert_eq!(fixture.ledger.state.lock().unwrap().active, 1);
        assert!(!dropped.load(Ordering::SeqCst));
        worker.abort();
        // A queued task may keep its captures until the blocking executor
        // services that queue; don't assume abort means resources are free.
        release.send(()).unwrap();
        blocker.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(fixture.outcome(1), Some(Outcome::Failed));
        assert_eq!(fixture.ledger.state.lock().unwrap().active, 0);
        assert_eq!(error(fixture.claim(1)), Error::Duplicate);
    });
}

impl Ledger {
    pub(in crate::federation::execution) fn is_idle(&self) -> bool {
        self.state.lock().unwrap().active == 0
    }
}

impl Claim {
    fn reject(mut self, outcome: Outcome) {
        // A rejected admission cannot count as a completed inference.
        self.outcome = if outcome == Outcome::Completed {
            Outcome::Failed
        } else {
            outcome
        };
    }

    fn spawn_blocking(
        mut self,
        job: impl FnOnce() -> Outcome + Send + 'static,
    ) -> JoinHandle<Outcome> {
        tokio::task::spawn_blocking(move || {
            // FnOnce job captures (model/KV/workspace/guards) are destroyed as
            // it returns or unwinds, before accounting finishes below.
            let outcome = job();
            self.outcome = outcome;
            drop(self);
            outcome
        })
    }
}
