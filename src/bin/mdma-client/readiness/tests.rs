use super::*;
use mdma::grpc::ModelStatus;
use std::collections::VecDeque;
use std::future::{pending, ready};

fn healthy() -> IntrospectResponse {
    IntrospectResponse {
        ready: true,
        server_state: "serving".into(),
        default_model: "local".into(),
        models: vec![ModelStatus {
            name: "local".into(),
            ready: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn timeout_defaults_and_bounds() {
    assert_eq!(
        timeout(|_| Err(VarError::NotPresent)).unwrap(),
        Duration::from_secs(120)
    );
    for seconds in [0, 1, 120, 86400] {
        assert_eq!(
            timeout(|_| Ok(seconds.to_string())).unwrap(),
            Duration::from_secs(seconds)
        );
    }
    for value in ["", " ", "-1", "1.5", "many", "86401", "4294967296"] {
        assert!(
            timeout(|_| Ok(value.into()))
                .unwrap_err()
                .to_string()
                .contains("MDMA_READY_TIMEOUT_SECS")
        );
    }
    assert!(
        timeout(|_| Err(VarError::NotUnicode(std::ffi::OsString::new())))
            .unwrap_err()
            .to_string()
            .contains("MDMA_READY_TIMEOUT_SECS")
    );
}

#[tokio::test]
async fn ready_returns_after_one_probe() {
    let mut calls = 0;
    let result = wait(
        || {
            calls += 1;
            ready(Ok(healthy()))
        },
        Some(""),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(calls, 1);
    assert!(result.ready);
}

#[tokio::test]
async fn retries_transient_transport_errors_until_ready() {
    let mut results = VecDeque::from([
        Err(Status::unavailable("connection refused")),
        Err(Status::deadline_exceeded("busy")),
        Ok(healthy()),
    ]);
    let result = wait(
        || ready(results.pop_front().unwrap()),
        Some("local"),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(result.ready);
    assert!(results.is_empty());
}

#[tokio::test]
async fn waits_for_node_and_selected_model() {
    let mut node = healthy();
    node.ready = false;
    node.server_state = "draining".into();
    let mut model = healthy();
    model.models[0].ready = false;
    let mut results = VecDeque::from([Ok(node), Ok(model), Ok(healthy())]);
    assert!(
        wait(
            || ready(results.pop_front().unwrap()),
            Some(""),
            Duration::from_secs(2)
        )
        .await
        .unwrap()
        .ready
    );
    assert!(results.is_empty());
}

#[tokio::test]
async fn unrelated_unready_models_do_not_block_introspect_or_selected_model() {
    let mut health = healthy();
    health.models.push(ModelStatus {
        name: "offline".into(),
        ready: false,
        ..Default::default()
    });
    for selection in [None, Some(""), Some("local")] {
        assert!(
            wait(|| ready(Ok(health.clone())), selection, Duration::ZERO)
                .await
                .unwrap()
                .ready
        );
    }
}

#[tokio::test]
async fn explicit_ready_model_does_not_wait_for_unready_default() {
    let mut health = healthy();
    health.models[0].ready = false;
    health.models.push(ModelStatus {
        name: "other".into(),
        ready: true,
        ..Default::default()
    });
    assert!(
        wait(|| ready(Ok(health.clone())), Some("other"), Duration::ZERO)
            .await
            .unwrap()
            .ready
    );
    assert!(
        wait(|| ready(Ok(health.clone())), Some(""), Duration::ZERO)
            .await
            .unwrap_err()
            .to_string()
            .contains("model 'local' is not ready")
    );
}

#[tokio::test]
async fn unknown_model_fails_without_retry() {
    let mut calls = 0;
    let error = wait(
        || {
            calls += 1;
            ready(Ok(healthy()))
        },
        Some("missing"),
        Duration::from_secs(120),
    )
    .await
    .unwrap_err();
    assert_eq!(calls, 1);
    assert!(error.to_string().contains("unknown model 'missing'"));

    let front_door = IntrospectResponse {
        default_model: String::new(),
        models: vec![ModelStatus {
            name: "linux-qwen".into(),
            ready: true,
            ..Default::default()
        }],
        ..healthy()
    };
    let error = wait(
        || ready(Ok(front_door.clone())),
        Some(""),
        Duration::from_secs(120),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("no local models") && error.contains("linux-qwen"),
        "{error}"
    );
}

#[tokio::test]
async fn nontransient_rpc_errors_fail_without_retry() {
    for error in [
        Status::unauthenticated("client certificate rejected"),
        Status::permission_denied("access denied"),
        Status::invalid_argument("bad request"),
        Status::internal("server failure"),
    ] {
        let expected = error.code();
        let mut calls = 0;
        let error = wait(
            || {
                calls += 1;
                ready(Err(error.clone()))
            },
            None,
            Duration::from_secs(120),
        )
        .await
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.downcast_ref::<Status>().unwrap().code(), expected);
    }
}

#[tokio::test]
async fn zero_disables_retries_but_still_requires_readiness() {
    let mut calls = 0;
    let error = wait(
        || {
            calls += 1;
            ready(Err(Status::unavailable("connection refused")))
        },
        None,
        Duration::ZERO,
    )
    .await
    .unwrap_err();
    assert_eq!(calls, 1);
    assert!(error.to_string().contains("MDMA not ready"));
    let mut health = healthy();
    health.ready = false;
    assert!(
        wait(|| ready(Ok(health.clone())), None, Duration::ZERO)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unavailable_timeout_preserves_last_error_and_does_not_probe_after_deadline() {
    let mut calls = 0;
    let error = wait(
        || {
            calls += 1;
            ready(Err(Status::unavailable("connection refused")))
        },
        None,
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    assert_eq!(calls, 1);
    assert!(error.to_string().contains("timed out waiting"));
    assert!(error.to_string().contains("connection refused"));
}

#[tokio::test]
async fn total_deadline_bounds_a_stalled_introspect_rpc() {
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait(pending, None, Duration::from_millis(20)),
    )
    .await;
    assert!(
        result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("Introspect RPC timed out")
    );
}
