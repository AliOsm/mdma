use super::*;
use crate::federation::config::{CaRoot, ControlPermissions, SigningKey, Window};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose,
    IsCa, KeyPair, KeyUsagePurpose, SanType};

mod fleet;

fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

fn leaf(issuer: &CertifiedIssuer<'_, KeyPair>) -> (CertificateDer<'static>, String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["local.mdma.test".into()]).unwrap();
    params.subject_alt_names.push(SanType::URI(
        "https://local.mdma.test/.well-known/mdma/identity".try_into().unwrap(),
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.signed_by(&key, issuer).unwrap();
    (cert.der().clone(), cert.pem(), key.serialize_pem())
}

struct Fixture {
    dir: tempfile::TempDir,
    config: Config,
    client_root: CertificateDer<'static>,
    client_leaf: CertificateDer<'static>,
    fed_root: CertificateDer<'static>,
    fed_leaf: CertificateDer<'static>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let issuer = ca();
        let client_issuer = ca();
        let (fed_leaf, cert, key) = leaf(&issuer);
        let (client_leaf, _, _) = leaf(&client_issuer);
        let root_path = dir.path().join("root.pem");
        std::fs::write(&root_path, issuer.pem()).unwrap();
        let config = Config {
            server_id: "local.mdma.test".into(),
            listen: "127.0.0.1:50052".parse().unwrap(),
            tls_name: "local.mdma.test".into(),
            trust_state: dir.path().join("trust.json"),
            tls_cert: dir.path().join("tls.pem"),
            tls_key: dir.path().join("tls-key.pem"),
            identity_roots: vec![CaRoot {
                path: root_path, certificate_sha256: tls::sha256(issuer.der()),
            }],
            signer_id: "ed25519:runtime".into(),
            signer_key: dir.path().join("catalog.pk8"),
            endpoint: None,
            poll_catalogs: false,
            enable_execution: false,
            enable_routing: false,
            routes: vec![],
        };
        std::fs::write(&config.tls_cert, cert).unwrap();
        std::fs::write(&config.tls_key, key).unwrap();
        let store = Store::initialize(&config.trust_state, &config.server_id).unwrap();
        let mut peer = crate::federation::config::tests::peer();
        let now = tls::utc_ms().unwrap();
        for pin in &mut peer.tls_pins {
            pin.window = Window { valid_from_ms: now - 60_000, valid_until_ms: now + 300_000 };
        }
        for key in &mut peer.signing_keys {
            key.window = Window { valid_from_ms: now - 60_000, valid_until_ms: now + 300_000 };
        }
        peer.ca_roots = config.identity_roots.clone();
        store.stage(peer).unwrap();
        store.activate_control("remote.mdma.test", ControlPermissions {
            identify: true, scan_capabilities: true,
        }).unwrap();
        let signer = KeyPairHandle::generate().unwrap();
        store.approve_local_signer(SigningKey {
            key_id: config.signer_id.clone(),
            public_key: URL_SAFE_NO_PAD.encode(signer.public_key_bytes()),
            window: Window { valid_from_ms: now - 60_000, valid_until_ms: now + 300_000 },
        }).unwrap();
        std::fs::write(&config.signer_key, signer.to_pkcs8().unwrap()).unwrap();
        drop(store);
        Self { dir, config, client_root: client_issuer.der().clone(), client_leaf,
            fed_root: issuer.der().clone(), fed_leaf }
    }

    fn prepare(&self) -> Result<Prepared, BoxError> {
        // The temp directory remains alive throughout preparation.
        Prepared::load(self.config.clone(), "127.0.0.1:6362".parse().unwrap(),
            std::slice::from_ref(&self.client_root), std::slice::from_ref(&self.client_leaf))
    }
}

#[test]
fn explicitly_approved_control_only_state_prepares_strict_listener() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare().unwrap();
    drop(prepared); // Release the store lock before removing its temp directory.
}

#[test]
fn polling_is_explicit_and_requires_local_client_auth_under_the_pinned_issuer() {
    let mut fixture = Fixture::new();
    fixture.config.poll_catalogs = true;
    assert!(fixture.prepare().is_ok());
    // Keep the independently reviewed peer issuer in a separate file before
    // replacing only the local credential/issuer.
    let store = Store::open(&fixture.config.trust_state, &fixture.config.server_id).unwrap();
    let mut peer = store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    let peer_root = fixture.dir.path().join("peer-root.pem");
    std::fs::copy(&fixture.config.identity_roots[0].path, &peer_root).unwrap();
    peer.ca_roots[0].path = peer_root;
    store.approve_policy(peer).unwrap();
    drop(store);
    let issuer = ca();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["local.mdma.test".into()]).unwrap();
    params.subject_alt_names.push(SanType::URI(
        "https://local.mdma.test/.well-known/mdma/identity".try_into().unwrap(),
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params.signed_by(&key, &issuer).unwrap();
    std::fs::write(&fixture.config.tls_cert, cert.pem()).unwrap();
    std::fs::write(&fixture.config.tls_key, key.serialize_pem()).unwrap();
    fixture.config.identity_roots[0].certificate_sha256 = tls::sha256(issuer.der());
    std::fs::write(&fixture.config.identity_roots[0].path, issuer.pem()).unwrap();
    assert!(fixture.prepare().is_err());
    fixture.config.poll_catalogs = false;
    assert!(fixture.prepare().is_ok());
    fixture.config.poll_catalogs = true;
    assert!(fixture.prepare().is_err());
}

#[test]
fn missing_state_never_initializes_or_enrolls() {
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.config.trust_state).unwrap();
    let path = fixture.config.trust_state.clone();
    assert!(fixture.prepare().is_err());
    assert!(!path.exists());
}

#[test]
fn overlapping_local_or_peer_issuer_and_shared_leaf_key_fail() {
    let mut fixture = Fixture::new();
    fixture.client_root = fixture.fed_root.clone();
    assert!(fixture.prepare().is_err());
    let mut fixture = Fixture::new();
    fixture.client_leaf = fixture.fed_leaf.clone();
    assert!(fixture.prepare().is_err());
}

#[test]
fn local_issuer_and_selected_signer_material_must_match_reviewed_pins() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.config.signer_key,
        KeyPairHandle::generate().unwrap().to_pkcs8().unwrap()).unwrap();
    assert!(fixture.prepare().is_err());
    let mut fixture = Fixture::new();
    fixture.config.identity_roots[0].certificate_sha256 = tls::sha256(&fixture.client_root);
    assert!(fixture.prepare().is_err());
    let mut fixture = Fixture::new();
    fixture.config.tls_name = "wrong.mdma.test".into();
    assert!(fixture.prepare().is_err());
}

#[test]
fn separate_listener_and_no_execution_or_route_activation_are_required() {
    let mut fixture = Fixture::new();
    fixture.config.listen = "127.0.0.1:6362".parse().unwrap();
    assert!(fixture.prepare().is_err());
    let fixture = Fixture::new();
    let store = Store::open(&fixture.config.trust_state, &fixture.config.server_id).unwrap();
    let mut policy = store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    policy.catalog_exports.push("not-activated".into());
    store.approve_policy(policy).unwrap();
    drop(store);
    assert!(fixture.prepare().is_ok(), "explicit catalog-only policies are supported");
    let store = Store::open(&fixture.config.trust_state, &fixture.config.server_id).unwrap();
    let mut policy = store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    policy.execute_exports.push(crate::federation::config::ExecuteExport {
        model: "not-activated".into(),
        operation: crate::federation::config::Operation::Synthesize,
    });
    store.approve_policy(policy.clone()).unwrap();
    drop(store);
    assert!(fixture.prepare().is_err());
    let store = Store::open(&fixture.config.trust_state, &fixture.config.server_id).unwrap();
    policy.execute_exports.clear();
    policy.route_imports.push(crate::federation::config::RouteImport {
        model: "not-activated".into(),
        operation: crate::federation::config::Operation::Synthesize,
        artifact_id: format!("sha256:{}", "a".repeat(64)),
        profile_id: format!("sha256:{}", "b".repeat(64)),
    });
    store.approve_policy(policy).unwrap();
    drop(store);
    assert!(fixture.prepare().is_err());
}

#[test]
fn runtime_config_is_bounded_and_rejects_unknown_fields_or_zero_port() {
    let mut fixture = Fixture::new();
    let path = fixture.dir.path().join("runtime.toml");
    let text = toml::to_string(&fixture.config).unwrap();
    std::fs::write(&path, &text).unwrap();
    assert!(Config::load(&path).is_ok());
    std::fs::write(&path, text.replace("poll_catalogs = false\n", "")).unwrap();
    assert!(!Config::load(&path).unwrap().poll_catalogs);
    std::fs::write(&path, format!("auto_enroll = true\n{text}")).unwrap();
    assert!(Config::load(&path).is_err());
    fixture.config.listen.set_port(0);
    std::fs::write(&path, toml::to_string(&fixture.config).unwrap()).unwrap();
    assert!(Config::load(&path).is_err());
    std::fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(Config::load(&path).is_err());
}

#[tokio::test]
async fn explicit_execution_requires_actual_native_integration() {
    use crate::federation::control::Federation;
    let mut fixture = Fixture::new();
    fixture.config.enable_execution = true;
    let prepared = fixture.prepare().unwrap();
    let listener = TcpListener::bind_address("127.0.0.1:0", Executor::default()).await.unwrap();
    assert!(prepared.serve(listener, Executor::default()).await.is_err());
    let prepared = fixture.prepare().unwrap();
    let mut native = crate::federation::inventory::tests::native_service();
    let prepared = prepared.with_native_service(&mut native, Executor::default()).unwrap();
    assert!(prepared.execution_enabled());
    let error = prepared.service.synthesize(rama::http::grpc::Request::new(
        crate::federation::control::SynthesizeRequest { envelope: None, prompt: "abc".into(), max_tokens: 1, sampler: None }
    )).await.err().unwrap();
    assert_eq!(error.code(), rama::http::grpc::Code::InvalidArgument);
    // Attaching a model/controller never grants execution to a peer.
    assert!(prepared.store.snapshot().unwrap().records().all(|record| record.policy.execute_exports.is_empty()));
}

#[test]
fn routing_flags_are_explicit_require_polling_imports_and_do_not_shadow_local_models() {
    let mut fixture = Fixture::new();
    fixture.config.enable_routing = true;
    assert!(fixture.prepare().is_err(), "no polling/no explicit alias");
    fixture.config.poll_catalogs = true;
    assert!(fixture.prepare().is_err(), "no alias");
    let route = Route {
        alias: "native".into(), peer_id: "remote.mdma.test".into(), model: "remote-model".into(),
        artifact_id: format!("sha256:{}", "a".repeat(64)), profile_id: format!("sha256:{}", "b".repeat(64)),
        operation: crate::federation::config::Operation::Synthesize,
    };
    fixture.config.routes.push(route.clone());
    assert!(fixture.prepare().is_err(), "no current import approval");
    let store = Store::open(&fixture.config.trust_state, &fixture.config.server_id).unwrap();
    let mut policy = store.snapshot().unwrap().peer("remote.mdma.test").unwrap().policy.clone();
    policy.route_imports.push(crate::federation::config::RouteImport {
        model: route.model, artifact_id: route.artifact_id, profile_id: route.profile_id, operation: route.operation,
    });
    store.approve_policy(policy).unwrap();
    drop(store);
    let prepared = fixture.prepare().unwrap();
    let mut native = crate::federation::inventory::tests::native_service();
    assert!(prepared.with_native_service(&mut native, Executor::default()).is_err(), "cannot shadow local model");
    fixture.config.routes[0].alias = "remote-native".into();
    let prepared = fixture.prepare().unwrap().with_native_service(&mut native, Executor::default()).unwrap();
    assert!(prepared.client_policy.routing);
    assert!(!prepared.execution_enabled(), "origin routing is independent from executor permission");
    drop(prepared);
    fixture.config.routes.push(fixture.config.routes[0].clone());
    assert!(fixture.prepare().is_err(), "duplicate aliases");
}

#[test]
fn old_runtime_configs_default_both_workload_switches_off() {
    let fixture = Fixture::new();
    let path = fixture.dir.path().join("old-config.toml");
    let text = toml::to_string(&fixture.config).unwrap()
        .replace("enable_execution = false\n", "").replace("enable_routing = false\n", "").replace("routes = []\n", "");
    std::fs::write(&path, text).unwrap();
    let config = Config::load(&path).unwrap();
    assert!(!config.enable_execution && !config.enable_routing && config.routes.is_empty());
}
