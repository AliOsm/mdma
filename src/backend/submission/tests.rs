use super::*;
use crate::memory::{BufferBudget, MemoryDomain};

#[test]
fn only_a_completed_submission_becomes_idle() {
    let mut submission = Submission::default();
    assert!(submission.is_idle());
    submission.finish(|| panic!("nothing to submit")).unwrap();
    submission.begin(|| Ok(())).unwrap();
    assert!(!submission.is_idle());
    submission.begin(|| panic!("already recording")).unwrap();
    submission.finish(|| Ok(())).unwrap();
    assert!(submission.is_idle());
}

#[test]
fn failed_submit_or_wait_keeps_deferred_buffers_charged() {
    for stage in ["end", "submit", "wait", "reset"] {
        let budget = BufferBudget::new(Some(100), None);
        let ((), mut deferred) = budget.allocate(MemoryDomain::Device, 100, || Ok(())).unwrap();
        let mut submission = Submission::default();
        submission.begin(|| Ok(())).unwrap();
        assert!(submission.finish(|| Err(Error::Backend(stage.into()))).is_err());
        assert!(!submission.is_idle());
        assert!(submission.begin(|| panic!("must not reuse")).is_err());
        assert!(submission.finish(|| panic!("must not resubmit")).is_err());
        assert_eq!(budget.used(MemoryDomain::Device), 100);
        // A later successful device-wide join permits teardown.
        deferred.release(|| true);
        assert_eq!(budget.used(MemoryDomain::Device), 0);
    }
}

#[test]
fn begin_failure_is_not_silently_retried() {
    let mut submission = Submission::default();
    assert!(submission.begin(|| Err(Error::Backend("begin".into()))).is_err());
    assert!(!submission.is_idle());
    assert!(submission.begin(|| panic!("must not reuse")).is_err());
}

#[test]
fn unwinding_does_not_make_inflight_work_idle() {
    let mut submission = Submission::default();
    submission.begin(|| Ok(())).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = submission.finish(|| panic!("injected submit failure"));
    }));
    assert!(result.is_err());
    assert!(!submission.is_idle());
    assert!(submission.finish(|| panic!("must not resubmit")).is_err());
}
