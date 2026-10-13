//! Federation peers, grants, catalogs and routes, for operators.
use super::catalog_cache::Cache;
use super::config::Operation;
use super::routing::selection::Route;
use super::trust::{State, Store};
use crate::grpc::{CatalogModel, CatalogView, FederationPeer, FederationRoute, FederationTopology};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

pub(crate) struct Topology {
    store: Arc<Store>,
    cache: Option<Cache>,
    routes: Vec<Route>,
}

const fn state(state: State) -> &'static str {
    match state {
        State::Staged => "staged",
        State::Active => "active",
        State::Suspended => "suspended",
        State::Revoked => "revoked",
    }
}

const fn operation(operation: Operation) -> &'static str {
    match operation {
        Operation::Synthesize => "synthesize",
        Operation::Decide => "decide",
    }
}

impl Topology {
    pub(crate) const fn new(store: Arc<Store>, cache: Option<Cache>, routes: Vec<Route>) -> Self {
        Self {
            store,
            cache,
            routes,
        }
    }

    /// Ticks on every trust-state change.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.store.subscribe()
    }

    pub(crate) fn snapshot(&self) -> FederationTopology {
        let Ok(snapshot) = self.store.snapshot() else {
            return FederationTopology {
                authorized: true,
                enabled: true,
                ..FederationTopology::default()
            };
        };
        let now_ms = super::tls::utc_ms().unwrap_or(0);
        let now = Instant::now();
        let peers = snapshot
            .records()
            .map(|record| {
                let peer = &record.policy;
                let body = self.cache.as_ref().and_then(|cache| {
                    cache
                        .snapshot(&self.store, &peer.server_id, now_ms, now)
                        .ok()
                        .flatten()
                });
                let host = body.as_ref().map(|body| body.host.clone());
                let catalog = body.map(|body| CatalogView {
                    issued_at_ms: body.issued_at_ms,
                    expires_at_ms: body.expires_at_ms,
                    server_state: body.server_state.as_str().into(),
                    models: body
                        .models
                        .into_iter()
                        .map(|m| CatalogModel {
                            model: m.model,
                            architecture: m.architecture,
                            backend: m.backend,
                            ready: m.ready,
                            class: m.class.as_str().into(),
                            capabilities: crate::registry::Capability::names(&m.capabilities),
                            resident: m.resident,
                            parameters: m.parameters,
                            quantization: m.quantization,
                            trained_ctx: m.trained_ctx,
                            chat_format: m
                                .chat_format
                                .map_or_else(String::new, |f| f.name().into()),
                            decision: m.decision.map_or_else(String::new, |c| c.as_str().into()),
                        })
                        .collect(),
                });
                FederationPeer {
                    peer_id: peer.server_id.clone(),
                    endpoint: peer.endpoint.clone(),
                    state: state(record.state).into(),
                    catalog_exports: peer.catalog_exports.clone(),
                    execute_exports: peer
                        .execute_exports
                        .iter()
                        .map(|e| e.model.clone())
                        .collect(),
                    route_imports: peer.route_imports.iter().map(|r| r.model.clone()).collect(),
                    catalog,
                    host,
                }
            })
            .collect();
        FederationTopology {
            authorized: true,
            enabled: true,
            server_id: snapshot.server_id.clone(),
            trust_generation: snapshot.generation,
            peers,
            routes: self
                .routes
                .iter()
                .map(|r| FederationRoute {
                    alias: r.alias.clone(),
                    peer_id: r.peer_id.clone(),
                    model: r.model.clone(),
                    operation: operation(r.operation).into(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
