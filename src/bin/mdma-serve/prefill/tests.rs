use super::calibrate_shared;
use std::cell::Cell;

#[test]
fn a_later_calibration_cannot_override_a_rejection_or_failure() {
    for results in [
        [Ok(false), Ok(true)],
        [Ok(true), Ok(false)],
        [Err(()), Ok(true)],
        [Ok(true), Err(())],
    ] {
        let mut all_f16 = true;
        let backend_f16 = Cell::new(false);
        let mut expected = true;
        let probes = Cell::new(0);
        for result in results {
            expected &= result.unwrap_or(false);
            calibrate_shared(
                &mut all_f16,
                || {
                    probes.set(probes.get() + 1);
                    backend_f16.set(true);
                    result
                },
                |on| backend_f16.set(on),
            );
            assert_eq!(backend_f16.get(), expected);
        }
        assert_eq!(probes.get(), 2);
        assert!(!backend_f16.get());
    }
}

#[test]
fn all_models_must_pass_to_enable_f16() {
    let mut all_f16 = true;
    let backend_f16 = Cell::new(false);
    for _ in 0..3 {
        calibrate_shared(&mut all_f16, || Ok::<_, ()>(true), |on| backend_f16.set(on));
        assert!(backend_f16.get());
    }
}
