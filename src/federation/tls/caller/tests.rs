use super::*;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls_pki_types::PrivatePkcs8KeyDer;

pub(crate) fn authenticated() -> (Caller, String) {
    let (connection, leaf, _) = connection(true, true);
    let now_ms = utc_ms().unwrap();
    let caller =
        Caller::from_inbound(&connection, now_ms + 300_000, now_ms, Instant::now()).unwrap();
    (caller, certificate_spki_sha256(&leaf).unwrap())
}

fn connection(
    tls13: bool,
    authenticated: bool,
) -> (
    rustls::ServerConnection,
    CertificateDer<'static>,
    CertificateDer<'static>,
) {
    let material = material(tls13, authenticated);
    let (_, server) =
        super::super::tests::handshake(Arc::new(material.client), Arc::new(material.server))
            .unwrap();
    (server, material.caller_leaf, material.root)
}

pub(crate) struct Material {
    pub client: rustls::ClientConfig,
    pub server: rustls::ServerConfig,
    pub caller_leaf: CertificateDer<'static>,
    pub server_leaf: CertificateDer<'static>,
    pub root: CertificateDer<'static>,
}

pub(crate) fn material(tls13: bool, authenticated: bool) -> Material {
    // Ordinary caller issuer, unrelated to any federation peer issuer/pin.
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let mut leaf = CertificateParams::new(vec!["local.mdma.test".into()]).unwrap();
    leaf.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::ServerAuth,
    ];
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let server_key = KeyPair::generate().unwrap();
    let server_cert = leaf.signed_by(&server_key, &issuer).unwrap().der().clone();
    let caller_key = KeyPair::generate().unwrap();
    let caller_cert = leaf.signed_by(&caller_key, &issuer).unwrap().der().clone();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    let versions: &[&rustls::SupportedProtocolVersion] = if tls13 {
        &[&rustls::version::TLS13]
    } else {
        &[&rustls::version::TLS12]
    };
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots.clone()));
    let verifier = if authenticated {
        verifier.build()
    } else {
        verifier.allow_unauthenticated().build()
    }
    .unwrap();
    let mut server = rustls::ServerConfig::builder_with_protocol_versions(versions)
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server_cert.clone()],
            PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        )
        .unwrap();
    let client = rustls::ClientConfig::builder_with_protocol_versions(versions)
        .with_root_certificates(roots);
    let mut client = if authenticated {
        client
            .with_client_auth_cert(
                vec![caller_cert.clone()],
                PrivatePkcs8KeyDer::from(caller_key.serialize_der()).into(),
            )
            .unwrap()
    } else {
        client.with_no_client_auth()
    };
    server.alpn_protocols = vec![b"h2".to_vec()];
    client.alpn_protocols = server.alpn_protocols.clone();
    Material {
        client,
        server,
        caller_leaf: caller_cert,
        server_leaf: server_cert,
        root: issuer.der().clone(),
    }
}

#[test]
fn caller_is_actual_full_local_mtls_spki_not_a_peer_or_metadata_identity() {
    let (connection, leaf, root) = connection(true, true);
    let now_ms = utc_ms().unwrap();
    let now = Instant::now();
    let caller = Caller::from_inbound(&connection, now_ms + 20_000, now_ms, now).unwrap();
    assert_eq!(caller.spki, certificate_spki_sha256(&leaf).unwrap());
    // No identity URI SAN: normal clients aren't federation peers.
    assert!(profile(leaf.as_ref(), TlsRole::ClientAuth, now_ms).is_err());
    let fixture = super::super::tests::Fixture::new();
    assert!(
        caller
            .authorize(
                &fixture.store,
                "remote.mdma.test",
                "model",
                super::super::super::config::Operation::Decide,
                now_ms,
                now
            )
            .is_err()
    );
    let grant = super::super::super::trust::Disclosure {
        caller_spki_sha256: caller.spki.clone(),
        peer_id: "remote.mdma.test".into(),
        model: "model".into(),
        operation: super::super::super::config::Operation::Decide,
        window: super::super::super::config::Window {
            valid_from_ms: now_ms,
            valid_until_ms: now_ms + 10_000,
        },
    };
    fixture.store.approve_disclosure(grant.clone()).unwrap();
    assert_eq!(
        caller
            .authorize(
                &fixture.store,
                &grant.peer_id,
                &grant.model,
                grant.operation,
                now_ms,
                now
            )
            .unwrap(),
        now + Duration::from_secs(10)
    );
    assert!(
        caller
            .authorize(
                &fixture.store,
                &grant.peer_id,
                &grant.model,
                grant.operation,
                now_ms,
                now + Duration::from_secs(20)
            )
            .is_err()
    );
    fixture
        .store
        .remove_disclosure(&caller.spki, &grant.peer_id, &grant.model, grant.operation)
        .unwrap();
    assert!(
        caller
            .authorize(
                &fixture.store,
                &grant.peer_id,
                &grant.model,
                grant.operation,
                now_ms,
                now
            )
            .is_err()
    );
    assert!(listener_expiry(&[leaf], &[root], now_ms).unwrap() > now_ms);
}

#[test]
fn local_tls12_anonymous_and_incomplete_connections_cannot_route() {
    for (tls13, authenticated) in [(false, true), (true, false)] {
        let (connection, _, _) = connection(tls13, authenticated);
        let now = utc_ms().unwrap();
        assert!(Caller::from_inbound(&connection, now + 10_000, now, Instant::now()).is_err());
    }
    let fixture = super::super::tests::Fixture::new();
    let (_, server) = fixture.configs();
    let connection = rustls::ServerConnection::new(Arc::new(server)).unwrap();
    let now = utc_ms().unwrap();
    assert!(Caller::from_inbound(&connection, now + 10_000, now, Instant::now()).is_err());
}

#[test]
fn ordinary_listener_lifetime_is_never_extended_by_expired_material() {
    let (_, leaf, root) = connection(true, true);
    assert!(listener_expiry(&[], std::slice::from_ref(&root), utc_ms().unwrap()).is_err());
    let expiry = listener_expiry(std::slice::from_ref(&leaf), std::slice::from_ref(&root), utc_ms().unwrap()).unwrap();
    assert!(listener_expiry(&[leaf], &[root], expiry).is_err());
}

#[test]
fn an_actual_resumed_local_tls_connection_cannot_mint_routing_consent() {
    let material = material(true, true);
    let client = Arc::new(material.client);
    let server = Arc::new(material.server);
    let (_, first) = super::super::tests::handshake(client.clone(), server.clone()).unwrap();
    let now = utc_ms().unwrap();
    Caller::from_inbound(&first, now + 10_000, now, Instant::now()).unwrap();
    let (_, resumed) = super::super::tests::handshake(client, server).unwrap();
    assert_eq!(resumed.handshake_kind(), Some(rustls::HandshakeKind::Resumed));
    assert!(Caller::from_inbound(&resumed, now + 10_000, now, Instant::now()).is_err());
}
