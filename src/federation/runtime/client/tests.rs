use super::*;
use rama::{http::Body, service::service_fn};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn local_rpcs_preserve_missing_deadline_but_transport_receipts_are_private() {
    let inner = service_fn(|request: Request| async move {
        assert!(
            request
                .extensions()
                .get_ref::<Receipt>()
                .unwrap()
                .0
                .is_err()
        );
        Ok::<_, Infallible>(Response::new(Body::from("local")))
    });
    let policy = ClientPolicy {
        routing: true,
        expires_ms: 0,
    };
    let mut request = Request::new(Body::empty());
    request
        .headers_mut()
        .insert("caller-spki", "forged".parse().unwrap());
    let response = policy.http_service(inner).serve(request).await.unwrap();
    assert_eq!(response.status(), rama::http::StatusCode::OK);
}

#[tokio::test]
async fn supplied_deadline_bounds_stalled_decoding_and_quota_is_not_queued() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = entered.clone();
    let inner = service_fn(move |_: Request| {
        let observed = observed.clone();
        async move {
            observed.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok::<_, Infallible>(Response::new(Body::empty()))
        }
    });
    let gate = Arc::new(
        ClientPolicy {
            routing: true,
            expires_ms: 0,
        }
        .http_service(inner),
    );
    let request = || {
        let mut request = Request::new(Body::empty());
        request
            .headers_mut()
            .insert("grpc-timeout", "200m".parse().unwrap());
        request
    };
    let g = gate.clone();
    let first_request = request();
    let first = tokio::spawn(async move { g.serve(first_request).await });
    entered.notified().await;
    let g = gate.clone();
    let second_request = request();
    let second = tokio::spawn(async move { g.serve(second_request).await });
    entered.notified().await;
    let third = gate.serve(request()).await.unwrap();
    assert_eq!(third.headers().get("grpc-status").unwrap(), "8");
    for task in [first, second] {
        let response = task.await.unwrap().unwrap();
        assert_eq!(response.headers().get("grpc-status").unwrap(), "4");
    }
}

#[tokio::test]
async fn lease_covers_encoded_body_and_rechecks_before_plaintext_frame_release() {
    use rama::http::body::util::BodyExt;
    let permits = Arc::new(Semaphore::new(1));
    let permit = Arc::new(permits.clone().try_acquire_owned().unwrap());
    let eligible = Arc::new(AtomicBool::new(true));
    let flag = eligible.clone();
    let inner = service_fn(move |_: Request| {
        let permit = permit.clone();
        let flag = flag.clone();
        async move {
            let response = Response::new(Body::from("private output"));
            response.extensions().insert(execution::ResponseLease::new(
                permit,
                Arc::new(move || flag.load(Ordering::SeqCst)),
            ));
            Ok::<_, Infallible>(response)
        }
    });
    let gate = ClientPolicy {
        routing: true,
        expires_ms: 0,
    }
    .http_service(inner);
    let response = gate.serve(Request::new(Body::empty())).await.unwrap();
    assert_eq!(permits.available_permits(), 0);
    eligible.store(false, Ordering::SeqCst);
    assert!(response.into_body().collect().await.is_err());
    drop(gate); // Drops the fixture's retained inner permit too.
    assert_eq!(permits.available_permits(), 1);
}

impl ClientPolicy {
    pub(crate) fn test_routing(
        chain: &[rustls_pki_types::CertificateDer<'_>],
        roots: &[rustls_pki_types::CertificateDer<'_>],
    ) -> Self {
        Self {
            routing: true,
            expires_ms: tls::listener_expiry(chain, roots, tls::utc_ms().unwrap()).unwrap(),
        }
    }
}
