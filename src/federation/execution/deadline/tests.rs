use super::*;
use rama::http::HeaderValue;

fn headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("grpc-timeout", value.parse().unwrap());
    headers
}

#[test]
fn deadline_is_mandatory_positive_single_strict_and_bounded_before_add() {
    assert_eq!(
        Ingress::capture(&HeaderMap::new()).unwrap_err(),
        Error::InvalidGrpcDeadline
    );
    for invalid in [
        "",
        "S",
        "0S",
        "00000000n",
        "123456789m",
        "+1S",
        "-1S",
        "1s",
        "1h",
        "1",
        "1 S",
        "1.1S",
        "1SS",
        "1S,2S",
    ] {
        assert_eq!(
            Ingress::capture(&headers(invalid)).unwrap_err(),
            Error::InvalidGrpcDeadline,
            "{invalid}"
        );
    }
    let mut duplicate = headers("1S");
    duplicate.append("grpc-timeout", HeaderValue::from_static("2S"));
    assert_eq!(
        Ingress::capture(&duplicate).unwrap_err(),
        Error::InvalidGrpcDeadline
    );
    let mut invalid_bytes = HeaderMap::new();
    invalid_bytes.insert("grpc-timeout", HeaderValue::from_bytes(&[0xff]).unwrap());
    assert_eq!(
        Ingress::capture(&invalid_bytes).unwrap_err(),
        Error::InvalidGrpcDeadline
    );

    let now = Instant::now();
    for unit in ["H", "M", "S", "m", "u", "n"] {
        assert!(Ingress::capture(&headers(&format!("1{unit}"))).is_ok());
    }
    for (value, duration) in [
        ("1H", MAX_TIMEOUT),
        ("1M", Duration::from_secs(60)),
        ("1S", Duration::from_secs(1)),
        ("1m", Duration::from_millis(1)),
        ("1u", Duration::from_micros(1)),
        ("1n", Duration::from_nanos(1)),
    ] {
        assert_eq!(
            Ingress::capture_at(&headers(value), now)
                .unwrap()
                .grpc_deadline,
            now + duration
        );
    }
    let ingress = Ingress::capture_at(&headers("99999999H"), now).unwrap();
    assert_eq!(ingress.grpc_deadline, now + MAX_TIMEOUT);
}

#[test]
fn body_decoding_and_validation_consume_the_original_budget() {
    let received = Instant::now();
    let ingress = Ingress::capture_at(&headers("10S"), received).unwrap();
    let now = received + Duration::from_secs(2);
    let deadline = ingress
        .resolve(5000, received + Duration::from_secs(20), now)
        .unwrap();
    assert_eq!(deadline.at(), received + Duration::from_secs(5));
    assert_eq!(deadline.remaining_ms(now).unwrap(), 3000);
    assert_eq!(
        deadline
            .remaining_ms(now + Duration::from_micros(1))
            .unwrap(),
        2999
    );
    assert_eq!(
        deadline
            .remaining_ms(received + Duration::from_millis(4999))
            .unwrap(),
        1
    );
    assert_eq!(
        deadline.remaining_ms(received + Duration::from_micros(4_999_001)),
        Err(Error::Expired)
    );
    assert_eq!(deadline.remaining(deadline.at()), Err(Error::Expired));
    assert_eq!(
        deadline.remaining_ms(deadline.at() + Duration::from_secs(1)),
        Err(Error::Expired)
    );
}

#[test]
fn minimum_of_grpc_payload_local_limit_and_current_tls_binding_is_fixed() {
    let received = Instant::now();
    let ingress = Ingress::capture_at(&headers("1S"), received).unwrap();
    let long_credential = received + Duration::from_mins(15);
    assert_eq!(
        ingress
            .resolve(2000, long_credential, received)
            .unwrap()
            .at(),
        received + Duration::from_secs(1)
    );
    let ingress = Ingress::capture_at(&headers("99999999H"), received).unwrap();
    assert_eq!(
        ingress
            .resolve(MAX_TIMEOUT_MS, long_credential, received)
            .unwrap()
            .at(),
        received + MAX_TIMEOUT
    );
    let expiry = received + Duration::from_millis(50);
    assert_eq!(
        ingress.resolve(1000, expiry, received).unwrap().at(),
        expiry
    );
    assert_eq!(
        ingress.resolve(0, long_credential, received).unwrap_err(),
        Error::InvalidBudget
    );
    assert_eq!(
        ingress
            .resolve(MAX_TIMEOUT_MS + 1, long_credential, received)
            .unwrap_err(),
        Error::InvalidBudget
    );
    assert_eq!(
        ingress
            .resolve(1, long_credential, received + Duration::from_millis(1))
            .unwrap_err(),
        Error::Expired
    );
    assert_eq!(
        ingress.resolve(1000, received, received).unwrap_err(),
        Error::Expired
    );
    let fixture = crate::federation::tls::tests::Fixture::new();
    let (binding, _) = fixture.bindings();
    let ingress = Ingress::capture(&headers("99999999H")).unwrap();
    assert!(
        ingress
            .bind(MAX_TIMEOUT_MS, &binding, Instant::now())
            .unwrap()
            .at()
            <= binding.deadline()
    );
}
