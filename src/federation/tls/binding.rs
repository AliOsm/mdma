use super::*;
use rama::Service;
use rama::error::BoxError;
use rama::extensions::{Extension, ExtensionsRef};
use rama::io::Io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Authenticated connection facts, never derived from HTTP headers or Identify.
/// Fields are private so callers cannot forge a principal from a server name.
#[derive(Debug, Clone)]
pub struct Binding {
    leaf: Profile,
    role: TlsRole,
    root_sha256: String,
    endpoint: Option<String>,
    tls_name: Option<String>,
    handshake_generation: u64,
    deadline: Instant,
    expires_ms: u64,
}

impl Extension for Binding {}

pub fn utc_ms() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| deny("invalid UTC clock"))?
        .as_millis()
        .try_into()
        .map_err(|_| deny("UTC clock out of range"))
}

pub(super) fn full_handshake(
    kind: Option<rustls::HandshakeKind>,
    version: Option<rustls::ProtocolVersion>,
) -> Result<(), Error> {
    if version != Some(rustls::ProtocolVersion::TLSv1_3)
        || !matches!(
            kind,
            Some(rustls::HandshakeKind::Full | rustls::HandshakeKind::FullWithHelloRetryRequest)
        )
    {
        return Err(deny("full TLS 1.3 handshake required"));
    }
    Ok(())
}

pub(crate) fn local_expiry(
    chain: &[CertificateDer<'_>],
    role: TlsRole,
    local_id: &str,
    signing_key: &[u8; 32],
    now_ms: u64,
) -> Result<u64, Error> {
    if chain.is_empty() || chain.len() > MAX_CHAIN_CERTS {
        return Err(deny("invalid local chain"));
    }
    let leaf = profile(chain[0].as_ref(), role, now_ms)?;
    if leaf.server_id != local_id {
        return Err(deny("local certificate identity mismatch"));
    }
    if leaf.public_key == signing_key {
        return Err(deny("local TLS and signing keys are identical"));
    }
    let mut expiry = leaf.expires_ms;
    for der in &chain[1..] {
        expiry = expiry.min(validity(&parse(der.as_ref())?, now_ms)?);
    }
    Ok(expiry)
}

impl Binding {
    fn new(
        snapshot: &Snapshot,
        verified: Verified,
        role: TlsRole,
        local_expires_ms: u64,
        now_ms: u64,
        now: Instant,
    ) -> Result<Self, Error> {
        let peer = snapshot
            .peer(&verified.profile.server_id)
            .ok_or_else(|| deny("peer disappeared"))?;
        let expires_ms = verified.expires_ms.min(local_expires_ms);
        let remaining_ms = expires_ms
            .checked_sub(now_ms)
            .filter(|ms| *ms > 0)
            .ok_or_else(|| deny("binding already expired"))?;
        let deadline = now
            .checked_add(Duration::from_millis(remaining_ms))
            .ok_or_else(|| deny("binding lifetime out of range"))?;
        Ok(Self {
            leaf: verified.profile,
            role,
            root_sha256: verified.root_sha256,
            endpoint: (role == TlsRole::ServerAuth).then(|| peer.policy.endpoint.clone()),
            tls_name: (role == TlsRole::ServerAuth).then(|| peer.policy.tls_name.clone()),
            handshake_generation: snapshot.generation,
            deadline,
            expires_ms,
        })
    }

    pub fn server_id(&self) -> &str {
        &self.leaf.server_id
    }
    pub fn role(&self) -> TlsRole {
        self.role
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn handshake_generation(&self) -> u64 {
        self.handshake_generation
    }

    /// Required on every RPC and at the final admission/output barrier. An old
    /// snapshot is never accepted as authority. Callers watch Store::subscribe
    /// and this deadline to close/cancel streams, retaining worker charges.
    pub fn authorize(
        &self,
        store: &Store,
        action: super::super::trust::Action<'_>,
        now_ms: u64,
        now: Instant,
    ) -> Result<Snapshot, Error> {
        if now >= self.deadline || now_ms >= self.expires_ms {
            return Err(deny("connection authorization expired"));
        }
        let snapshot = store.authorize(&self.leaf.server_id, action, now_ms)?;
        let (peer, _) = eligible(&snapshot, &self.leaf, self.role, now_ms)?;
        if !peer
            .ca_roots
            .iter()
            .any(|r| r.certificate_sha256 == self.root_sha256)
        {
            return Err(deny("binding trust anchor removed"));
        }
        if self.endpoint.as_ref().is_some_and(|e| e != &peer.endpoint)
            || self.tls_name.as_ref().is_some_and(|n| n != &peer.tls_name)
        {
            return Err(deny("outbound endpoint/reference name changed"));
        }
        Ok(snapshot)
    }

    pub(crate) fn from_inbound(
        store: &Store,
        connection: &rustls::ServerConnection,
        local_expires_ms: u64,
        now_ms: u64,
        now: Instant,
    ) -> Result<Self, Error> {
        if connection.is_handshaking() {
            return Err(deny("TLS handshake incomplete"));
        }
        full_handshake(connection.handshake_kind(), connection.protocol_version())?;
        if connection.alpn_protocol() != Some(b"h2") {
            return Err(deny("federation requires HTTP/2"));
        }
        store.observe_time(now_ms)?;
        let snapshot = store.snapshot()?;
        let chain = connection
            .peer_certificates()
            .ok_or_else(|| deny("mTLS certificate absent"))?;
        let verified = verify_chain(&snapshot, chain, TlsRole::ClientAuth, now_ms, None)?;
        Self::new(
            &snapshot,
            verified,
            TlsRole::ClientAuth,
            local_expires_ms,
            now_ms,
            now,
        )
    }

    /// Capture facts from a completed outbound connection, revalidating the
    /// current peer chain/pins. `local_expires_ms` must come from the same local
    /// credential chain used to construct that connection.
    pub fn from_outbound(
        store: &Store,
        peer_id: &str,
        connection: &rustls::ClientConnection,
        local_expires_ms: u64,
        now_ms: u64,
        now: Instant,
    ) -> Result<Self, Error> {
        if connection.is_handshaking() {
            return Err(deny("TLS handshake incomplete"));
        }
        full_handshake(connection.handshake_kind(), connection.protocol_version())?;
        if connection.alpn_protocol() != Some(b"h2") {
            return Err(deny("federation requires HTTP/2"));
        }
        store.observe_time(now_ms)?;
        let snapshot = store.snapshot()?;
        let chain = connection
            .peer_certificates()
            .ok_or_else(|| deny("server certificate absent"))?;
        let verified = verify_chain(&snapshot, chain, TlsRole::ServerAuth, now_ms, Some(peer_id))?;
        Self::new(
            &snapshot,
            verified,
            TlsRole::ServerAuth,
            local_expires_ms,
            now_ms,
            now,
        )
    }
}

/// Place directly after Rama's TLS acceptor and before HttpServer. Rama 0.4
/// leaves NegotiatedTlsParameters.peer_certificate_chain empty on inbound
/// rustls streams, but its lossless stream conversion exposes the authenticated
/// rustls connection. No patched dependency or identity header is needed.
#[derive(Clone)]
pub struct BindPeerService<S> {
    store: Arc<Store>,
    local_expires_ms: u64,
    inner: S,
}

impl<S> BindPeerService<S> {
    pub fn new(store: Arc<Store>, local_expires_ms: u64, inner: S) -> Self {
        Self {
            store,
            local_expires_ms,
            inner,
        }
    }
}

impl<S, IO> Service<rama::tls::rustls::server::TlsStream<IO>> for BindPeerService<S>
where
    IO: Io + Unpin + ExtensionsRef + 'static,
    S: Service<rama::tls::rustls::server::TlsStream<IO>, Error: Into<BoxError>>,
{
    type Output = S::Output;
    type Error = BoxError;

    async fn serve(
        &self,
        stream: rama::tls::rustls::server::TlsStream<IO>,
    ) -> Result<Self::Output, Self::Error> {
        let stream: rustls::server::TlsStream<IO> = stream.into();
        let (io, connection) = stream.get_ref();
        let binding = Binding::from_inbound(
            &self.store,
            connection,
            self.local_expires_ms,
            utc_ms()?,
            Instant::now(),
        )?;
        let mut changes = self.store.subscribe();
        binding.authorize(
            &self.store,
            super::super::trust::Action::Authenticate,
            utc_ms()?,
            Instant::now(),
        )?;
        io.extensions().insert(binding.clone());
        let invalidation = async {
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(binding.deadline().into()) => return,
                    changed = changes.changed() => {
                        if changed.is_err() { return; }
                        let Ok(now_ms) = utc_ms() else { return; };
                        if binding.authorize(&self.store, super::super::trust::Action::Authenticate,
                            now_ms, Instant::now()).is_err()
                        {
                            return;
                        }
                    }
                }
            }
        };
        tokio::select! {
            result = self.inner.serve(stream.into()) => result.map_err(Into::into),
            () = invalidation => Err(deny("binding expired or was invalidated").into()),
        }
    }
}
