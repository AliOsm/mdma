use super::*;
use MemoryDomain::{Device, Host};
use std::{assert_matches, sync::Barrier};

struct Buffer {
    charge: BufferCharge,
    budget: BufferBudget,
    bytes: usize,
}

impl Buffer {
    fn new(budget: &BufferBudget, bytes: usize) -> Result<Self> {
        let ((), charge) = budget.allocate(Device, bytes, || {
            assert!(budget.used(Device) >= bytes, "admitted before driver call");
            Ok(())
        })?;
        Ok(Self {
            charge,
            budget: budget.clone(),
            bytes,
        })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.charge.release(|| {
            assert!(
                self.budget.used(Device) >= self.bytes,
                "charged during driver free"
            );
            true
        });
    }
}

#[test]
fn domains_are_independent_and_rejection_does_not_call_driver() {
    let budget = BufferBudget::new(Some(100), Some(20));
    let ((), mut device) = budget.allocate(Device, 100, || Ok(())).unwrap();
    let ((), mut host) = budget.allocate(Host, 20, || Ok(())).unwrap();
    for (domain, used, limit) in [(Device, 100, 100), (Host, 20, 20)] {
        assert_matches!(budget.allocate(domain, 1, || -> Result<()> { panic!("must not allocate") }),
            Err(Error::BufferBudget { domain: actual, requested: 1, used: u, limit: l }) if actual == domain && u == used && l == limit);
    }
    host.release(|| true);
    assert_eq!(budget.used(Host), 0);
    assert_eq!(budget.used(Device), 100);
    device.release(|| true);
    device.release(|| panic!("must not free twice"));
    assert_eq!(budget.used(Device), 0);
}

#[test]
fn failed_driver_allocation_and_unwind_refund_reservations() {
    let budget = BufferBudget::new(Some(100), None);
    assert!(
        budget
            .allocate(Device, 100, || -> Result<()> {
                assert_eq!(budget.used(Device), 100);
                Err(Error::Backend("injected allocation failure".into()))
            })
            .is_err()
    );
    assert_eq!(budget.used(Device), 0);
    let panic = std::panic::catch_unwind(|| {
        let _ = budget.allocate(Device, 100, || -> Result<()> {
            panic!("allocation did not succeed")
        });
    });
    assert!(panic.is_err());
    assert_eq!(budget.used(Device), 0);
}

#[test]
fn last_alias_retained_scratch_and_twins_share_one_ledger() {
    let budget = BufferBudget::new(Some(100), None);
    let weights = Arc::new(Buffer::new(&budget, 50).unwrap());
    let alias = weights.clone();
    let scratch = Buffer::new(&budget, 20).unwrap();
    let twin = budget.clone();
    let graph = Buffer::new(&twin, 30).unwrap();
    assert_eq!(budget.used(Device), 100);
    assert!(Buffer::new(&twin, 1).is_err());
    drop(weights);
    drop(graph);
    assert_eq!(budget.used(Device), 70);
    drop(alias);
    assert_eq!(
        budget.used(Device),
        20,
        "idle scratch is not a request reservation"
    );
    drop(scratch);
    assert_eq!(budget.used(Device), 0);
}

#[test]
fn partial_setup_and_scratch_growth_charge_the_overlap() {
    let budget = BufferBudget::new(Some(100), None);
    let scratch = Buffer::new(&budget, 40).unwrap();
    let setup = || -> Result<()> {
        let _first = Buffer::new(&budget, 50)?;
        let _second = Buffer::new(&budget, 20)?;
        Ok(())
    };
    assert!(setup().is_err());
    assert_eq!(budget.used(Device), 40);
    assert!(
        Buffer::new(&budget, 70).is_err(),
        "replacement cannot discount a still-live buffer"
    );
    drop(scratch);
    let replacement = Buffer::new(&budget, 70).unwrap();
    drop(replacement);
    assert_eq!(budget.used(Device), 0);
}

#[test]
fn deferred_and_failed_frees_do_not_refund_early() {
    let budget = BufferBudget::new(Some(100), None);
    let ((), charge) = budget.allocate(Device, 100, || Ok(())).unwrap();
    let mut deferred = [charge];
    assert_eq!(budget.used(Device), 100);
    deferred[0].release(|| {
        assert_eq!(budget.used(Device), 100);
        false
    });
    assert_eq!(budget.used(Device), 100);
    deferred[0].release(|| true);
    assert_eq!(budget.used(Device), 0);
    let ((), charge) = budget.allocate(Device, 100, || Ok(())).unwrap();
    drop(charge); // No successful release, e.g. abandoned after a device error.
    assert_eq!(budget.used(Device), 100);
    assert!(Buffer::new(&budget, 1).is_err());
}

#[test]
fn native_reservations_are_not_charged_to_buffer_quota() {
    let native = crate::memory::NativeMemoryBudget::new(100);
    let buffers = BufferBudget::new(Some(100), None);
    let reservation = native
        .reserve(100, crate::memory::MemoryCategory::Weights)
        .unwrap();
    let buf = Buffer::new(&buffers, 100).unwrap();
    assert_eq!(native.used(), 100);
    assert_eq!(buffers.used(Device), 100);
    drop(reservation);
    assert_eq!(buffers.used(Device), 100);
    drop(buf);
}

#[test]
fn cannot_replace_a_live_shared_or_failed_free_ledger() {
    let mut budget = BufferBudget::default();
    budget.replace(BufferBudget::new(Some(100), None)).unwrap();
    let twin = budget.clone();
    assert!(budget.replace(BufferBudget::default()).is_err());
    drop(twin);
    let ((), mut charge) = budget.allocate(Device, 1, || Ok(())).unwrap();
    assert!(budget.replace(BufferBudget::default()).is_err());
    charge.release(|| true);
    budget.replace(BufferBudget::new(Some(1), None)).unwrap();
    let ((), charge) = budget.allocate(Device, 1, || Ok(())).unwrap();
    drop(charge);
    assert!(budget.replace(BufferBudget::default()).is_err());
}

#[test]
fn competing_allocations_cannot_oversubscribe() {
    let budget = BufferBudget::new(Some(100), None);
    let held = Barrier::new(3);
    let release = Barrier::new(3);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let buffer = Buffer::new(&budget, 80);
                    held.wait();
                    release.wait();
                    buffer.is_ok()
                })
            })
            .collect();
        held.wait();
        let used = budget.used(Device);
        release.wait();
        let admitted = workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .filter(|&ok| ok)
            .count();
        assert_eq!(used, 80);
        assert_eq!(admitted, 1);
    });
    assert_eq!(budget.used(Device), 0);
}

#[test]
fn unlimited_still_checks_overflow_and_zero_can_forbid_a_domain() {
    let budget = BufferBudget::default();
    assert_eq!(budget.limit(Device), None);
    let ((), mut charge) = budget.allocate(Device, usize::MAX, || Ok(())).unwrap();
    assert!(budget.allocate(Device, 1, || Ok(())).is_err());
    charge.release(|| true);
    let budget = BufferBudget::new(Some(0), None);
    assert!(Buffer::new(&budget, 1).is_err());
}

#[test]
fn unsupported_backend_rejects_buffer_limits() {
    use crate::backend::Backend;
    let mut cpu = crate::backend::cpu::CpuBackend;
    assert_matches!(
        cpu.set_buffer_budget(BufferBudget::default()),
        Err(Error::NotImplemented(_))
    );
    assert!(cpu.buffer_budget().is_none());
}
