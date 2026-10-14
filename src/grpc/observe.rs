//! `Watch`: status, memory and federation snapshots.
use super::proto::{
    DeviceMemory, FederationTopology, MemorySnapshot, MemoryUsage, ModelGeometry, ServerSnapshot,
    WatchRequest, WeightUpload,
};
use super::{MdmaService, MemoryDomain};
use crate::backend::Backend;
use crate::federation::tls::ClientSpki;
use crate::model::Model;
use crate::registry::{Kind, Phase};
use rama::extensions::{Extensions, ExtensionsRef, Ingress as TransportIngress};
use rama::http::grpc::{Request, Status};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

pub(crate) type SnapshotStream =
    Pin<Box<dyn Stream<Item = Result<ServerSnapshot, Status>> + Send + Sync + 'static>>;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
const MIN_INTERVAL_MS: u32 = 250;
const MAX_INTERVAL_MS: u32 = 60_000;
const HEARTBEAT: Duration = Duration::from_secs(30);

pub(super) fn interval(requested_ms: u32) -> Duration {
    if requested_ms == 0 {
        return DEFAULT_INTERVAL;
    }
    Duration::from_millis(u64::from(
        requested_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS),
    ))
}

fn saturating<T: TryInto<u32>>(n: T) -> u32 {
    n.try_into().unwrap_or(u32::MAX)
}

pub(super) fn geometry<B: Backend>(backend: &B, model: &Model<B>) -> Option<ModelGeometry> {
    let config = model.config();
    Some(ModelGeometry {
        n_layers: saturating(config.n_layers),
        n_embd: saturating(config.n_embd),
        n_ff: saturating(config.n_ff),
        n_heads: saturating(config.n_heads),
        n_kv_heads: saturating(config.n_kv_heads),
        head_dim: saturating(config.head_dim),
        // What one position costs while every layer still grows. Sliding-window
        // layers stop at their window, so this bounds longer contexts from above.
        // Recurrent state is fixed, not per position.
        kv_bytes_per_token: (model.kv_bytes(backend, 1).ok()?
            - model.kv_states().ok()?.iter().sum::<usize>()) as u64,
        layer_kv_widths: model
            .kv_widths()
            .ok()?
            .into_iter()
            .map(saturating)
            .collect(),
    })
}

impl<B> MdmaService<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    pub(super) fn memory_snapshot(&self) -> MemorySnapshot {
        let budget = self
            .memory
            .as_ref()
            .map(crate::memory::NativeMemoryBudget::snapshot)
            .unwrap_or_default();
        let buffers = self.backend.buffer_budget();
        let usage = |domain| {
            buffers.map(|b| MemoryUsage {
                used_bytes: b.used(domain) as u64,
                limit_bytes: b.limit(domain).unwrap_or(0) as u64,
            })
        };
        // Aliases share one loaded model.
        let mut uploads: Vec<(usize, WeightUpload)> = Vec::new();
        for entry in self.registry.entries() {
            if entry.phase != Phase::Resident || entry.info.kind != Kind::Gguf {
                continue;
            }
            if let Some((_, upload)) = uploads.iter_mut().find(|(slot, _)| *slot == entry.upload) {
                upload.models.push(entry.name.to_owned());
                continue;
            }
            let reserved = self.registry.resident(entry.name).and_then(|loaded| {
                loaded
                    .decoder()
                    .and_then(|decoder| decoder.model.weight_reservation_bytes())
            });
            let upload = WeightUpload {
                upload_id: saturating(uploads.len()),
                bytes: reserved.map_or(entry.info.disk_bytes, |bytes| bytes as u64),
                reserved: reserved.is_some(),
                models: vec![entry.name.to_owned()],
            };
            uploads.push((entry.upload, upload));
        }
        MemorySnapshot {
            limit_bytes: if budget.limit == usize::MAX {
                0
            } else {
                budget.limit as u64
            },
            used_bytes: budget.used as u64,
            weights_bytes: budget.weights as u64,
            kv_bytes: budget.kv as u64,
            workspace_bytes: budget.workspace as u64,
            device_buffers: usage(MemoryDomain::Device),
            host_buffers: usage(MemoryDomain::Host),
            device: Some(DeviceMemory {
                name: self
                    .backend
                    .device_label()
                    .unwrap_or_else(|| self.backend_name.to_owned()),
                unified_memory: self.backend.unified_memory(),
                available_bytes: self
                    .backend
                    .available_device_bytes()
                    .ok()
                    .flatten()
                    .map(|bytes| bytes as u64),
            }),
            weights: uploads.into_iter().map(|(_, upload)| upload).collect(),
            active_requests: saturating(self.active.load(std::sync::atomic::Ordering::Relaxed)),
        }
    }

    /// Topology for operator pins only.
    pub(super) fn federation<T>(&self, req: &Request<T>) -> FederationTopology {
        let caller = req
            .extensions()
            .get_ref::<TransportIngress<Extensions>>()
            .and_then(|ingress| ingress.get_ref::<ClientSpki>());
        if !caller.is_some_and(|spki| self.operators.contains(&spki.0)) {
            return FederationTopology::default();
        }
        self.topology.as_ref().map_or_else(
            || FederationTopology {
                authorized: true,
                ..FederationTopology::default()
            },
            |topology| topology.snapshot(),
        )
    }
}

pub(super) fn watch<B>(service: MdmaService<B>, req: Request<WatchRequest>) -> SnapshotStream
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    let every = interval(req.get_ref().interval_ms);
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut power = service.power.as_ref().map(|(rx, _)| rx.clone());
        let mut trust = service
            .topology
            .as_ref()
            .map(|topology| topology.subscribe());
        let mut models = service.registry.subscribe();
        let shutdown = service.shutdown.clone();
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (mut last, mut sent) = (None::<ServerSnapshot>, Instant::now());
        loop {
            let snapshot = ServerSnapshot {
                status: Some(service.status(&req)),
                memory: Some(service.memory_snapshot()),
                federation: Some(service.federation(&req)),
            };
            if last.as_ref() != Some(&snapshot) || sent.elapsed() >= HEARTBEAT {
                if tx.send(Ok(snapshot.clone())).await.is_err() {
                    return;
                }
                (last, sent) = (Some(snapshot), Instant::now());
            }
            tokio::select! {
                () = tx.closed() => return,
                () = async {
                    match &shutdown {
                        Some(guard) => guard.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => return,
                _ = ticker.tick() => {}
                result = models.changed() => if result.is_err() { return },
                () = async {
                    let Some(rx) = trust.as_mut() else { return std::future::pending().await };
                    if rx.changed().await.is_err() { std::future::pending::<()>().await }
                } => {}
                () = async {
                    let Some(rx) = power.as_mut() else { return std::future::pending().await };
                    if rx.changed().await.is_err() { std::future::pending::<()>().await }
                } => {}
            }
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

#[cfg(test)]
mod tests;
