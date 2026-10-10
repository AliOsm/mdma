use mdma::grpc::IntrospectResponse;
use rama::error::BoxError;
use rama::http::grpc::{Code, Status};
use std::env::VarError;
use std::future::Future;
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout_at};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub(super) fn timeout(env: impl FnOnce(&str) -> Result<String, VarError>) -> Result<Duration, BoxError> {
    let seconds = match env("MDMA_READY_TIMEOUT_SECS") {
        Ok(value) => value.parse::<u32>().ok().filter(|n| *n <= 86400)
            .ok_or("MDMA_READY_TIMEOUT_SECS must be an integer between 0 and 86400")?,
        Err(VarError::NotPresent) => 120,
        Err(error) => return Err(format!("MDMA_READY_TIMEOUT_SECS: {error}").into()),
    };
    Ok(Duration::from_secs(u64::from(seconds)))
}

fn not_ready(health: &IntrospectResponse, model: Option<&str>) -> Result<Option<String>, Status> {
    if !health.ready {
        return Ok(Some(format!("node state={}", health.server_state)));
    }
    if let Some(name) = model {
        let name = if name.is_empty() { health.default_model.as_str() } else { name };
        let selected = health.models.iter().find(|m| m.name == name)
            .ok_or_else(|| Status::not_found(format!("unknown model '{name}'")))?;
        if !selected.ready {
            return Ok(Some(format!("model '{name}' is not ready")));
        }
    }
    Ok(None)
}

pub(super) async fn wait<F, Fut>(
    mut probe: F,
    model: Option<&str>,
    timeout: Duration,
) -> Result<IntrospectResponse, BoxError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<IntrospectResponse, Status>>,
{
    let deadline = Instant::now() + if timeout.is_zero() { PROBE_TIMEOUT } else { timeout };
    let mut announced = false;
    loop {
        let probe_deadline = (Instant::now() + PROBE_TIMEOUT).min(deadline);
        let reason = match timeout_at(probe_deadline, probe()).await {
            Ok(Ok(health)) => match not_ready(&health, model)? {
                None => return Ok(health),
                Some(reason) => reason,
            },
            Ok(Err(error)) if matches!(error.code(), Code::Unavailable | Code::DeadlineExceeded) => error.to_string(),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => "Introspect RPC timed out".to_string(),
        };
        if timeout.is_zero() {
            return Err(format!("MDMA not ready: {reason}").into());
        }
        let expired = || -> BoxError {
            format!("timed out waiting for MDMA readiness after {:.3}s: {reason}", timeout.as_secs_f64()).into()
        };
        if Instant::now() >= deadline { return Err(expired()); }
        if !announced {
            eprintln!("waiting for MDMA readiness (up to {}s): {reason}", timeout.as_secs_f64());
            announced = true;
        }
        sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
        if Instant::now() >= deadline { return Err(expired()); }
    }
}

#[cfg(test)]
#[path = "readiness/tests.rs"]
mod tests;
