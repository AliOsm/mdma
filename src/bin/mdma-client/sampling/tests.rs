use super::*;

#[test]
fn default_keeps_server_sampling_defaults() {
    assert!(config(|name| {
        assert_eq!(name, "MDMA_GREEDY");
        Err(VarError::NotPresent)
    }).unwrap().is_none());
    assert!(config(|_| Ok("0".into())).unwrap().is_none());
}

#[test]
fn greedy_sends_explicit_values_without_penalties() {
    let sampler = config(|_| Ok("1".into())).unwrap().unwrap();
    assert_eq!(sampler.temperature, Some(0.0));
    assert_eq!(sampler.top_p, Some(1.0));
    assert_eq!(sampler.min_p, Some(0.0));
    assert_eq!(sampler.repetition_penalty, Some(1.0));
    assert_eq!(sampler.repetition_window, Some(0));
}

#[test]
fn invalid_values_fail_instead_of_random_sampling() {
    for value in ["", "true", "false", "2", "-1", "0.0", " 1"] {
        let error = config(|_| Ok(value.into())).unwrap_err();
        assert!(error.to_string().contains("MDMA_GREEDY must be 0 or 1"));
    }
    assert!(config(|_| Err(VarError::NotUnicode("invalid".into()))).is_err());
}
