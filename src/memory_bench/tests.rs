use super::*;
use crate::backend::cpu::CpuBackend;
use std::assert_matches;

#[test]
fn bounds_precede_allocation() {
    for (bytes, iterations) in [
        (0, 1),
        (MAX_BYTES + 1, 1),
        (1, 0),
        (1, MAX_ITERATIONS + 1),
        (usize::MAX, usize::MAX),
    ] {
        assert_matches!(host(bytes, iterations), Err(Error::ShapeMismatch(_)));
        assert_matches!(
            device(&CpuBackend, bytes, iterations),
            Err(Error::ShapeMismatch(_))
        );
    }
}

#[test]
fn tiny_copies_verify_and_report_payload_units() {
    let mut results = vec![host(1024, 2).unwrap()];
    results.extend(device(&CpuBackend, 1024, 2).unwrap());
    assert_eq!(
        results.iter().map(|r| r.operation).collect::<Vec<_>>(),
        [
            "ram_copy",
            "host_to_device",
            "device_copy",
            "device_to_host"
        ]
    );
    for r in results {
        assert!(r.seconds > 0.);
        assert!(r.payload_gb_per_second.is_finite());
        assert_eq!(r.bytes_per_copy, 1024);
        assert_eq!(r.iterations, 2);
        assert_eq!(r.payload_gb_per_second, 2048. / r.seconds / 1e9);
    }
}
