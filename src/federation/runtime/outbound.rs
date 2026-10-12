//! One fresh, directly dialed control connection per poll. The HTTP/2
//! connection future is owned by the operation, not detached or pooled.
use crate::federation::{
    catalog::{self, CatalogBody},
    catalog_cache::{Cache, POLL_TIMEOUT},
    control::{FederationClient, ScanCapabilitiesRequest},
    tls::{self, Binding},
    trust::{Action, Store},
};
use rama::dns::client::DnsConnector;
use rama::http::core::client::conn::http2;
use rama::net::{client::ConnectRequest, uri::Uri};
use rama::tcp::client::service::TcpConnector;
use rama::tls::rustls::{client::TlsStream, dep::tokio_rustls::TlsConnector};
use rama::{
    Service,
    error::BoxError,
    http::{Body, Request, Response, grpc::Status},
    rt::Executor,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::{future::Future, pin::Pin, sync::Arc, time::Instant};

pub(super) struct Stage {
    pub name: &'static str,
    source: BoxError,
}
impl std::fmt::Debug for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}
impl std::error::Error for Stage {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}
impl Stage {
    /// Static OS failure classes only; never expose error messages or endpoints.
    pub(super) fn io_kind(&self) -> Option<std::io::ErrorKind> {
        let mut error: &(dyn std::error::Error + 'static) = &*self.source;
        for _ in 0..16 {
            if let Some(io) = error.downcast_ref::<std::io::Error>() {
                return Some(io.kind());
            }
            error = error.source()?;
        }
        None
    }
}
/// Whether `error` is a failure to open the TCP connection: the peer is down,
/// filtered, or the OS refused the dial.
pub(crate) fn connect_failed(error: &BoxError) -> bool {
    error
        .downcast_ref::<Stage>()
        .is_some_and(|stage| stage.name == "tcp_connect")
}

fn stage(name: &'static str, error: impl Into<BoxError>) -> BoxError {
    Box::new(Stage {
        name,
        source: error.into(),
    })
}

#[derive(Clone)]
pub(crate) struct Credentials {
    pub store: Arc<Store>,
    pub chain: Vec<CertificateDer<'static>>,
    pub key: Arc<PrivateKeyDer<'static>>,
    pub signing_key: [u8; 32],
    /// From the exact loaded local chain AND explicitly pinned issuer.
    pub local_expires_ms: u64,
}

pub(crate) struct Transport(http2::SendRequest<Body>);

pub(crate) struct Connected {
    pub client: FederationClient<Transport>,
    pub binding: Binding,
    pub generation: u64,
    /// Never detached or pooled. Dropping the owning operation closes IO.
    pub drive: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Service<Request> for Transport {
    type Output = Response;
    type Error = Status;
    async fn serve(&self, request: Request) -> Result<Response, Status> {
        let mut sender = self.0.clone();
        sender
            .ready()
            .await
            .map_err(|_| Status::unavailable("control connection closed"))?;
        sender
            .send_request(request)
            .await
            .map(|r| r.map(Body::new))
            .map_err(|_| Status::unavailable("control transport failed"))
    }
}

impl Credentials {
    pub async fn poll(
        &self,
        cache: &Cache,
        peer_id: &str,
        executor: Executor,
    ) -> Result<CatalogBody, BoxError> {
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut changes = self.store.subscribe();
        let operation = self.exchange(cache, peer_id, executor, deadline);
        tokio::select! {
            biased;
            _ = changes.changed() => Err("trust changed during catalog poll".into()),
            result = tokio::time::timeout_at(deadline.into(), operation) =>
                result.map_err(|_| stage("poll_timeout", "federation catalog poll timed out"))?,
        }
    }

    async fn exchange(
        &self,
        cache: &Cache,
        peer_id: &str,
        executor: Executor,
        deadline: Instant,
    ) -> Result<CatalogBody, BoxError> {
        let connected = self.connect(peer_id, Action::PollCatalog, executor).await?;
        let poll = cache.begin_until(
            &self.store,
            &connected.binding,
            tls::utc_ms()?,
            Instant::now(),
            deadline,
        )?;
        let snapshot = connected.binding.authorize(
            &self.store,
            Action::PollCatalog,
            tls::utc_ms()?,
            Instant::now(),
        )?;
        let client = connected
            .client
            .with_max_decoding_message_size(catalog::MAX_BODY_JSON_BYTES + 512)
            .with_max_encoding_message_size(4096);
        let exchange = async {
            let response = client
                .scan_capabilities(ScanCapabilitiesRequest {
                    origin: snapshot.server_id,
                    destination: peer_id.into(),
                    major_version: 1,
                    nonce: poll.nonce().to_vec(),
                })
                .await
                .map_err(|error| stage("catalog_rpc", error))?
                .into_inner();
            Ok(poll.finish(&self.store, response, tls::utc_ms()?, Instant::now())?)
        };
        if self.store.snapshot()?.generation != connected.generation {
            return Err("trust changed before control dispatch".into());
        }
        tokio::select! {
            result = exchange => result,
            () = connected.drive => Err("control connection ended before catalog completion".into()),
            () = tokio::time::sleep_until(connected.binding.deadline().into()) =>
                Err("control binding expired during poll".into()),
        }
    }

    /// One fresh authenticated connection to an explicitly enrolled endpoint.
    /// The caller owns deadline/invalidation supervision around the entire dial.
    pub(crate) async fn connect(
        &self,
        peer_id: &str,
        action: Action<'_>,
        executor: Executor,
    ) -> Result<Connected, BoxError> {
        let snapshot = self
            .store
            .authorize(peer_id, Action::Authenticate, tls::utc_ms()?)?;
        let peer = &snapshot.peer(peer_id).ok_or("peer disappeared")?.policy;
        peer.validate()?;
        let endpoint: Uri = peer.endpoint.parse()?;
        let config = tls::client_config(
            self.store.clone(),
            peer_id.into(),
            self.chain.clone(),
            self.key.clone_key(),
            &self.signing_key,
        )?;
        let input = ConnectRequest::new(
            endpoint
                .authority()
                .ok_or("peer endpoint authority absent")?
                .to_string()
                .parse()?,
        );
        // Rama's native DNS + bounded TCP connector; no proxy, discovery,
        // redirect, pooling or plaintext fallback. DNS is not enrollment.
        let connector =
            DnsConnector::new(TcpConnector::new().with_max_in_flight_connect_attempts(1));
        let stream = connector
            .serve(input)
            .await
            .map_err(|error| stage("tcp_connect", error))?
            .conn;
        let stream = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from(peer.tls_name.clone())?, stream)
            .await
            .map_err(|error| stage("tls_handshake", error))?;
        // Capture the actual completed rustls connection before handing it to
        // HTTP. Neither negotiated metadata nor Identify can mint this binding.
        let binding = Binding::from_outbound(
            &self.store,
            peer_id,
            stream.get_ref().1,
            self.local_expires_ms,
            tls::utc_ms()?,
            Instant::now(),
        )
        .map_err(|error| stage("outbound_binding", error))?;
        let current = binding.authorize(&self.store, action, tls::utc_ms()?, Instant::now())?;
        if current.generation != snapshot.generation {
            return Err("trust changed while dialing control endpoint".into());
        }
        let mut builder = http2::Builder::new(executor);
        builder.set_enable_push(false);
        builder.set_max_concurrent_streams(1);
        builder.set_max_header_list_size(16 * 1024);
        let (sender, connection) = builder.handshake(TlsStream::new(stream)).await?;
        let client = FederationClient::new(Transport(sender), endpoint)
            .with_max_decoding_message_size(catalog::MAX_DECIDE_RESULT_BYTES as usize + 2048)
            .with_max_encoding_message_size(96 * 1024);
        if self.store.snapshot()?.generation != current.generation {
            return Err("trust changed during HTTP/2 negotiation".into());
        }
        Ok(Connected {
            client,
            binding,
            generation: current.generation,
            drive: Box::pin(async move {
                let _ = connection.await;
            }),
        })
    }
}

#[cfg(test)]
mod tests;
