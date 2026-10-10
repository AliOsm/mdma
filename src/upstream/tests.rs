use super::*;
use rama::http::grpc::Code;

#[test]
fn endpoints_require_an_https_origin() {
    for url in ["https://localhost:50051", "https://worker.example/", "https://[::1]:50051"] {
        assert!(origin(url).is_ok(), "{url}");
    }
    for url in [
        "http://localhost:50051",
        "localhost:50051",
        "https://host/completion",
        "https://host?model=x",
        "https://host#fragment",
        "https://user:password@host",
    ] {
        assert!(origin(url).is_err(), "{url}");
    }
}

#[test]
fn forwarding_counts_hops_without_losing_metadata() {
    let mut request = Request::new(IntrospectRequest {});
    request.metadata_mut().insert("x-request-id", "trace-1".parse().unwrap());
    for hops in 1..=MAX_HOPS {
        request = forward(request).unwrap();
        assert_eq!(request.metadata().get(HOPS_HEADER).unwrap(), hops.to_string().as_str());
        assert_eq!(request.metadata().get("x-request-id").unwrap(), "trace-1");
    }
    assert_eq!(forward(request).unwrap_err().code(), Code::ResourceExhausted);
}

#[test]
fn malformed_hops_cannot_reset_the_limit() {
    for value in ["", "-1", "256", "abc"] {
        let mut request = Request::new(IntrospectRequest {});
        request.metadata_mut().insert(HOPS_HEADER, value.parse().unwrap());
        assert_eq!(forward(request).unwrap_err().code(), Code::InvalidArgument);
    }
    let mut request = Request::new(IntrospectRequest {});
    request.metadata_mut().append(HOPS_HEADER, "0".parse().unwrap());
    request.metadata_mut().append(HOPS_HEADER, "8".parse().unwrap());
    assert_eq!(forward(request).unwrap_err().code(), Code::InvalidArgument);
}
