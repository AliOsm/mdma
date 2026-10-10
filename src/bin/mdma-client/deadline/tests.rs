use super::*;

#[test]
fn bounded_explicit_timeout_preserves_unset_local_semantics() {
    assert!(timeout(|_| Err(VarError::NotPresent)).unwrap().is_none());
    for value in ["0", "300001", "-1", "", "abc", "1.5"] {
        assert!(timeout(|_| Ok(value.into())).is_err(), "{value}");
    }
    for ms in [1, 30_000, 300_000] {
        let duration = timeout(|name| {
            assert_eq!(name, "MDMA_RPC_TIMEOUT_MS");
            Ok(ms.to_string())
        })
        .unwrap();
        assert_eq!(duration, Some(Duration::from_millis(ms)));
        assert!(
            request((), duration)
                .unwrap()
                .metadata()
                .contains_key(&"grpc-timeout")
        );
    }
    assert!(
        !request((), None)
            .unwrap()
            .metadata()
            .contains_key(&"grpc-timeout")
    );
    assert!(timeout(|_| Err(VarError::NotUnicode("bad".into()))).is_err());
}
