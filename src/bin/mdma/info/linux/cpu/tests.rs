use super::*;

#[test]
fn hybrid_cpu_does_not_derive_cores_from_threads_per_core() {
    let cpu = parse(
        r#"{"lscpu":[
        {"field":"Vendor ID:","data":"GenuineIntel"},
        {"field":"Model name:","data":"Intel Core i9"},
        {"field":"Socket(s):","data":"1"},
        {"field":"Core(s) per socket:","data":"24"},
        {"field":"Thread(s) per core:","data":"2"},
        {"field":"CPU(s):","data":"32"},
        {"field":"CPU max MHz:","data":"6000.0000"},
        {"field":"CPU min MHz:","data":"800.0000"},
        {"field":"L3 cache:","data":"36 MiB (1 instance)"}
    ]}"#,
    )
    .unwrap();
    assert_eq!(cpu.manufacturer.as_deref(), Some("Intel"));
    assert_eq!(cpu.physical_cores, Some(24));
    assert_eq!(cpu.logical_threads, Some(32));
    assert_eq!(cpu.max_mhz, Some(6000.0));
    assert_eq!(cpu.caches[0].size, "36 MiB (1 instance)");
}

#[test]
fn proc_fallback_reports_average_observed_clock() {
    let cpu = fallback(
        "processor: 0\nvendor_id: AuthenticAMD\nmodel name: AMD Ryzen\ncpu MHz: 1000\n\nprocessor: 1\ncpu MHz: 3000\n\n",
    );
    assert_eq!(cpu.manufacturer.as_deref(), Some("AMD"));
    assert_eq!(cpu.logical_threads, Some(2));
    assert_eq!(cpu.current_mhz, Some(2000.0));
    assert!(cpu.max_mhz.is_none());
}

#[test]
fn absent_or_nonfinite_cpu_fields_do_not_become_zero() {
    let cpu = parse(r#"{"lscpu":[{"field":"CPU max MHz:","data":"NaN"}]}"#).unwrap();
    assert!(cpu.max_mhz.is_none());
    assert!(cpu.logical_threads.is_none());
    assert!(parse("invalid").is_err());
    assert!(fallback("").current_mhz.is_none());
}
