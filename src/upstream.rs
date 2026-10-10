use crate::grpc::client::{Client, mtls_client, origin};
use crate::grpc::{DecideRequest, DecideResponse, IntrospectRequest, ModelStatus, SynthesizeRequest, SynthesizeResponse};
use rama::error::BoxError;
use rama::http::grpc::codec::Streaming;
use rama::http::grpc::metadata::MetadataMap;
use rama::http::grpc::{Request, Response, Status};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const HOPS_HEADER: &str = "x-mdma-hops";
const MAX_HOPS: u8 = 8;
const UPSTREAM_INTROSPECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub url: String,
    /// Empty selects the remote node's default model.
    #[serde(default)]
    pub model: String,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub tls_ca: PathBuf,
}

impl UpstreamConfig {
    pub(crate) fn validate(&self) -> Result<(), BoxError> {
        origin(&self.url)?;
        if !self.model.is_empty() && self.model.trim().is_empty() {
            return Err("upstream model must be a name or an empty string for the remote default".into());
        }
        if [&self.tls_cert, &self.tls_key, &self.tls_ca].iter().any(|p| p.as_os_str().is_empty()) {
            return Err("upstream tls_cert, tls_key and tls_ca paths must not be empty".into());
        }
        Ok(())
    }
}

fn forward<T>(mut request: Request<T>) -> Result<Request<T>, Status> {
    let metadata = request.metadata_mut();
    let mut values = metadata.get_all(HOPS_HEADER).iter();
    let hops = match values.next() {
        Some(value) => {
            value.to_str().ok().and_then(|s| s.parse::<u8>().ok()).ok_or_else(|| Status::invalid_argument("invalid x-mdma-hops"))?
        }
        None => 0,
    };
    if values.next().is_some() {
        return Err(Status::invalid_argument("multiple x-mdma-hops values"));
    }
    if hops >= MAX_HOPS {
        return Err(Status::resource_exhausted("MDMA forwarding hop limit exceeded"));
    }
    metadata.insert(HOPS_HEADER, u16::from(hops + 1).into());
    Ok(request)
}

pub struct Upstream {
    url: String,
    model: String,
    client: Client,
}

impl Upstream {
    pub fn new(config: &UpstreamConfig) -> Result<Self, BoxError> {
        config.validate()?;
        let client = mtls_client(&config.url, &config.tls_cert, &config.tls_key, &config.tls_ca)?;
        Ok(Self { url: config.url.clone(), model: config.model.clone(), client })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn synthesize(&self, mut request: Request<SynthesizeRequest>) -> Result<Response<Streaming<SynthesizeResponse>>, Status> {
        request.get_mut().model.clone_from(&self.model);
        self.client.synthesize(forward(request)?).await
    }

    pub async fn decide(&self, mut request: Request<DecideRequest>) -> Result<Response<DecideResponse>, Status> {
        request.get_mut().model.clone_from(&self.model);
        self.client.decide(forward(request)?).await
    }

    pub async fn introspect(&self, metadata: &MetadataMap) -> Result<ModelStatus, Status> {
        let mut request = Request::new(IntrospectRequest {});
        *request.metadata_mut() = metadata.clone();
        let response = tokio::time::timeout(UPSTREAM_INTROSPECT_TIMEOUT, self.client.introspect(forward(request)?))
            .await
            .map_err(|_| Status::deadline_exceeded("MDMA Introspect check timed out"))??
            .into_inner();
        let name = if self.model.is_empty() { &response.default_model } else { &self.model };
        let mut model = response
            .models
            .into_iter()
            .find(|m| &m.name == name)
            .ok_or_else(|| Status::not_found(format!("upstream has no model named {name:?}")))?;
        model.ready &= response.ready;
        Ok(model)
    }
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream").field("url", &self.url).field("model", &self.model).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
