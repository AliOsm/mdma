use super::MdmaClient;
use rama::Service;
use rama::error::{BoxError, extra::OpaqueError};
use rama::http::client::EasyHttpConnectorBuilder;
use rama::http::grpc::Status;
use rama::http::{Body, Request, Response};
use rama::net::uri::Uri;
use rama::rt::Executor;
use rama::tls::rustls::client::RustlsClientConfigExt;
use rama::service::BoxService;
use rama::tls::client::{
    ClientAuth, ClientAuthData, TlsClientConfig, TlsServerTrust, TlsServerTrustAnchors,
};
use rustls_pki_types::pem::{Error as PemError, PemObject};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;

pub type Client = MdmaClient<Transport>;

pub struct Transport(BoxService<Request, Response, OpaqueError>);

impl Service<Request> for Transport {
    type Output = Response;
    type Error = Status;

    async fn serve(&self, request: Request) -> Result<Response, Status> {
        self.0
            .serve(request)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))
    }
}

pub(crate) fn origin(url: &str) -> Result<Uri, BoxError> {
    let uri: Uri = url.parse()?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri.userinfo().is_some()
        || uri.path_or_root() != "/"
        || uri.query().is_some()
        || uri.fragment().is_some()
    {
        return Err("MDMA endpoint must be an https://host:port origin without credentials, path, query or fragment".into());
    }
    Ok(uri)
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, BoxError> {
    let certs = CertificateDer::pem_file_iter(path)?.collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", path.display()).into());
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, BoxError> {
    PrivateKeyDer::from_pem_file(path).map_err(|error| match error {
        PemError::NoItemsFound => format!("no private key found in {}", path.display()).into(),
        error => error.into(),
    })
}

/// Loads PEM credentials without connecting. Both peers must trust the other's CA.
pub fn mtls_client(url: &str, cert: &Path, key: &Path, ca: &Path) -> Result<Client, BoxError> {
    let origin = origin(url)?;
    let cert_chain = load_certs(cert)?;
    let private_key = load_private_key(key)?;
    let tls = TlsClientConfig::new()
        .with_alpn_http_auto()
        .with_client_auth(ClientAuth::Single(ClientAuthData {
            private_key,
            cert_chain,
        }))
        .with_server_trust(TlsServerTrust::custom(TlsServerTrustAnchors::try_new(
            load_certs(ca)?,
        )?))
        // The listener can serve explicit federation aliases. A fresh full
        // handshake must authenticate their local disclosure principal; an
        // ordinary TLS session ticket is not current routing authorization.
        .with_modify_rustls_config(|mut config| {
            config.resumption = rama::tls::rustls::dep::rustls::client::Resumption::disabled();
            Ok(config)
        });
    let connector = EasyHttpConnectorBuilder::new()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector::<Body>(Executor::default())
        .without_connection_pool()
        .build_client::<Body, Body, _>();
    Ok(MdmaClient::new(
        Transport(BoxService::new(connector)),
        origin,
    ))
}

#[cfg(test)]
mod tests;
