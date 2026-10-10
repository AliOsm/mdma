use super::*;
#[test]
fn cpu_probe_exercises_real_transfers_and_matrix_multiplication() {
    let caps = Probe.run(mdma::backend::cpu::CpuBackend).unwrap();
    assert_eq!(caps.kv_element_bytes, 4);
    assert_eq!(caps.device.as_deref(), Some("CPU"));
    assert!(caps.operations.iter().all(|(_, available)| *available));
}
