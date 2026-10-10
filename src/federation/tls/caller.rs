//! Local client TLS facts. These are not federation peer identities and do not
//! impersonate end users at an executor. Metadata never constructs a Caller.
use super::*;
use rama::{
    Service,
    error::BoxError,
    extensions::{Extension, ExtensionsRef},
    io::Io,
};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Caller {
    spki: String,
    deadline: Instant,
    expires_ms: u64,
}
impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Caller").finish_non_exhaustive()
    }
}
impl Extension for Caller {}

impl Caller {
    fn from_inbound(
        connection: &rustls::ServerConnection,
        local_expires_ms: u64,
        now_ms: u64,
        now: Instant,
    ) -> Result<Self, Error> {
        if connection.is_handshaking() {
            return Err(deny("local TLS handshake incomplete"));
        }
        binding::full_handshake(connection.handshake_kind(), connection.protocol_version())?;
        if connection.alpn_protocol() != Some(b"h2") {
            return Err(deny("routing requires HTTP/2"));
        }
        // The normal listener's required WebPKI client verifier has already
        // authenticated possession and chain/EKU. No URI peer identity needed.
        let chain = connection
            .peer_certificates()
            .ok_or_else(|| deny("local mTLS client absent"))?;
        if chain.is_empty() || chain.len() > MAX_CHAIN_CERTS {
            return Err(deny("invalid local client chain"));
        }
        let mut expires_ms = local_expires_ms;
        for cert in chain {
            expires_ms = expires_ms.min(validity(&parse(cert.as_ref())?, now_ms)?);
        }
        let remaining = expires_ms
            .checked_sub(now_ms)
            .filter(|ms| *ms > 0)
            .ok_or_else(|| deny("local client binding expired"))?;
        let deadline = now
            .checked_add(Duration::from_millis(remaining))
            .ok_or_else(|| deny("local client lifetime out of range"))?;
        Ok(Self {
            spki: certificate_spki_sha256(&chain[0])?,
            deadline,
            expires_ms,
        })
    }

    pub(crate) fn authorize(
        &self,
        store: &Store,
        peer_id: &str,
        model: &str,
        operation: super::super::config::Operation,
        now_ms: u64,
        now: Instant,
    ) -> Result<Instant, Error> {
        if now >= self.deadline || now_ms >= self.expires_ms {
            return Err(deny("local client binding expired"));
        }
        let window = store.disclosure_window(&self.spki, peer_id, model, operation, now_ms)?;
        let consent_deadline = now
            .checked_add(Duration::from_millis(window.valid_until_ms - now_ms))
            .ok_or_else(|| deny("disclosure lifetime out of range"))?;
        Ok(self.deadline.min(consent_deadline))
    }
}

/// Directly after the ordinary required-client-auth TLS acceptor. TLS1.2,
/// resumption or invalid credential lifetimes remain eligible for ordinary
/// local RPCs, but do not receive a routing principal.
#[derive(Clone)]
pub struct BindCallerService<S> {
    inner: S,
    expires_ms: u64,
}
impl<S> BindCallerService<S> {
    pub(crate) fn new(inner: S, expires_ms: u64) -> Self {
        Self { inner, expires_ms }
    }
}
impl<S, IO> Service<rama::tls::rustls::server::TlsStream<IO>> for BindCallerService<S>
where
    IO: Io + Unpin + ExtensionsRef + 'static,
    S: Service<rama::tls::rustls::server::TlsStream<IO>, Error: Into<BoxError>>,
{
    type Output = S::Output;
    type Error = BoxError;
    async fn serve(
        &self,
        stream: rama::tls::rustls::server::TlsStream<IO>,
    ) -> Result<Self::Output, BoxError> {
        let stream: rustls::server::TlsStream<IO> = stream.into();
        let (io, connection) = stream.get_ref();
        // Extensions can never be inherited from another TLS connection.
        if io.extensions().contains::<Caller>() {
            return Err("preexisting local routing principal".into());
        }
        if let Ok(caller) = utc_ms().and_then(|now_ms| {
            Caller::from_inbound(connection, self.expires_ms, now_ms, Instant::now())
        }) {
            io.extensions().insert(caller);
        }
        self.inner.serve(stream.into()).await.map_err(Into::into)
    }
}

/// Conservative expiry of the exact normal listener server chain and all
/// configured client trust anchors. No independent caller-supplied lifetime.
pub(crate) fn listener_expiry(
    server_chain: &[CertificateDer<'_>],
    client_roots: &[CertificateDer<'_>],
    now_ms: u64,
) -> Result<u64, Error> {
    if server_chain.is_empty() || client_roots.is_empty() {
        return Err(deny("local credentials absent"));
    }
    server_chain
        .iter()
        .chain(client_roots)
        .try_fold(u64::MAX, |expiry, cert| {
            Ok(expiry.min(validity(&parse(cert.as_ref())?, now_ms)?))
        })
}

#[cfg(test)]
pub(crate) mod tests;
