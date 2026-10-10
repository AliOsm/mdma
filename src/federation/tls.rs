//! Federation's strict certificate profile on top of normal rustls PKI.
//!
//! A parsed identity is *not* authentication. Verifiers additionally check the
//! chain under this particular peer's pinned roots and rustls verifies proof of
//! possession. Only a completed TLS connection can produce an RPC binding.

use super::config::{Peer, TlsRole};
use super::trust::{Snapshot, State, Store};
use rama::tls::rustls::dep::rustls;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use sha2::{Digest as _, Sha256};
use std::io::Read;
use std::sync::Arc;
use x509_parser::prelude::*;

mod binding;
pub(crate) mod caller;
pub use caller::BindCallerService;
pub(crate) use caller::{Caller, listener_expiry};
pub use binding::utc_ms;
pub use binding::{BindPeerService, Binding};

const MAX_CERT_BYTES: usize = 64 * 1024;
const MAX_CHAIN_CERTS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("federation certificate denied: {0}")]
    Profile(&'static str),
    #[error("federation PKI: {0}")]
    Pki(#[from] rustls::Error),
    #[error(transparent)]
    Trust(#[from] super::trust::Error),
    #[error("certificate storage: {0}")]
    Io(#[from] std::io::Error),
}

fn deny(message: &'static str) -> Error {
    Error::Profile(message)
}
fn tls_error(error: impl std::fmt::Display) -> rustls::Error {
    rustls::Error::General(error.to_string())
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// A provisioning digest only, not authentication or peer enrollment.
pub fn certificate_spki_sha256(cert: &CertificateDer<'_>) -> Result<String, Error> {
    Ok(sha256(parse(cert.as_ref())?.public_key().raw))
}

#[derive(Debug, Clone)]
pub(crate) struct Profile {
    pub server_id: String,
    pub spki_sha256: String,
    pub public_key: Vec<u8>,
    pub expires_ms: u64,
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, Error> {
    if der.len() > MAX_CERT_BYTES {
        return Err(deny("certificate too large"));
    }
    let (rest, certificate) =
        X509Certificate::from_der(der).map_err(|_| deny("invalid DER certificate"))?;
    if !rest.is_empty() {
        return Err(deny("trailing certificate data"));
    }
    certificate
        .extensions_map()
        .map_err(|_| deny("duplicate certificate extension"))?;
    Ok(certificate)
}

fn validity(certificate: &X509Certificate<'_>, now_ms: u64) -> Result<u64, Error> {
    let before = certificate.validity().not_before.timestamp();
    let after = certificate.validity().not_after.timestamp();
    let after_ms = u64::try_from(after)
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .filter(|ms| *ms <= super::config::MAX_SAFE_TIMESTAMP)
        .ok_or_else(|| deny("invalid certificate expiry"))?;
    // Pre-epoch notBefore is valid, but UTC itself must remain nonnegative.
    if before >= 0 && now_ms < before.cast_unsigned().saturating_mul(1000) || now_ms >= after_ms {
        return Err(deny("certificate outside its validity interval"));
    }
    Ok(after_ms)
}

pub(crate) fn profile(der: &[u8], role: TlsRole, now_ms: u64) -> Result<Profile, Error> {
    let cert = parse(der)?;
    let expires_ms = validity(&cert, now_ms)?;
    let basic = cert
        .basic_constraints()
        .map_err(|_| deny("invalid basic constraints"))?;
    if basic.is_some_and(|bc| bc.value.ca) {
        return Err(deny("federation leaf is a CA"));
    }
    let ku = cert
        .key_usage()
        .map_err(|_| deny("invalid key usage"))?
        .ok_or_else(|| deny("explicit digitalSignature key usage required"))?;
    if !ku.value.digital_signature() || ku.value.key_cert_sign() {
        return Err(deny("invalid federation leaf key usage"));
    }
    let eku = cert
        .extended_key_usage()
        .map_err(|_| deny("invalid extended key usage"))?
        .ok_or_else(|| deny("explicit role extended key usage required"))?;
    let matches_role = match role {
        TlsRole::ClientAuth => eku.value.client_auth,
        TlsRole::ServerAuth => eku.value.server_auth,
    };
    if !matches_role {
        return Err(deny("certificate lacks required role EKU"));
    }
    let san = cert
        .subject_alternative_name()
        .map_err(|_| deny("invalid SAN"))?
        .ok_or_else(|| deny("identity URI SAN required"))?;
    let uris: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect();
    if uris.len() != 1 {
        return Err(deny("exactly one identity URI SAN required"));
    }
    let id = uris[0]
        .strip_prefix("https://")
        .and_then(|s| s.strip_suffix("/.well-known/mdma/identity"))
        .ok_or_else(|| deny("identity URI is not the exact federation form"))?;
    super::server_id::validate(id).map_err(|_| deny("invalid URI server identity"))?;
    Ok(Profile {
        server_id: id.into(),
        spki_sha256: sha256(cert.public_key().raw),
        public_key: cert.public_key().subject_public_key.data.to_vec(),
        expires_ms,
    })
}

fn eligible<'a>(
    snapshot: &'a Snapshot,
    leaf: &Profile,
    role: TlsRole,
    now_ms: u64,
) -> Result<(&'a Peer, u64), Error> {
    let record = snapshot
        .peer(&leaf.server_id)
        .ok_or_else(|| deny("unconfigured peer"))?;
    if !matches!(record.state, State::Staged | State::Active) {
        return Err(deny("peer suspended or revoked"));
    }
    if snapshot.tls_denied(&leaf.spki_sha256) {
        return Err(deny("revoked TLS public key"));
    }
    if record.policy.signing_keys.iter().any(|k| {
        k.bytes().is_ok_and(|k| k.as_slice() == leaf.public_key)
    }) {
        return Err(deny("TLS and catalog signing keys must be different"));
    }
    let pin = record
        .policy
        .tls_pins
        .iter()
        .find(|pin| {
            pin.role == role
                && pin.spki_sha256 == leaf.spki_sha256
                && record.pin_eligible(pin, now_ms)
        })
        .ok_or_else(|| deny("no current role-specific leaf SPKI pin"))?;
    Ok((
        &record.policy,
        leaf.expires_ms.min(pin.window.valid_until_ms),
    ))
}

fn roots(peer: &Peer, now_ms: u64) -> Result<Vec<(CertificateDer<'static>, String, u64)>, Error> {
    let mut roots = Vec::new();
    for root in &peer.ca_roots {
        let mut bytes = Vec::new();
        std::fs::File::open(&root.path)?
            .take((MAX_CERT_BYTES * 2 + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CERT_BYTES * 2 {
            return Err(deny("root PEM file too large"));
        }
        let certs = CertificateDer::pem_slice_iter(&bytes)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| deny("invalid root PEM"))?;
        let Ok([der]) = <[_; 1]>::try_from(certs) else {
            return Err(deny(
                "each approved root file must contain exactly one certificate",
            ));
        };
        let digest = sha256(der.as_ref());
        if digest != root.certificate_sha256 {
            return Err(deny("root DER fingerprint changed"));
        }
        let cert = parse(der.as_ref())?;
        if !cert
            .basic_constraints()
            .map_err(|_| deny("invalid root constraints"))?
            .is_some_and(|bc| bc.value.ca)
        {
            return Err(deny("approved trust anchor is not a CA"));
        }
        let expiry = validity(&cert, now_ms)?;
        roots.push((der, digest, expiry));
    }
    Ok(roots)
}

#[derive(Debug)]
pub(crate) struct Verified {
    pub profile: Profile,
    pub root_sha256: String,
    pub expires_ms: u64,
}

fn verify_chain(
    snapshot: &Snapshot,
    chain: &[CertificateDer<'_>],
    role: TlsRole,
    now_ms: u64,
    expected: Option<&str>,
) -> Result<Verified, Error> {
    if chain.is_empty() || chain.len() > MAX_CHAIN_CERTS {
        return Err(deny("invalid certificate chain length"));
    }
    let leaf = profile(chain[0].as_ref(), role, now_ms)?;
    if expected.is_some_and(|id| id != leaf.server_id) {
        return Err(deny("wrong configured peer identity"));
    }
    let (peer, mut expiry) = eligible(snapshot, &leaf, role, now_ms)?;
    for certificate in &chain[1..] {
        expiry = expiry.min(validity(&parse(certificate.as_ref())?, now_ms)?);
    }
    let now = UnixTime::since_unix_epoch(std::time::Duration::from_millis(now_ms));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    // A verifier per root records the actual approved anchor, not a combined
    // listener trust store. Verification never fetches AIA/CRL/OCSP URLs.
    for (der, root_sha256, root_expiry) in roots(peer, now_ms)? {
        let mut store = rustls::RootCertStore::empty();
        store.add(der)?;
        let store = Arc::new(store);
        let valid = match role {
            TlsRole::ClientAuth => {
                let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                    store,
                    provider.clone(),
                )
                .build()
                .map_err(|_| deny("invalid client trust anchor"))?;
                verifier
                    .verify_client_cert(&chain[0], &chain[1..], now)
                    .map(|_| ())
            }
            TlsRole::ServerAuth => {
                let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
                    store,
                    provider.clone(),
                )
                .build()
                .map_err(|_| deny("invalid server trust anchor"))?;
                let name = ServerName::try_from(peer.tls_name.as_str())
                    .map_err(|_| deny("invalid tls_name"))?;
                verifier
                    .verify_server_cert(&chain[0], &chain[1..], &name, &[], now)
                    .map(|_| ())
            }
        };
        if valid.is_ok() {
            return Ok(Verified {
                profile: leaf,
                root_sha256,
                expires_ms: expiry.min(root_expiry),
            });
        }
    }
    Err(deny(
        "chain invalid under this peer's approved roots/service name",
    ))
}

/// Defense against unrestricted client-RPC bypass. Until a separate allowlisted
/// client identity layer exists, overlapping issuers are rejected, not tolerated.
pub fn reject_client_root_overlap(
    peers: &[Peer],
    client_roots: &[CertificateDer<'_>],
) -> Result<(), Error> {
    let mut client_keys = Vec::new();
    for cert in client_roots {
        // Compare key material, not SPKI encoding: an AlgorithmIdentifier
        // parameter/encoding variation must not disguise the same issuer key.
        client_keys.push(sha256(parse(cert.as_ref())?.public_key().subject_public_key.data.as_ref()));
    }
    for peer in peers {
        // Validity is checked at handshake; fingerprint/key comparison here must
        // not overlook a reissued certificate with the same issuer key.
        for root in &peer.ca_roots {
            let mut bytes = Vec::new();
            std::fs::File::open(&root.path)?
                .take((MAX_CERT_BYTES * 2 + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_CERT_BYTES * 2 {
                return Err(deny("root PEM file too large"));
            }
            let cert =
                CertificateDer::from_pem_slice(&bytes).map_err(|_| deny("invalid root PEM"))?;
            if sha256(cert.as_ref()) != root.certificate_sha256 {
                return Err(deny("root fingerprint changed"));
            }
            if client_keys.contains(&sha256(parse(cert.as_ref())?.public_key().subject_public_key.data.as_ref())) {
                return Err(deny("federation issuer overlaps unrestricted client trust"));
            }
        }
    }
    Ok(())
}

pub struct InboundVerifier {
    store: Arc<Store>,
}

impl std::fmt::Debug for InboundVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FederationInboundVerifier")
    }
}

impl InboundVerifier {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl ClientCertVerifier for InboundVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let now_ms = now
            .as_secs()
            .checked_mul(1000)
            .ok_or_else(|| tls_error("invalid UTC time"))?;
        self.store.observe_time(now_ms).map_err(tls_error)?;
        let snapshot = self.store.snapshot().map_err(tls_error)?;
        let chain: Vec<_> = std::iter::once(leaf.clone())
            .chain(intermediates.iter().cloned())
            .collect();
        verify_chain(&snapshot, &chain, TlsRole::ClientAuth, now_ms, None).map_err(tls_error)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(tls_error("federation requires TLS 1.3"))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub struct OutboundVerifier {
    store: Arc<Store>,
    peer_id: String,
}

impl std::fmt::Debug for OutboundVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationOutboundVerifier")
            .field("peer_id", &self.peer_id)
            .finish_non_exhaustive()
    }
}

impl OutboundVerifier {
    pub fn new(store: Arc<Store>, peer_id: String) -> Self {
        Self { store, peer_id }
    }
}

impl ServerCertVerifier for OutboundVerifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let now_ms = now
            .as_secs()
            .checked_mul(1000)
            .ok_or_else(|| tls_error("invalid UTC time"))?;
        self.store.observe_time(now_ms).map_err(tls_error)?;
        let snapshot = self.store.snapshot().map_err(tls_error)?;
        let peer = snapshot
            .peer(&self.peer_id)
            .ok_or_else(|| tls_error("unknown outbound peer"))?;
        let expected = ServerName::try_from(peer.policy.tls_name.as_str()).map_err(tls_error)?;
        if &expected != server_name {
            return Err(tls_error("outbound TLS reference name changed"));
        }
        let chain: Vec<_> = std::iter::once(leaf.clone())
            .chain(intermediates.iter().cloned())
            .collect();
        verify_chain(
            &snapshot,
            &chain,
            TlsRole::ServerAuth,
            now_ms,
            Some(&self.peer_id),
        )
        .map_err(tls_error)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(tls_error("federation requires TLS 1.3"))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Construct from scratch: TLS 1.3 only, no stored sessions/tickets/early data.
pub fn server_config(
    store: Arc<Store>,
    chain: Vec<CertificateDer<'static>>,
    key: rustls_pki_types::PrivateKeyDer<'static>,
    local_signing_key: &[u8; 32],
) -> Result<rustls::ServerConfig, Error> {
    binding::local_expiry(
        &chain,
        TlsRole::ServerAuth,
        &store.snapshot()?.server_id,
        local_signing_key,
        utc_ms()?,
    )?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(InboundVerifier::new(store)))
        .with_single_cert(chain, key)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    // The from-scratch rustls builder installs a disabled ticketer.
    if config.ticketer.enabled() {
        return Err(deny("session tickets unexpectedly enabled"));
    }
    Ok(config)
}

/// Validate the configured local issuer independently of the remote trust
/// records. This also caps the listener binding by the actual issuer lifetime.
pub(crate) fn local_server_expiry(
    chain: &[CertificateDer<'_>],
    policy: &Peer,
    signing_key: &[u8; 32],
    now_ms: u64,
) -> Result<u64, Error> {
    local_credential_expiry(chain, policy, signing_key, TlsRole::ServerAuth, now_ms)
}

pub(crate) fn local_client_expiry(
    chain: &[CertificateDer<'_>],
    policy: &Peer,
    signing_key: &[u8; 32],
    now_ms: u64,
) -> Result<u64, Error> {
    local_credential_expiry(chain, policy, signing_key, TlsRole::ClientAuth, now_ms)
}

fn local_credential_expiry(
    chain: &[CertificateDer<'_>],
    policy: &Peer,
    signing_key: &[u8; 32],
    role: TlsRole,
    now_ms: u64,
) -> Result<u64, Error> {
    policy.validate().map_err(super::trust::Error::from)?;
    let expiry = binding::local_expiry(
        chain, role, &policy.server_id, signing_key, now_ms,
    )?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    for (root, _, root_expiry) in roots(policy, now_ms)? {
        let mut store = rustls::RootCertStore::empty();
        store.add(root)?;
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_millis(now_ms));
        let valid = match role {
            TlsRole::ServerAuth => {
                let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(store), provider.clone(),
                ).build().map_err(|_| deny("invalid local issuer"))?;
                verifier.verify_server_cert(
                    &chain[0], &chain[1..], &ServerName::try_from(policy.tls_name.as_str())
                        .map_err(|_| deny("invalid local TLS reference name"))?,
                    &[], now,
                ).map(|_| ())
            }
            TlsRole::ClientAuth => {
                let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(store), provider.clone(),
                ).build().map_err(|_| deny("invalid local issuer"))?;
                verifier.verify_client_cert(&chain[0], &chain[1..], now).map(|_| ())
            }
        };
        if valid.is_ok() {
            return Ok(expiry.min(root_expiry));
        }
    }
    Err(deny("local federation chain has no approved issuer"))
}

pub(crate) fn reject_shared_leaf_key(
    federation_leaf: &CertificateDer<'_>,
    client_leaf: &CertificateDer<'_>,
) -> Result<(), Error> {
    if parse(federation_leaf.as_ref())?.public_key().subject_public_key.data
        == parse(client_leaf.as_ref())?.public_key().subject_public_key.data {
        return Err(deny("client and federation TLS credentials share a key"));
    }
    Ok(())
}

pub fn client_config(
    store: Arc<Store>,
    peer_id: String,
    chain: Vec<CertificateDer<'static>>,
    key: rustls_pki_types::PrivateKeyDer<'static>,
    local_signing_key: &[u8; 32],
) -> Result<rustls::ClientConfig, Error> {
    binding::local_expiry(
        &chain,
        TlsRole::ClientAuth,
        &store.snapshot()?.server_id,
        local_signing_key,
        utc_ms()?,
    )?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(OutboundVerifier::new(store, peer_id)))
        .with_client_auth_cert(chain, key)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    Ok(config)
}

#[cfg(test)]
pub(crate) mod tests;
