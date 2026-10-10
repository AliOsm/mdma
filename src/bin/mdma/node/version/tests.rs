use super::*;

#[test]
fn rejects_legacy_old_invalid_and_future_protocol_but_not_compatible_new_builds() {
    let policy = Policy::new(Some("0.1.0"), None).unwrap();
    assert!(policy.check(None).is_err());
    let mut identity = local();
    identity.mdma_version = "0.0.9".into();
    assert!(policy.check(Some(&identity)).is_err());
    identity.mdma_version = "invalid".into();
    assert!(policy.check(Some(&identity)).is_err());
    identity.mdma_version = "0.1.0-rc.1".into();
    assert!(policy.check(Some(&identity)).is_err());
    identity.mdma_version = "0.1.0+different.build".into();
    assert!(policy.check(Some(&identity)).is_ok());
    identity.mdma_version = "0.2.0".into();
    identity.build_id = "other-build".into();
    assert!(policy.check(Some(&identity)).is_ok());
    identity.protocol_version = PROTOCOL + 1;
    assert!(policy.check(Some(&identity)).is_err());
}

#[test]
fn optional_build_pin_and_bad_policies() {
    let policy = Policy::new(None, Some("release-build")).unwrap();
    assert!(policy.check(Some(&local())).is_err());
    assert!(Policy::new(Some("bad"), None).is_err());
    assert!(Policy::new(None, Some("bad\nbuild")).is_err());
}
