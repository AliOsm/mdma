//! Opt-in control, native execution and client routing integration. No implicit
//! enrollment, model grants, recovery or workload activation.

use super::{
    config,
    control::{ControlService, Signer},
    signing::KeyPairHandle,
    tls,
    trust::Store,
};
use rama::tcp::server::TcpListener;
use rama::tls::rustls::{
    dep::rustls,
    server::{RustlsServerConfigExt, TlsAcceptorLayer},
};
use rama::tls::server::TlsServerConfig;
use rama::{Layer, error::BoxError, http::server::HttpServer, rt::Executor};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

mod client;
pub(crate) mod outbound;
mod polling;
pub use super::routing::Route;
pub use client::{ClientConnections, ClientGate, ClientPolicy};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server_id: String,
    pub listen: SocketAddr,
    pub trust_state: PathBuf,
    pub tls_name: String,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    /// Reviewed local issuer DER fingerprints, not a remotely discovered CA.
    pub identity_roots: Vec<config::CaRoot>,
    pub signer_id: String,
    /// DER PKCS#8; private key material is never part of a peer bundle.
    pub signer_key: PathBuf,
    /// Where peers reach this node, as exported in its peer bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Explicit outbound control-only polling; never enrolls or grants routes.
    #[serde(default)]
    pub poll_catalogs: bool,
    /// Both flags are independent, explicit and false by default.
    #[serde(default)]
    pub enable_execution: bool,
    #[serde(default)]
    pub enable_routing: bool,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn read(path: &Path, max: usize) -> Result<Vec<u8>, BoxError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err("federation file exceeds size limit".into());
    }
    Ok(bytes)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, BoxError> {
        let config: Self = toml::from_str(std::str::from_utf8(&read(path, 64 * 1024)?)?)?;
        super::server_id::validate(&config.server_id)?;
        if config.listen.port() == 0 {
            return Err("federation listen port must be explicit".into());
        }
        for path in [
            &config.trust_state,
            &config.tls_cert,
            &config.tls_key,
            &config.signer_key,
        ] {
            if path.as_os_str().is_empty() {
                return Err("empty federation file path".into());
            }
        }
        Ok(config)
    }

    pub(crate) fn local_policy(&self) -> config::Peer {
        config::Peer {
            server_id: self.server_id.clone(),
            endpoint: self
                .endpoint
                .clone()
                .unwrap_or_else(|| format!("https://{}", self.listen)),
            tls_name: self.tls_name.clone(),
            ca_roots: self.identity_roots.clone(),
            tls_pins: vec![],
            signing_keys: vec![],
            control_permissions: config::ControlPermissions::default(),
            catalog_exports: vec![],
            execute_exports: vec![],
            route_imports: vec![],
            install: None,
            installed_exports: false,
            limits: config::Limits::default(),
        }
    }
}

/// Kept private: callers cannot substitute a credential expiry or trust store
/// unrelated to the TLS configuration.
pub struct Prepared {
    listen: SocketAddr,
    store: Arc<Store>,
    config: rustls::ServerConfig,
    local_expires_ms: u64,
    service: ControlService,
    polling: Option<polling::Poller>,
    routing: Option<(
        outbound::Credentials,
        super::catalog_cache::Cache,
        Vec<Route>,
    )>,
    execution_enabled: bool,
    /// A peer may push models here.
    accepts_installs: bool,
    integrated: bool,
    client_policy: ClientPolicy,
}

impl Prepared {
    pub fn load(
        config: Config,
        client_listen: SocketAddr,
        client_roots: &[CertificateDer<'_>],
        client_chain: &[CertificateDer<'_>],
    ) -> Result<Self, BoxError> {
        if config.listen == client_listen {
            return Err("federation requires a separate listener".into());
        }
        // Open only. A missing/corrupt state must never enroll or recover peers.
        let store = Arc::new(Store::open(&config.trust_state, &config.server_id)?);
        let snapshot = store.snapshot()?;
        let mut policies: Vec<_> = snapshot.records().map(|r| r.policy.clone()).collect();
        if policies
            .iter()
            .any(|p| !p.execute_exports.is_empty() || p.installed_exports)
            && !config.enable_execution
        {
            return Err("execute grants require explicit enable_execution".into());
        }
        let accepts_installs = policies.iter().any(|p| p.install.is_some());
        if policies.iter().any(|p| !p.route_imports.is_empty()) && !config.enable_routing {
            return Err("import grants require explicit enable_routing".into());
        }
        if config.enable_routing && (!config.poll_catalogs || config.routes.is_empty())
            || !config.enable_routing && !config.routes.is_empty()
            || config.routes.len() > 256
        {
            return Err("routes require explicit enable_routing and catalog polling".into());
        }
        let mut aliases = std::collections::BTreeSet::new();
        for route in &config.routes {
            route.validate()?;
            if !aliases.insert(&route.alias) {
                return Err("duplicate route alias".into());
            }
            store.authorize(
                &route.peer_id,
                super::trust::Action::Import {
                    model: &route.model,
                    artifact_id: &route.artifact_id,
                    profile_id: &route.profile_id,
                    operation: route.operation,
                },
                tls::utc_ms()?,
            )?;
        }
        let client_policy = ClientPolicy {
            routing: config.enable_routing,
            expires_ms: if config.enable_routing {
                tls::listener_expiry(client_chain, client_roots, tls::utc_ms()?)?
            } else {
                0
            },
        };
        let local = config.local_policy();
        local.validate()?;
        policies.push(local.clone());
        tls::reject_client_root_overlap(&policies, client_roots)?;
        let chain = CertificateDer::pem_slice_iter(&read(&config.tls_cert, 512 * 1024)?)
            .collect::<Result<Vec<_>, _>>()?;
        let leaf = chain
            .first()
            .ok_or("federation certificate chain is empty")?;
        tls::reject_shared_leaf_key(leaf, client_chain.first().ok_or("client chain is empty")?)?;
        let key = PrivateKeyDer::from_pem_slice(&read(&config.tls_key, 128 * 1024)?)?;
        let signer = Signer::new(
            &store,
            config.signer_id,
            KeyPairHandle::from_pkcs8(&read(&config.signer_key, 16 * 1024)?)?,
        )?;
        let local_expires_ms =
            tls::local_server_expiry(&chain, &local, &signer.public_key(), tls::utc_ms()?)?;
        let polling = if config.poll_catalogs {
            let expires_ms =
                tls::local_client_expiry(&chain, &local, &signer.public_key(), tls::utc_ms()?)?
                    .min(local_expires_ms);
            Some(polling::Poller::new(outbound::Credentials {
                store: store.clone(),
                chain: chain.clone(),
                key: Arc::new(key.clone_key()),
                signing_key: signer.public_key(),
                local_expires_ms: expires_ms,
            }))
        } else {
            None
        };
        let routing = if config.enable_routing {
            let poller = polling.as_ref().ok_or("routing requires polling")?;
            Some((
                poller.credentials().clone(),
                poller.cache.clone(),
                config.routes,
            ))
        } else {
            None
        };
        let tls_config = tls::server_config(store.clone(), chain, key, &signer.public_key())?;
        let service = ControlService::new(store.clone(), signer);
        Ok(Self {
            listen: config.listen,
            store,
            config: tls_config,
            local_expires_ms,
            service,
            polling,
            routing,
            execution_enabled: config.enable_execution,
            accepts_installs,
            integrated: false,
            client_policy,
        })
    }

    /// Attach only the native service's actual loaded registry and live state.
    /// Explicit catalog disclosure never confers workload or routing grants.
    #[must_use]
    pub fn with_native_catalogs<B>(mut self, service: &crate::grpc::MdmaService<B>) -> Self
    where
        B: crate::backend::Backend + Send + Sync + 'static,
        B::Buf: Send + Sync,
    {
        self.service = self.service.with_inventory(service.federation_inventory());
        self
    }

    /// Enable only explicitly opted-in workload paths after native warm-up and
    /// actual executable/artifact profile capture. The default remains control.
    pub fn with_native_service<B>(
        mut self,
        service: &mut crate::grpc::MdmaService<B>,
        executor: Executor,
    ) -> Result<Self, BoxError>
    where
        B: crate::backend::Backend + Send + Sync + 'static,
        B::Buf: Send + Sync,
    {
        self.service = self
            .service
            .with_inventory(service.federation_inventory())
            .with_install(service.federation_folder());
        if self.execution_enabled {
            self.service = self.service.with_execution(service.federation_executor());
        }
        let cache = self.polling.as_ref().map(|poller| poller.cache.clone());
        let mut configured = Vec::new();
        if let Some((credentials, cache, routes)) = self.routing.take() {
            configured.clone_from(&routes);
            let router = super::routing::Router::new(
                routes,
                credentials,
                cache,
                executor,
                &service.federation_local_names(),
            )?;
            service.set_federation_router(router);
        }
        service.set_federation_topology(super::topology::Topology::new(
            self.store.clone(),
            cache,
            configured,
        ));
        self.integrated = true;
        Ok(self)
    }

    pub fn client_policy(&self) -> ClientPolicy {
        self.client_policy
    }
    /// Whether this node routes requests to its peers.
    pub fn routing_enabled(&self) -> bool {
        self.routing.is_some()
    }

    pub fn execution_enabled(&self) -> bool {
        self.execution_enabled
    }

    /// Whether a peer may push models here, so the node may start without any.
    pub fn accepts_installs(&self) -> bool {
        self.accepts_installs
    }

    pub async fn bind(&self, executor: Executor) -> Result<TcpListener, BoxError> {
        TcpListener::bind_address(self.listen.to_string(), executor).await
    }

    pub async fn serve(self, listener: TcpListener, executor: Executor) -> Result<(), BoxError> {
        if !self.integrated && (self.execution_enabled || self.client_policy.routing) {
            return Err("workloads require native service integration; control-only mode cannot execute or route".into());
        }
        let service = self.service.with_shutdown(
            executor
                .guard()
                .map(rama::graceful::ShutdownGuard::clone_weak),
        );
        let config = self.config;
        let tls = TlsServerConfig::new().with_modify_rustls_config(move |_| Ok(config.clone()));
        let mut http = HttpServer::new_h2(executor.clone());
        http.h2_mut().set_max_send_buf_size(1);
        http.h2_mut().set_max_concurrent_streams(2);
        http.h2_mut().set_max_header_list_size(16 * 1024);
        let service = TlsAcceptorLayer::new(tls).into_layer(tls::BindPeerService::new(
            self.store,
            self.local_expires_ms,
            http.service(super::execution::ingress::Gate::new(service.into_server())),
        ));
        let service = super::execution::ingress::Connections::new(service);
        let polls = async move {
            match self.polling {
                Some(poller) => poller.run(executor).await,
                None => std::future::pending::<Result<(), BoxError>>().await,
            }
        };
        tokio::select! {
            () = listener.serve(service) => Ok(()),
            result = polls => result,
        }
    }
}

#[cfg(test)]
mod tests;
