//! Opt-in transport acceptance against separately launched real `mdma-serve`
//! processes. No fixed addresses, shared private peer bundles, or model grants.
use super::*;
use crate::federation::{
    catalog,
    control::{FederationClient, IdentifyRequest, ScanCapabilitiesRequest},
};
use rama::http::{
    Body,
    grpc::{Code, Status},
};
use std::assert_matches;
use std::time::Duration;

fn secret(path: &Path, bytes: impl AsRef<[u8]>) {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).unwrap();
    file.write_all(bytes.as_ref()).unwrap();
    file.sync_all().unwrap();
}

fn named_leaf(
    id: &str,
    issuer: &CertifiedIssuer<'_, KeyPair>,
) -> (CertificateDer<'static>, String, String) {
    named_leaf_with_names(id, issuer, vec![id.into()])
}

fn named_leaf_with_names(
    id: &str,
    issuer: &CertifiedIssuer<'_, KeyPair>,
    names: Vec<String>,
) -> (CertificateDer<'static>, String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(names).unwrap();
    params.subject_alt_names.push(SanType::URI(
        format!("https://{id}/.well-known/mdma/identity")
            .try_into()
            .unwrap(),
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let cert = params.signed_by(&key, issuer).unwrap();
    (cert.der().clone(), cert.pem(), key.serialize_pem())
}

#[test]
#[ignore = "operator-only fresh private fixture provisioning; requires MDMA_TEST_FEDERATION_DIR and A/B_ENDPOINT"]
fn provision() {
    let root = PathBuf::from(std::env::var("MDMA_TEST_FEDERATION_DIR").unwrap());
    // No replacement of an existing deployment/trust store.
    std::fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut endpoints = vec![
        std::env::var("MDMA_TEST_FEDERATION_A_ENDPOINT").unwrap(),
        std::env::var("MDMA_TEST_FEDERATION_B_ENDPOINT").unwrap(),
    ];
    match std::env::var("MDMA_TEST_FEDERATION_C_ENDPOINT") {
        Ok(endpoint) => endpoints.push(endpoint),
        Err(std::env::VarError::NotPresent) => {}
        Err(error) => panic!("invalid third test endpoint: {error}"),
    }
    let issuers: Vec<_> = endpoints.iter().map(|_| ca()).collect();
    let client_issuer = ca();
    let ids = ["node-a.mdma.test", "node-b.mdma.test", "node-c.mdma.test"];
    let names = ["node-a", "node-b", "node-c"];
    let leaves: Vec<_> = issuers
        .iter()
        .enumerate()
        .map(|(i, ca)| named_leaf(ids[i], ca))
        .collect();
    let signers: Vec<_> = endpoints
        .iter()
        .map(|_| KeyPairHandle::generate().unwrap())
        .collect();
    let now = tls::utc_ms().unwrap();
    let duration =
        std::env::var("MDMA_TEST_TRUST_WINDOW_MS").map_or(900_000, |v| v.parse::<u64>().unwrap());
    assert!((1..=86_400_000).contains(&duration));
    let window = Window {
        valid_from_ms: now - 60_000,
        valid_until_ms: now + duration,
    };
    for i in 0..endpoints.len() {
        let dir = root.join(names[i]);
        std::fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let own_root = CaRoot {
            path: "own-root.pem".into(),
            certificate_sha256: tls::sha256(issuers[i].der()),
        };
        secret(&dir.join("own-root.pem"), issuers[i].pem());
        secret(&dir.join("tls.pem"), &leaves[i].1);
        secret(&dir.join("tls-key.pem"), &leaves[i].2);
        secret(&dir.join("catalog.pk8"), signers[i].to_pkcs8().unwrap());
        let endpoint: rama::net::uri::Uri = endpoints[i].parse().unwrap();
        let (_, legacy_cert, legacy_key) = named_leaf_with_names(
            ids[i],
            &client_issuer,
            vec![
                ids[i].into(),
                endpoint.host().unwrap().to_string(),
                "localhost".into(),
                "127.0.0.1".into(),
            ],
        );
        secret(&dir.join("client-server.pem"), legacy_cert);
        secret(&dir.join("client-server-key.pem"), legacy_key);
        secret(&dir.join("client-root.pem"), client_issuer.pem());
        let peers: Vec<_> = (0..endpoints.len())
            .filter(|other| *other != i)
            .map(|other| {
                let path = PathBuf::from(format!("peer-{other}-root.pem"));
                secret(&dir.join(&path), issuers[other].pem());
                let peer_root = CaRoot {
                    path,
                    certificate_sha256: tls::sha256(issuers[other].der()),
                };
                let peer = config::Peer {
                    server_id: ids[other].into(),
                    endpoint: endpoints[other].clone(),
                    tls_name: ids[other].into(),
                    ca_roots: vec![peer_root],
                    tls_pins: [config::TlsRole::ClientAuth, config::TlsRole::ServerAuth]
                        .into_iter()
                        .map(|role| config::TlsPin {
                            role,
                            spki_sha256: tls::certificate_spki_sha256(&leaves[other].0).unwrap(),
                            window,
                        })
                        .collect(),
                    signing_keys: vec![SigningKey {
                        key_id: format!("ed25519:fleet-{other}"),
                        public_key: URL_SAFE_NO_PAD.encode(signers[other].public_key_bytes()),
                        window,
                    }],
                    control_permissions: config::ControlPermissions::default(),
                    catalog_exports: vec![],
                    execute_exports: vec![],
                    route_imports: vec![],
                    install: None,
                    installed_exports: false,
                    limits: config::Limits::default(),
                };
                peer.validate().unwrap();
                peer
            })
            .collect();
        // Independent initial test-client state, not a backup/rollback of a
        // running production store. Each process keeps its exclusive writer.
        for filename in ["trust.json", "probe-trust.json"] {
            let store = Store::initialize(&dir.join(filename), ids[i]).unwrap();
            for peer in &peers {
                store.stage(peer.clone()).unwrap();
                store
                    .activate_control(
                        &peer.server_id,
                        ControlPermissions {
                            identify: true,
                            scan_capabilities: true,
                        },
                    )
                    .unwrap();
            }
            store
                .approve_local_signer(SigningKey {
                    key_id: format!("ed25519:fleet-{i}"),
                    public_key: URL_SAFE_NO_PAD.encode(signers[i].public_key_bytes()),
                    window,
                })
                .unwrap();
        }
        let endpoint: rama::net::uri::Uri = endpoints[i].parse().unwrap();
        let config = Config {
            server_id: ids[i].into(),
            listen: format!("0.0.0.0:{}", endpoint.port_u16().unwrap())
                .parse()
                .unwrap(),
            trust_state: "trust.json".into(),
            tls_name: ids[i].into(),
            tls_cert: "tls.pem".into(),
            tls_key: "tls-key.pem".into(),
            identity_roots: vec![own_root],
            signer_id: format!("ed25519:fleet-{i}"),
            signer_key: "catalog.pk8".into(),
            endpoint: None,
            poll_catalogs: true,
            enable_execution: false,
            enable_routing: false,
            routes: vec![],
        };
        secret(&dir.join("runtime.toml"), toml::to_string(&config).unwrap());
    }
    println!(
        "Provisioned {} isolated control-only test identities; policy window {duration}ms.",
        endpoints.len()
    );
}

#[test]
#[ignore = "operator-only generated native fixtures; requires a fresh MDMA_TEST_NATIVE_FIXTURES_DIR"]
fn native_fixtures() {
    let root = PathBuf::from(std::env::var("MDMA_TEST_NATIVE_FIXTURES_DIR").unwrap());
    std::fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let gguf = crate::gguf::tests::Fixture::new(1, 8)
        .with_tokenizer()
        .write();
    secret(
        &root.join("native.gguf"),
        std::fs::read(gguf.path()).unwrap(),
    );
    let laya = crate::laya::compat_tests::checkpoint();
    std::fs::create_dir(root.join("laya")).unwrap();
    std::fs::create_dir(root.join("laya/encoder")).unwrap();
    std::fs::create_dir(root.join("laya/tokenizer")).unwrap();
    for path in [
        "model.safetensors",
        "rl_agent_config.json",
        "encoder/config.json",
        "tokenizer/tokenizer.json",
        "tokenizer/tokenizer_config.json",
    ] {
        secret(
            &root.join("laya").join(path),
            std::fs::read(laya.path().join(path)).unwrap(),
        );
    }
    println!("Generated native GGUF and Laya acceptance fixtures, not production models.");
}

#[tokio::test]
#[ignore = "physical native catalog capture; requires fresh probe state, MDMA_TEST_PEER and MDMA_TEST_ROUTES_OUT"]
async fn native_snapshot() {
    use crate::federation::{catalog_cache::Cache, config::Operation, routing::Route};
    let cfg = Config::load(Path::new("runtime.toml")).unwrap();
    let store = Arc::new(Store::open(Path::new("probe-trust.json"), &cfg.server_id).unwrap());
    let own_key = store
        .local_signer(&cfg.signer_id, tls::utc_ms().unwrap())
        .unwrap()
        .bytes()
        .unwrap();
    let chain = CertificateDer::pem_file_iter(&cfg.tls_cert)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let credentials = super::super::outbound::Credentials {
        store,
        chain: chain.clone(),
        key: Arc::new(PrivateKeyDer::from_pem_file(&cfg.tls_key).unwrap()),
        signing_key: own_key,
        local_expires_ms: tls::local_client_expiry(
            &chain,
            &cfg.local_policy(),
            &own_key,
            tls::utc_ms().unwrap(),
        )
        .unwrap(),
    };
    let peer = std::env::var("MDMA_TEST_PEER").unwrap();
    let body = credentials
        .poll(&Cache::default(), &peer, Executor::default())
        .await
        .unwrap();
    assert!(!body.server_state.forces_unready());
    assert_eq!(body.models.len(), 2);
    let backend = std::env::var("MDMA_TEST_NATIVE_BACKEND").unwrap_or_else(|_| "cpu".into());
    assert_matches!(
        backend.as_str(),
        "cpu" | "cuda" | "metal" | "rocm" | "vulkan"
    );
    let routes: Vec<_> = body
        .models
        .into_iter()
        .map(|d| {
            assert!(d.ready);
            assert_eq!(
                d.backend,
                if d.kind == catalog::Kind::Laya {
                    "cpu"
                } else {
                    &backend
                }
            );
            assert_matches!(d.model.as_str(), "native" | "laya");
            println!(
                "Verified {} {} {} {} {}",
                d.model, d.backend, d.architecture, d.artifact_id, d.profile_id
            );
            Route {
                alias: format!(
                    "{}-{}",
                    std::env::var("MDMA_TEST_ROUTE_PREFIX").unwrap_or_else(|_| "remote".into()),
                    d.model
                ),
                peer_id: peer.clone(),
                model: d.model,
                artifact_id: d.artifact_id,
                profile_id: d.profile_id,
                operation: match d.kind {
                    catalog::Kind::Gguf => Operation::Synthesize,
                    catalog::Kind::Laya => Operation::Decide,
                },
            }
        })
        .collect();
    secret(
        Path::new(&std::env::var("MDMA_TEST_ROUTES_OUT").unwrap()),
        serde_json::to_vec_pretty(&routes).unwrap(),
    );
    println!(
        "Verified actual signed native catalog; captured exact routes for offline operator review."
    );
}

#[tokio::test]
#[ignore = "physical GPU executor negative/drop acceptance; requires reviewed peer and expected GPU backend"]
async fn gpu_execution_probe() {
    use crate::federation::{
        catalog_cache::Cache,
        control::{ExecutionEnvelope, SynthesizeRequest, synthesize_event::Event},
        trust::Action,
    };
    use rama::http::grpc::Request;
    use tokio_stream::StreamExt;
    let cfg = Config::load(Path::new("runtime.toml")).unwrap();
    let store = Arc::new(Store::open(Path::new("probe-trust.json"), &cfg.server_id).unwrap());
    let own_key = store
        .local_signer(&cfg.signer_id, tls::utc_ms().unwrap())
        .unwrap()
        .bytes()
        .unwrap();
    let chain = CertificateDer::pem_file_iter(&cfg.tls_cert)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let credentials = super::super::outbound::Credentials {
        store,
        chain: chain.clone(),
        key: Arc::new(PrivateKeyDer::from_pem_file(&cfg.tls_key).unwrap()),
        signing_key: own_key,
        local_expires_ms: tls::local_client_expiry(
            &chain,
            &cfg.local_policy(),
            &own_key,
            tls::utc_ms().unwrap(),
        )
        .unwrap(),
    };
    let peer = std::env::var("MDMA_TEST_PEER").unwrap();
    let backend = std::env::var("MDMA_TEST_NATIVE_BACKEND").unwrap();
    assert_matches!(backend.as_str(), "cuda" | "metal" | "rocm" | "vulkan");
    let catalog = credentials
        .poll(&Cache::default(), &peer, Executor::default())
        .await
        .unwrap();
    let descriptor = catalog.models.iter().find(|d| d.model == "native").unwrap();
    assert!(descriptor.ready && descriptor.backend == backend);
    let message = || {
        let mut id = vec![0; 16];
        aws_lc_rs::rand::fill(&mut id).unwrap();
        SynthesizeRequest {
            envelope: Some(ExecutionEnvelope {
                origin: cfg.server_id.clone(),
                destination: peer.clone(),
                request_id: id,
                model: descriptor.model.clone(),
                artifact_id: descriptor.artifact_id.clone(),
                profile_id: descriptor.profile_id.clone(),
                timeout_ms: 10_000,
            }),
            prompt: "Hello".into(),
            max_tokens: 8,
            sampler: Some(crate::federation::control::SamplerConfig {
                temperature: Some(0.),
                ..Default::default()
            }),
        }
    };
    for field in ["artifact", "profile"] {
        let connected = credentials
            .connect(&peer, Action::Authenticate, Executor::default())
            .await
            .unwrap();
        let mut changed = message();
        let envelope = changed.envelope.as_mut().unwrap();
        if field == "artifact" {
            envelope.artifact_id = format!("sha256:{}", "0".repeat(64));
        } else {
            envelope.profile_id = format!("sha256:{}", "0".repeat(64));
        }
        let call = async {
            let mut request = Request::new(changed);
            request.try_set_timeout(Duration::from_secs(10)).unwrap();
            let error = connected.client.synthesize(request).await.err().unwrap();
            assert_eq!(error.code(), Code::FailedPrecondition, "{field}");
        };
        tokio::select! {
            () = call => {},
            () = connected.drive => panic!("GPU negative probe disconnected"),
            () = tokio::time::sleep(Duration::from_secs(10)) => panic!("GPU negative probe timed out"),
        }
    }
    {
        let connected = credentials
            .connect(&peer, Action::Authenticate, Executor::default())
            .await
            .unwrap();
        let call = async {
            let mut request = Request::new(message());
            request.try_set_timeout(Duration::from_secs(10)).unwrap();
            let mut stream = connected
                .client
                .synthesize(request)
                .await
                .unwrap()
                .into_inner();
            assert_matches!(
                stream.next().await.unwrap().unwrap().event,
                Some(Event::Accepted(_))
            );
            assert_matches!(
                stream.next().await.unwrap().unwrap().event,
                Some(Event::Token(_))
            );
            drop(stream); // Cancel/RST; the owned connection below is dropped too.
        };
        tokio::select! {
            () = call => {},
            () = connected.drive => panic!("GPU drop probe disconnected prematurely"),
            () = tokio::time::sleep(Duration::from_secs(10)) => panic!("GPU drop probe timed out"),
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    // A new, independent acceptance request, not a retry of canceled work.
    let connected = credentials
        .connect(&peer, Action::Authenticate, Executor::default())
        .await
        .unwrap();
    let call = async {
        let mut next = message();
        next.max_tokens = 1;
        let mut request = Request::new(next.clone());
        request.try_set_timeout(Duration::from_secs(10)).unwrap();
        let mut stream = connected
            .client
            .synthesize(request)
            .await
            .unwrap()
            .into_inner();
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            assert!(
                events.len() < 3,
                "max-one-token probe cannot collect extra events"
            );
            events.push(event.unwrap());
        }
        crate::federation::routing::tests::validate_probe(&next, descriptor, &events).unwrap();
    };
    tokio::select! {
        () = call => {},
        () = connected.drive => panic!("GPU independent probe disconnected"),
        () = tokio::time::sleep(Duration::from_secs(10)) => panic!("GPU independent probe timed out"),
    }
    println!(
        "Verified actual {backend} artifact/profile denial, stream drop and independent joined completion."
    );
}

struct Transport(
    rama::service::BoxService<
        rama::http::Request,
        rama::http::Response,
        rama::error::extra::OpaqueError,
    >,
);

impl rama::Service<rama::http::Request> for Transport {
    type Output = rama::http::Response;
    type Error = Status;
    async fn serve(&self, request: rama::http::Request) -> Result<Self::Output, Self::Error> {
        self.0
            .serve(request)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))
    }
}

#[tokio::test]
#[ignore = "physical-host control acceptance; requires cwd containing fresh provisioned runtime.toml"]
async fn probe() {
    let cfg = Config::load(Path::new("runtime.toml")).unwrap();
    let store = Arc::new(Store::open(Path::new("probe-trust.json"), &cfg.server_id).unwrap());
    let snapshot = store.snapshot().unwrap();
    assert!((1..=2).contains(&snapshot.records().count()));
    for record in snapshot.records() {
        probe_peer(&cfg, &store, &record.policy).await;
    }
}

async fn probe_peer(cfg: &Config, store: &Arc<Store>, peer: &config::Peer) {
    use rama::http::client::EasyHttpConnectorBuilder;
    use rama::tls::{client::TlsClientConfig, rustls::client::RustlsClientConfigExt};
    assert!(
        peer.catalog_exports.is_empty()
            && peer.execute_exports.is_empty()
            && peer.route_imports.is_empty()
    );
    let own_key = store
        .local_signer(&cfg.signer_id, tls::utc_ms().unwrap())
        .unwrap()
        .bytes()
        .unwrap();
    let chain = CertificateDer::pem_file_iter(&cfg.tls_cert)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(&cfg.tls_key).unwrap();
    let credentials = super::super::outbound::Credentials {
        store: store.clone(),
        chain: chain.clone(),
        key: Arc::new(key.clone_key()),
        signing_key: own_key,
        local_expires_ms: tls::local_client_expiry(
            &chain,
            &cfg.local_policy(),
            &own_key,
            tls::utc_ms().unwrap(),
        )
        .unwrap(),
    };
    let cache = crate::federation::catalog_cache::Cache::default();
    let first = credentials
        .poll(&cache, &peer.server_id, Executor::default())
        .await
        .unwrap();
    assert!(first.models.is_empty());
    assert_eq!(
        cache
            .snapshot(
                store,
                &peer.server_id,
                tls::utc_ms().unwrap(),
                std::time::Instant::now()
            )
            .unwrap()
            .unwrap()
            .nonce,
        first.nonce
    );
    let second = credentials
        .poll(&cache, &peer.server_id, Executor::default())
        .await
        .unwrap();
    assert_ne!(first.nonce, second.nonce);
    let client_config =
        tls::client_config(store.clone(), peer.server_id.clone(), chain, key, &own_key).unwrap();
    let tls = TlsClientConfig::new()
        .with_server_name(rama::net::address::Host::try_from(peer.tls_name.clone()).unwrap())
        .with_modify_rustls_config(move |_| Ok(client_config.clone()));
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector::<Body>(Executor::default())
        .without_connection_pool()
        .build_client::<Body, Body, _>();
    let client = FederationClient::new(
        Transport(rama::service::BoxService::new(connector)),
        peer.endpoint.parse().unwrap(),
    );
    let identify = IdentifyRequest {
        origin: cfg.server_id.clone(),
        destination: peer.server_id.clone(),
        supported_major_versions: vec![1],
    };
    let result = tokio::time::timeout(Duration::from_secs(2), client.identify(identify.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.into_inner().server_id, peer.server_id);
    let mut nonce = [0; 32];
    aws_lc_rs::rand::fill(&mut nonce).unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        client.scan_capabilities(ScanCapabilitiesRequest {
            origin: cfg.server_id.clone(),
            destination: peer.server_id.clone(),
            major_version: 1,
            nonce: nonce.to_vec(),
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .into_inner();
    let signing = store
        .signing_key(&peer.server_id, &reply.key_id, tls::utc_ms().unwrap())
        .unwrap();
    let envelope = catalog::SignedCatalogEnvelope {
        key_id: reply.key_id,
        body_json: reply.body_json,
        signature: reply.signature.try_into().unwrap(),
    };
    let context = catalog::VerifyContext {
        now_ms: tls::utc_ms().unwrap(),
        expected_origin: &peer.server_id,
        expected_destination: &cfg.server_id,
        expected_nonce: &nonce,
        key_valid_from_ms: signing.window.valid_from_ms,
        key_valid_until_ms: signing.window.valid_until_ms,
    };
    let body =
        catalog::verify_signed_catalog(&envelope, &signing.bytes().unwrap(), &context).unwrap();
    assert!(body.models.is_empty());
    let wrong = catalog::VerifyContext {
        expected_nonce: &[0; 32],
        ..context
    };
    assert!(catalog::verify_signed_catalog(&envelope, &signing.bytes().unwrap(), &wrong).is_err());
    let mut forged = identify;
    forged.origin = "forged.mdma.test".into();
    let error = tokio::time::timeout(Duration::from_secs(2), client.identify(forged))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), Code::Unauthenticated);
    println!(
        "Verified {} -> {}: completed outbound mTLS binding, verified cache refresh, signed empty catalog, wrong-nonce and forged-origin rejection; zero workload grants.",
        cfg.server_id, peer.server_id
    );
}
