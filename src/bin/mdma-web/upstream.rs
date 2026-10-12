// Connection to mdma-serve: the Watch stream and playground runs.
use mdma::grpc::client::Client;
use mdma::grpc::{
    FederationTopology, Host, IntrospectResponse, MemorySnapshot, ModelStatus, ServerSnapshot,
    WatchRequest,
};
use rama::graceful::{ShutdownGuard, WeakShutdownGuard};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

const RECONNECT: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(1)..=Duration::from_secs(10);

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(super) struct Status {
    /// The `Watch` stream is up.
    pub connected: bool,
    pub error: Option<String>,
    pub ready: bool,
    pub state: String,
    pub backend: String,
    pub on_battery: bool,
    pub battery_percent: Option<u8>,
    pub gpu_power_limit_mw: u32,
    pub gpu_power_default_mw: u32,
    pub default_model: String,
    pub host: Option<Host>,
    pub models: Vec<ModelStatus>,
    /// Absent until the first `Watch` snapshot.
    pub memory: Option<MemorySnapshot>,
    pub federation: Option<FederationTopology>,
}

impl From<IntrospectResponse> for Status {
    fn from(r: IntrospectResponse) -> Self {
        Self {
            connected: true,
            error: None,
            ready: r.ready,
            state: r.server_state,
            backend: r.backend,
            on_battery: r.on_battery,
            battery_percent: u8::try_from(r.battery_percent).ok(),
            gpu_power_limit_mw: r.gpu_power_limit_mw,
            gpu_power_default_mw: r.gpu_power_default_mw,
            default_model: r.default_model,
            host: r.host,
            models: r.models,
            memory: None,
            federation: None,
        }
    }
}

impl From<ServerSnapshot> for Status {
    fn from(snapshot: ServerSnapshot) -> Self {
        Self {
            memory: snapshot.memory,
            federation: snapshot.federation,
            ..snapshot.status.map(Self::from).unwrap_or_default()
        }
    }
}

impl Status {
    fn unreachable(&self, error: String) -> Self {
        Self {
            connected: false,
            error: Some(error),
            ready: false,
            state: "unreachable".into(),
            ..self.clone()
        }
    }

    pub fn model(&self, name: &str) -> Option<&ModelStatus> {
        let name = if name.is_empty() {
            self.default_model.as_str()
        } else {
            name
        };
        self.models.iter().find(|m| m.name == name)
    }
}

pub(super) struct Upstream {
    pub client: Client,
    /// What this gateway keeps loaded.
    pub leases: super::leases::Leases,
    /// This gateway's certificate pin.
    pub pin: String,
    pub rpc_timeout: Option<Duration>,
    pub status: watch::Receiver<Arc<Status>>,
    streams: Arc<Semaphore>,
    runs: Mutex<HashMap<u64, CancellationToken>>,
    next_run: AtomicU64,
}

/// A running generation.
pub(super) struct Run {
    pub id: u64,
    pub cancel: CancellationToken,
    upstream: Arc<Upstream>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.upstream
            .runs
            .lock()
            .expect("run registry lock")
            .remove(&self.id);
    }
}

impl Upstream {
    pub fn start(
        client: Client,
        pin: String,
        rpc_timeout: Option<Duration>,
        max_streams: usize,
        watch_interval: Duration,
        guard: &ShutdownGuard,
    ) -> Arc<Self> {
        let (tx, rx) = watch::channel(Arc::new(Status {
            state: "connecting".into(),
            ..Status::default()
        }));
        let upstream = Arc::new(Self {
            client,
            pin,
            rpc_timeout,
            status: rx,
            leases: super::leases::Leases::default(),
            streams: Arc::new(Semaphore::new(max_streams)),
            runs: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(1),
        });
        guard.spawn_task(follow_status(
            upstream.clone(),
            tx,
            watch_interval,
            guard.clone_weak(),
        ));
        let (keeper, weak) = (upstream.clone(), guard.clone_weak());
        guard.spawn_task(async move { keeper.leases.keep(&keeper.client, weak).await });
        upstream
    }

    /// `None` when every stream slot is busy.
    pub fn begin(self: &Arc<Self>) -> Option<Run> {
        let permit = self.streams.clone().try_acquire_owned().ok()?;
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        self.runs
            .lock()
            .expect("run registry lock")
            .insert(id, cancel.clone());
        Some(Run {
            id,
            cancel,
            upstream: self.clone(),
            _permit: permit,
        })
    }

    /// Returns whether the run was still active.
    pub fn stop(&self, id: u64) -> bool {
        self.runs
            .lock()
            .expect("run registry lock")
            .get(&id)
            .inspect(|c| c.cancel())
            .is_some()
    }

    pub fn snapshot(&self) -> Arc<Status> {
        self.status.borrow().clone()
    }
}

/// Publishes a status change and logs connectivity transitions.
fn publish(tx: &watch::Sender<Arc<Status>>, next: Status) {
    let last = tx.borrow().clone();
    if next.connected != last.connected || next.error != last.error {
        match &next.error {
            Some(error) => eprintln!("mdma-web: server unreachable: {error}"),
            None => eprintln!(
                "mdma-web: connected to server (state={}, {} models)",
                next.state,
                next.models.len()
            ),
        }
    }
    tx.send_if_modified(|current| {
        let changed = **current != next;
        if changed {
            *current = Arc::new(next);
        }
        changed
    });
}

fn unreachable(tx: &watch::Sender<Arc<Status>>, error: String) {
    let next = tx.borrow().unreachable(error);
    publish(tx, next);
}

pub(super) fn describe(status: &rama::http::grpc::Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
}

/// Follows `Watch`, reconnecting with backoff.
async fn follow_status(
    upstream: Arc<Upstream>,
    tx: watch::Sender<Arc<Status>>,
    every: Duration,
    guard: WeakShutdownGuard,
) {
    let interval_ms = u32::try_from(every.as_millis()).unwrap_or(u32::MAX);
    let mut backoff = *RECONNECT.start();
    loop {
        let opened = tokio::select! {
            () = guard.cancelled() => return,
            opened = upstream.client.watch(WatchRequest { interval_ms }) => opened,
        };
        match opened {
            Ok(response) => {
                let mut snapshots = response.into_inner();
                loop {
                    let item = tokio::select! {
                        () = guard.cancelled() => return,
                        item = snapshots.next() => item,
                    };
                    match item {
                        Some(Ok(snapshot)) => {
                            backoff = *RECONNECT.start();
                            publish(&tx, Status::from(snapshot));
                        }
                        Some(Err(status)) => break unreachable(&tx, describe(&status)),
                        None => break unreachable(&tx, "server closed the status stream".into()),
                    }
                }
            }
            Err(status) => unreachable(&tx, describe(&status)),
        }
        tokio::select! {
            () = guard.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(*RECONNECT.end());
    }
}

#[cfg(test)]
#[path = "upstream/tests.rs"]
mod tests;
