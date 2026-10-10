use super::*;
use rama::http::{Body, HeaderValue, StatusCode};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Handler {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

impl Service<Request> for Handler {
    type Output = Response;
    type Error = Infallible;
    async fn serve(&self, request: Request) -> Result<Response, Infallible> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.extensions().get_ref::<Receipt>().is_some());
        tokio::time::sleep(self.delay).await;
        Ok(Response::new(Body::empty()))
    }
}

fn request(values: &[&str]) -> Request {
    let mut request = Request::new(Body::empty());
    *request.uri_mut() = "/mdma.federation.v1.Federation/Synthesize".parse().unwrap();
    for value in values {
        request
            .headers_mut()
            .append("grpc-timeout", HeaderValue::from_str(value).unwrap());
    }
    request
}

fn code(response: &Response) -> &str {
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get("grpc-status")
        .unwrap()
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn missing_malformed_zero_or_duplicate_timeout_never_reaches_decoder() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new(Handler {
        calls: calls.clone(),
        delay: Duration::ZERO,
    });
    for values in [
        vec![],
        vec!["0m"],
        vec!["3m", "4m"],
        vec![" 4m"],
        vec!["5X"],
        vec!["100000000m"],
    ] {
        assert_eq!(code(&gate.serve(request(&values)).await.unwrap()), "3");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fixed_ingress_deadline_bounds_body_decoding_and_does_not_reset() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new(Handler {
        calls: calls.clone(),
        delay: Duration::from_secs(1),
    });
    let start = Instant::now();
    assert_eq!(code(&gate.serve(request(&["5m"])).await.unwrap()), "4");
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(gate.decoding.available_permits(), 2);
}

#[tokio::test]
async fn predecode_work_is_bounded_and_never_queued() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new(Handler {
        calls: calls.clone(),
        delay: Duration::from_millis(20),
    });
    let one = gate.clone();
    let two = gate.clone();
    let one = tokio::spawn(async move { one.serve(request(&["100m"])).await });
    let two = tokio::spawn(async move { two.serve(request(&["100m"])).await });
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    assert_eq!(code(&gate.serve(request(&["100m"])).await.unwrap()), "8");
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn connections_charge_full_lifetime_and_do_not_queue() {
    #[derive(Clone)]
    struct Hold(Arc<tokio::sync::Notify>);
    impl Service<()> for Hold {
        type Output = ();
        type Error = rama::error::BoxError;
        async fn serve(&self, (): ()) -> Result<(), Self::Error> {
            self.0.notified().await;
            Ok(())
        }
    }
    let gate = Connections::new(Hold(Arc::new(tokio::sync::Notify::new())));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let gate = gate.clone();
        tasks.push(tokio::spawn(async move { gate.serve(()).await }));
    }
    while gate.slots.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    assert!(gate.serve(()).await.is_err());
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    assert_eq!(gate.slots.available_permits(), 8);
}

#[tokio::test]
async fn unary_body_retains_quota_and_checks_output_authorization_at_poll() {
    use rama::http::body::util::BodyExt as _;
    use std::sync::atomic::AtomicBool;
    let slots = Arc::new(Semaphore::new(1));
    let charge = Arc::new(slots.clone().try_acquire_owned().unwrap());
    let allowed = Arc::new(AtomicBool::new(true));
    let check = allowed.clone();
    let mut body = LeasedBody {
        inner: Body::from("sensitive-result"),
        _charge: charge,
        eligible: Arc::new(move || check.load(Ordering::SeqCst)),
    };
    assert_eq!(slots.available_permits(), 0);
    allowed.store(false, Ordering::SeqCst);
    assert!(
        body.frame().await.unwrap().is_err(),
        "no queued result is released after revoked authorization"
    );
    assert_eq!(slots.available_permits(), 0);
    drop(body);
    assert_eq!(slots.available_permits(), 1);
}
