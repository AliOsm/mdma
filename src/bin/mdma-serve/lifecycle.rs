use rama::error::BoxError;
use rama::graceful::Shutdown;
use std::future::Future;
use std::time::Duration;

#[cfg(unix)]
pub(super) fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    // Register eagerly, before starting model loading.
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
}

#[cfg(not(unix))]
pub(super) fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

pub(super) async fn supervise(
    shutdown: Shutdown,
    server: impl Future<Output = Result<(), BoxError>>,
    limit: Duration,
) -> Result<(), BoxError> {
    let drain = shutdown.shutdown_with_limit(limit);
    tokio::pin!(drain);
    let result = tokio::select! {
        result = server => {
            result?;
            // The listener stopping does not mean connections or workers have finished.
            drain.await
        }
        result = &mut drain => result,
    };
    result
        .map_err(|e| format!("mdma shutdown deadline exceeded; terminating remaining work: {e}"))?;
    eprintln!("mdma shutdown complete");
    Ok(())
}

#[cfg(test)]
#[path = "lifecycle/tests.rs"]
mod tests;
