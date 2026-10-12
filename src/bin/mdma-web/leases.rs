// Leases this gateway holds to keep models loaded: renewed while held,
// released on Release and when the gateway stops.
use mdma::grpc::client::Client;
use mdma::grpc::{AcquireRequest, ReleaseRequest, RenewRequest};
use rama::graceful::WeakShutdownGuard;
use rama::http::grpc::{Code, Request};
use super::deadline;
use super::pages;
use super::sse::element;
use super::upstream::{Upstream, describe};
use rama::futures::async_stream::stream_fn;
use rama::http::service::web::extract::{Query, State};
use rama::http::service::web::response::{IntoResponse, Sse};
use serde::Deserialize;
use std::sync::Arc;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

const TTL_MS: u32 = 60_000;
const RENEW_EVERY: Duration = Duration::from_secs(20);
/// Each lease call's deadline.
const CALL: Duration = Duration::from_secs(5);

/// Model name -> lease id.
#[derive(Default)]
pub(super) struct Leases(Mutex<BTreeMap<String, String>>);

fn call<T>(message: T) -> Result<Request<T>, String> {
    deadline::request(message, Some(CALL)).map_err(|e| e.to_string())
}

impl Leases {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.0.lock().expect("lease map lock")
    }

    pub fn held(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    pub fn holds(&self, model: &str) -> bool {
        self.lock().contains_key(model)
    }

    async fn acquire(client: &Client, model: &str) -> Result<String, String> {
        let request = call(AcquireRequest { model: model.to_owned(), ttl_ms: TTL_MS, wait: false })?;
        client.acquire(request).await.map(|r| r.into_inner().lease_id).map_err(|status| describe(&status))
    }

    /// Keeps `model` loaded until [`Self::release`].
    pub async fn hold(&self, client: &Client, model: &str) -> Result<(), String> {
        if self.holds(model) {
            return Ok(());
        }
        let id = Self::acquire(client, model).await?;
        self.lock().insert(model.to_owned(), id);
        Ok(())
    }

    pub async fn release(&self, client: &Client, model: &str) -> Result<(), String> {
        let Some(id) = self.lock().remove(model) else { return Ok(()) };
        match client.release(call(ReleaseRequest { lease_id: id })?).await {
            // Already expired on the server.
            Err(status) if status.code() != Code::NotFound => Err(describe(&status)),
            _ => Ok(()),
        }
    }

    /// Renews every lease, taking a new one where the server forgot it.
    async fn renew(&self, client: &Client) {
        let held: Vec<(String, String)> = self.lock().iter().map(|(m, id)| (m.clone(), id.clone())).collect();
        for (model, id) in held {
            let Ok(request) = call(RenewRequest { lease_id: id.clone(), ttl_ms: TTL_MS }) else { continue };
            let replaced = match client.renew(request).await {
                Ok(_) => continue,
                Err(status) if status.code() == Code::NotFound => Self::acquire(client, &model).await,
                Err(status) => Err(describe(&status)),
            };
            match replaced {
                // Unless it was released meanwhile.
                Ok(new) => if let Some(slot) = self.lock().get_mut(&model).filter(|slot| **slot == id) { *slot = new },
                Err(error) => eprintln!("mdma-web: keeping {model} loaded: {error}"),
            }
        }
    }

    /// Renews until shutdown, then releases everything held.
    pub async fn keep(&self, client: &Client, guard: WeakShutdownGuard) {
        loop {
            tokio::select! {
                () = guard.cancelled() => break,
                () = tokio::time::sleep(RENEW_EVERY) => self.renew(client).await,
            }
        }
        for model in self.held() {
            if let Err(error) = self.release(client, &model).await {
                eprintln!("mdma-web: releasing {model}: {error}");
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelQuery {
    model: String,
}

/// The federation panel with `outcome`'s error, if any.
fn answer(upstream: Arc<Upstream>, outcome: Result<(), String>) -> impl IntoResponse {
    Sse::new(stream_fn(move |mut yielder| async move {
        let status = upstream.snapshot();
        yielder.yield_item(element(pages::remote_panel(&status, &upstream.pin, &upstream.leases.held()))).await;
        yielder.yield_item(element(pages::lease_error(outcome.err().as_deref().unwrap_or_default()))).await;
    }))
}

pub(super) async fn hold(State(upstream): State<Arc<Upstream>>, Query(query): Query<ModelQuery>) -> impl IntoResponse {
    let outcome = if upstream.snapshot().models.iter().any(|m| m.name == query.model) {
        upstream.leases.hold(&upstream.client, &query.model).await
    } else {
        Err(format!("no model named {:?}", query.model))
    };
    answer(upstream, outcome)
}

pub(super) async fn release(State(upstream): State<Arc<Upstream>>, Query(query): Query<ModelQuery>) -> impl IntoResponse {
    let outcome = upstream.leases.release(&upstream.client, &query.model).await;
    answer(upstream, outcome)
}
