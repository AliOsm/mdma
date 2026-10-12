use super::super::config::{CaRoot, ControlPermissions, TlsPin, Window};
use super::super::trust::Action;
use super::*;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls_pki_types::PrivatePkcs8KeyDer;
use std::time::{Duration, Instant};

fn ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

fn leaf_params(id: &str) -> CertificateParams {
    let mut params = CertificateParams::new(vec![id.into()]).unwrap();
    params.subject_alt_names.push(SanType::URI(
        format!("https://{id}/.well-known/mdma/identity")
            .try_into()
            .unwrap(),
    ));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::ServerAuth,
    ];
    params
}

fn leaf(id: &str, issuer: &CertifiedIssuer<'_, KeyPair>) -> (CertificateDer<'static>, KeyPair) {
    let key = KeyPair::generate().unwrap();
    let cert = leaf_params(id).signed_by(&key, issuer).unwrap();
    (cert.der().clone(), key)
}

fn private(key: &KeyPair) -> rustls_pki_types::PrivateKeyDer<'static> {
    PrivatePkcs8KeyDer::from(key.serialize_der()).into()
}

pub(crate) struct Fixture {
    dir: tempfile::TempDir,
    issuer: CertifiedIssuer<'static, KeyPair>,
    pub(crate) store: Arc<Store>,
    pub(crate) remote_store: Arc<Store>,
    local_leaf: CertificateDer<'static>,
    remote_leaf: CertificateDer<'static>,
    local_key: KeyPair,
    remote_key: KeyPair,
    pub(crate) now_ms: u64,
}

impl Fixture {
    pub(crate) fn outbound_material(
        &self,
    ) -> (
        Vec<CertificateDer<'static>>,
        rustls_pki_types::PrivateKeyDer<'static>,
    ) {
        (vec![self.remote_leaf.clone()], private(&self.remote_key))
    }

    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let issuer = ca();
        let (local_leaf, local_key) = leaf("local.mdma.test", &issuer);
        let (remote_leaf, remote_key) = leaf("remote.mdma.test", &issuer);
        let now_ms = utc_ms().unwrap();
        let root_path = dir.path().join("root.pem");
        std::fs::write(&root_path, issuer.pem()).unwrap();
        let store =
            Arc::new(Store::initialize(&dir.path().join("local.json"), "local.mdma.test").unwrap());
        let remote_store = Arc::new(
            Store::initialize(&dir.path().join("remote.json"), "remote.mdma.test").unwrap(),
        );
        for (store, id, cert) in [
            (&store, "remote.mdma.test", &remote_leaf),
            (&remote_store, "local.mdma.test", &local_leaf),
        ] {
            let mut peer = super::super::config::tests::peer();
            peer.server_id = id.into();
            peer.tls_name = id.into();
            peer.ca_roots = vec![CaRoot {
                path: root_path.clone(),
                certificate_sha256: sha256(issuer.der()),
            }];
            peer.tls_pins = [TlsRole::ClientAuth, TlsRole::ServerAuth]
                .into_iter()
                .map(|role| TlsPin {
                    role,
                    spki_sha256: profile(cert, role, now_ms).unwrap().spki_sha256,
                    window: Window {
                        valid_from_ms: now_ms - 60_000,
                        valid_until_ms: now_ms + 300_000,
                    },
                })
                .collect();
            peer.signing_keys[0].window = peer.tls_pins[0].window;
            store.stage(peer).unwrap();
            store
                .activate_control(
                    id,
                    ControlPermissions {
                        identify: true,
                        scan_capabilities: true,
                    },
                )
                .unwrap();
        }
        Self {
            dir,
            issuer,
            store,
            remote_store,
            local_leaf,
            remote_leaf,
            local_key,
            remote_key,
            now_ms,
        }
    }

    fn verified(&self, cert: &CertificateDer<'_>, role: TlsRole) -> Result<Verified, Error> {
        verify_chain(
            &self.store.snapshot()?,
            std::slice::from_ref(cert),
            role,
            self.now_ms,
            Some("remote.mdma.test"),
        )
    }

    pub(crate) fn configs(&self) -> (rustls::ClientConfig, rustls::ServerConfig) {
        let server = server_config(
            self.store.clone(),
            vec![self.local_leaf.clone()],
            private(&self.local_key),
            &[7; 32],
        )
        .unwrap();
        let client = client_config(
            self.remote_store.clone(),
            "local.mdma.test".into(),
            vec![self.remote_leaf.clone()],
            private(&self.remote_key),
            &[7; 32],
        )
        .unwrap();
        (client, server)
    }

    pub(crate) fn bindings(&self) -> (Binding, Binding) {
        let (client, server) = self.configs();
        let (client, server) = handshake(Arc::new(client), Arc::new(server)).unwrap();
        let now = Instant::now();
        (
            Binding::from_inbound(
                &self.store,
                &server,
                self.now_ms + 100_000,
                self.now_ms,
                now,
            )
            .unwrap(),
            Binding::from_outbound(
                &self.remote_store,
                "local.mdma.test",
                &client,
                self.now_ms + 100_000,
                self.now_ms,
                now,
            )
            .unwrap(),
        )
    }
}

pub(super) fn handshake(
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
) -> Result<(rustls::ClientConnection, rustls::ServerConnection), rustls::Error> {
    let mut client =
        rustls::ClientConnection::new(client, ServerName::try_from("local.mdma.test").unwrap())?;
    let mut server = rustls::ServerConnection::new(server)?;
    for _ in 0..20 {
        if client.wants_write() {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            server.read_tls(&mut std::io::Cursor::new(bytes)).unwrap();
            server.process_new_packets()?;
        }
        if server.wants_write() {
            let mut bytes = Vec::new();
            server.write_tls(&mut bytes).unwrap();
            client.read_tls(&mut std::io::Cursor::new(bytes)).unwrap();
            client.process_new_packets()?;
        }
        if !client.is_handshaking()
            && !server.is_handshaking()
            && !client.wants_write()
            && !server.wants_write()
        {
            return Ok((client, server));
        }
    }
    panic!("handshake did not terminate");
}

#[test]
fn chain_and_role_pin_are_both_required() {
    let f = Fixture::new();
    f.verified(&f.remote_leaf, TlsRole::ClientAuth).unwrap();
    f.verified(&f.remote_leaf, TlsRole::ServerAuth).unwrap();
    let (unpinned, _) = leaf("remote.mdma.test", &f.issuer);
    assert!(f.verified(&unpinned, TlsRole::ClientAuth).is_err());
    let wrong_ca = ca();
    let forged = leaf_params("remote.mdma.test")
        .signed_by(&f.remote_key, &wrong_ca)
        .unwrap();
    assert!(f.verified(forged.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn renewal_same_key_and_explicit_overlap_work() {
    let f = Fixture::new();
    let renewed = leaf_params("remote.mdma.test")
        .signed_by(&f.remote_key, &f.issuer)
        .unwrap();
    f.verified(renewed.der(), TlsRole::ClientAuth).unwrap();
    let (new_leaf, _) = leaf("remote.mdma.test", &f.issuer);
    let snapshot = f.store.snapshot().unwrap();
    let mut policy = snapshot.peer("remote.mdma.test").unwrap().policy.clone();
    policy.tls_pins.push(TlsPin {
        role: TlsRole::ClientAuth,
        spki_sha256: profile(&new_leaf, TlsRole::ClientAuth, f.now_ms)
            .unwrap()
            .spki_sha256,
        window: policy.tls_pins[0].window,
    });
    f.store.approve_policy(policy).unwrap();
    f.verified(&new_leaf, TlsRole::ClientAuth).unwrap();
    f.verified(&f.remote_leaf, TlsRole::ClientAuth).unwrap();
}

#[test]
fn wrong_uri_multiple_uris_missing_usage_and_ca_leaf_fail() {
    let f = Fixture::new();
    let mut variants = Vec::new();
    let mut p = leaf_params("remote.mdma.test");
    p.subject_alt_names[1] = SanType::URI(
        "https://remote.mdma.test:443/.well-known/mdma/identity"
            .try_into()
            .unwrap(),
    );
    variants.push(p);
    let mut p = leaf_params("remote.mdma.test");
    p.subject_alt_names.push(p.subject_alt_names[1].clone());
    variants.push(p);
    let mut p = leaf_params("remote.mdma.test");
    p.key_usages.clear();
    variants.push(p);
    let mut p = leaf_params("remote.mdma.test");
    p.extended_key_usages.clear();
    variants.push(p);
    let mut p = leaf_params("remote.mdma.test");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    variants.push(p);
    for params in variants {
        let cert = params.signed_by(&f.remote_key, &f.issuer).unwrap();
        assert!(f.verified(cert.der(), TlsRole::ClientAuth).is_err());
    }
}

#[test]
fn wrong_certificate_role_and_reference_name_fail() {
    let f = Fixture::new();
    let mut params = leaf_params("remote.mdma.test");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params.signed_by(&f.remote_key, &f.issuer).unwrap();
    assert!(f.verified(cert.der(), TlsRole::ClientAuth).is_err());
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.tls_pins.retain(|p| p.role == TlsRole::ServerAuth);
    f.store.approve_policy(policy.clone()).unwrap();
    assert!(f.verified(&f.remote_leaf, TlsRole::ClientAuth).is_err());
    policy.tls_name = "wrong.mdma.test".into();
    f.store.approve_policy(policy).unwrap();
    assert!(f.verified(&f.remote_leaf, TlsRole::ServerAuth).is_err());
}

#[test]
fn no_common_name_fallback_or_identity_alias() {
    let f = Fixture::new();
    let mut params = leaf_params("remote.mdma.test");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "remote.mdma.test");
    params
        .subject_alt_names
        .retain(|name| matches!(name, SanType::URI(_)));
    let cert = params.signed_by(&f.remote_key, &f.issuer).unwrap();
    assert!(f.verified(cert.der(), TlsRole::ServerAuth).is_err());
    let cert = leaf_params("other.mdma.test")
        .signed_by(&f.remote_key, &f.issuer)
        .unwrap();
    assert!(f.verified(cert.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn root_file_replacement_and_client_issuer_overlap_fail() {
    let f = Fixture::new();
    let policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    assert!(
        reject_client_root_overlap(std::slice::from_ref(&policy), &[f.issuer.der().clone()])
            .is_err()
    );
    let unrelated = ca();
    reject_client_root_overlap(std::slice::from_ref(&policy), &[unrelated.der().clone()]).unwrap();
    std::fs::write(&policy.ca_roots[0].path, unrelated.pem()).unwrap();
    assert!(f.verified(&f.remote_leaf, TlsRole::ClientAuth).is_err());
}

#[test]
fn issuer_and_leaf_separation_compare_key_material_not_spki_encoding() {
    // A syntactic AlgorithmIdentifier variation, not a usable TLS credential.
    // The isolation precheck must reject identical key bits independently of
    // a subsequent PKI verifier rejecting its parameters/signature.
    fn variant(cert: &CertificateDer<'_>) -> CertificateDer<'static> {
        let mut bytes = cert.as_ref().to_vec();
        let parsed = parse(&bytes).unwrap();
        let spki = parsed.public_key().raw;
        let offset = bytes.windows(spki.len()).position(|w| w == spki).unwrap();
        let curve_oid = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let curve = spki
            .windows(curve_oid.len())
            .position(|w| w == curve_oid)
            .unwrap();
        bytes[offset + curve + curve_oid.len() - 1] = 0x06;
        let changed = CertificateDer::from(bytes);
        assert_ne!(
            parse(cert.as_ref()).unwrap().public_key().raw,
            parse(changed.as_ref()).unwrap().public_key().raw
        );
        assert_eq!(
            parse(cert.as_ref())
                .unwrap()
                .public_key()
                .subject_public_key
                .data,
            parse(changed.as_ref())
                .unwrap()
                .public_key()
                .subject_public_key
                .data
        );
        changed
    }
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let f = Fixture::new();
    let changed = variant(f.issuer.der());
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    let root_path = f.dir.path().join("variant-root.pem");
    std::fs::write(
        &root_path,
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            STANDARD.encode(&changed)
        ),
    )
    .unwrap();
    policy.ca_roots = vec![CaRoot {
        path: root_path,
        certificate_sha256: sha256(&changed),
    }];
    assert!(reject_client_root_overlap(&[policy], &[f.issuer.der().clone()]).is_err());
    assert!(reject_shared_leaf_key(&f.local_leaf, &variant(&f.local_leaf)).is_err());
}

#[test]
fn uri_name_constraints_are_rejected_instead_of_ignored() {
    let f = Fixture::new();
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let mut der = vec![0x30, 0x10, 0xa0, 0x0e, 0x30, 0x0c, 0x86, 0x0a];
    der.extend_from_slice(b".mdma.test");
    let mut extension = rcgen::CustomExtension::from_oid_content(&[2, 5, 29, 30], der);
    extension.set_criticality(true);
    params.custom_extensions.push(extension);
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let cert = leaf_params("remote.mdma.test")
        .signed_by(&f.remote_key, &issuer)
        .unwrap();
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    std::fs::write(&policy.ca_roots[0].path, issuer.pem()).unwrap();
    policy.ca_roots[0].certificate_sha256 = sha256(issuer.der());
    f.store.approve_policy(policy).unwrap();
    // rustls-webpki does not implement URI constraints; its specified behavior
    // is fail closed. Do not supply a hand-written constraint bypass.
    assert!(f.verified(cert.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn full_mtls_handshake_yields_live_bindings_and_never_resumes() {
    let f = Fixture::new();
    let (client, server) = f.configs();
    assert_eq!(server.send_tls13_tickets, 0);
    assert!(!server.ticketer.enabled());
    assert_eq!(server.max_early_data_size, 0);
    assert!(!client.enable_early_data);
    let client = Arc::new(client);
    let server = Arc::new(server);
    for _ in 0..2 {
        let (client_conn, server_conn) = handshake(client.clone(), server.clone()).unwrap();
        assert_eq!(
            client_conn.handshake_kind(),
            Some(rustls::HandshakeKind::Full)
        );
        let now = Instant::now();
        let inbound =
            Binding::from_inbound(&f.store, &server_conn, f.now_ms + 100_000, f.now_ms, now)
                .unwrap();
        inbound
            .authorize(&f.store, Action::Identify, f.now_ms, now)
            .unwrap();
        assert_eq!(inbound.server_id(), "remote.mdma.test");
        let outbound = Binding::from_outbound(
            &f.remote_store,
            "local.mdma.test",
            &client_conn,
            f.now_ms + 100_000,
            f.now_ms,
            now,
        )
        .unwrap();
        outbound
            .authorize(&f.remote_store, Action::ScanCapabilities, f.now_ms, now)
            .unwrap();
    }
}

#[test]
fn tls12_and_anonymous_client_cannot_connect() {
    let f = Fixture::new();
    let (_, server) = f.configs();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(f.issuer.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let client = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .with_root_certificates(roots.clone())
        .with_client_auth_cert(vec![f.remote_leaf.clone()], private(&f.remote_key))
        .unwrap();
    assert!(handshake(Arc::new(client), Arc::new(server)).is_err());
    let (_, server) = f.configs();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    assert!(handshake(Arc::new(client), Arc::new(server)).is_err());
}

#[test]
fn binding_expiry_clock_rollback_revocation_and_pin_removal() {
    let f = Fixture::new();
    let (client, server) = f.configs();
    let (_, server_conn) = handshake(Arc::new(client), Arc::new(server)).unwrap();
    let now = Instant::now();
    let binding =
        Binding::from_inbound(&f.store, &server_conn, f.now_ms + 1000, f.now_ms, now).unwrap();
    assert!(
        binding
            .authorize(
                &f.store,
                Action::Identify,
                f.now_ms - 1,
                now + Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        binding
            .authorize(&f.store, Action::Identify, f.now_ms + 1000, now)
            .is_err()
    );
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.tls_pins.retain(|p| p.role != TlsRole::ClientAuth);
    f.store.approve_policy(policy).unwrap();
    assert!(
        binding
            .authorize(&f.store, Action::Identify, f.now_ms, now)
            .is_err()
    );
    f.store.suspend("remote.mdma.test").unwrap();
    assert!(
        binding
            .authorize(&f.store, Action::Identify, f.now_ms, now)
            .is_err()
    );
}

#[test]
fn tls_key_cannot_double_as_catalog_key() {
    use base64::Engine as _;
    let f = Fixture::new();
    let key = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let cert = leaf_params("remote.mdma.test")
        .signed_by(&key, &f.issuer)
        .unwrap();
    let leaf = profile(cert.der(), TlsRole::ClientAuth, f.now_ms).unwrap();
    let mut policy = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    policy.tls_pins[0].spki_sha256 = leaf.spki_sha256;
    policy.signing_keys[0].key_id = "ed25519:tls-reuse".into();
    policy.signing_keys[0].public_key =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&leaf.public_key);
    f.store.approve_policy(policy).unwrap();
    assert!(f.verified(cert.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn leaf_expiry_and_not_before_are_enforced_even_with_valid_pin() {
    let f = Fixture::new();
    let mut params = leaf_params("remote.mdma.test");
    params.not_after = rcgen::date_time_ymd(2020, 1, 1);
    let expired = params.signed_by(&f.remote_key, &f.issuer).unwrap();
    assert!(f.verified(expired.der(), TlsRole::ClientAuth).is_err());
    let mut params = leaf_params("remote.mdma.test");
    params.not_before = rcgen::date_time_ymd(4090, 1, 1);
    let future = params.signed_by(&f.remote_key, &f.issuer).unwrap();
    assert!(f.verified(future.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn a_root_approved_for_another_peer_does_not_authenticate_this_one() {
    let f = Fixture::new();
    let other_issuer = ca();
    let root_path = f.dir.path().join("other-root.pem");
    std::fs::write(&root_path, other_issuer.pem()).unwrap();
    let mut other = f
        .store
        .snapshot()
        .unwrap()
        .peer("remote.mdma.test")
        .unwrap()
        .policy
        .clone();
    other.server_id = "other.mdma.test".into();
    other.ca_roots = vec![CaRoot {
        path: root_path,
        certificate_sha256: sha256(other_issuer.der()),
    }];
    f.store.stage(other).unwrap();
    let impersonator = leaf_params("remote.mdma.test")
        .signed_by(&f.remote_key, &other_issuer)
        .unwrap();
    assert!(f.verified(impersonator.der(), TlsRole::ClientAuth).is_err());
}

#[test]
fn endpoint_and_anchor_removal_deny_existing_binding() {
    let f = Fixture::new();
    let (client, server) = f.configs();
    let (client_conn, _) = handshake(Arc::new(client), Arc::new(server)).unwrap();
    let now = Instant::now();
    let binding = Binding::from_outbound(
        &f.remote_store,
        "local.mdma.test",
        &client_conn,
        f.now_ms + 100_000,
        f.now_ms,
        now,
    )
    .unwrap();
    let old = f
        .remote_store
        .snapshot()
        .unwrap()
        .peer("local.mdma.test")
        .unwrap()
        .policy
        .clone();
    let mut policy = old.clone();
    policy.endpoint = "https://127.0.0.1:9555".into();
    f.remote_store.approve_policy(policy).unwrap();
    assert!(
        binding
            .authorize(&f.remote_store, Action::Identify, f.now_ms, now)
            .is_err()
    );
    let mut policy = old;
    policy.ca_roots[0].certificate_sha256 = format!("sha256:{}", "ef".repeat(32));
    f.remote_store.approve_policy(policy).unwrap();
    assert!(
        binding
            .authorize(&f.remote_store, Action::Identify, f.now_ms, now)
            .is_err()
    );
}
