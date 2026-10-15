//! Private HTTP receipt, captured before protobuf decoding. No caller metadata
//! can manufacture a receipt or reset the ingress clock.
use super::deadline::{Error, Ingress};
use rama::http::{Body, StreamingBody, body::Frame};
use rama::{
    Service,
    extensions::{Extension, ExtensionsRef},
    http::{Request, Response, grpc::Status},
};
use std::{convert::Infallible, sync::Arc};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::Semaphore;

struct LeasedBody {
    inner: Body,
    _charge: Arc<tokio::sync::OwnedSemaphorePermit>,
    eligible: crate::session::tracked::Boundary,
}

impl StreamingBody for LeasedBody {
    type Data = <Body as StreamingBody>::Data;
    type Error = <Body as StreamingBody>::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if !(this.eligible)() {
            return Poll::Ready(Some(Err(rama::error::BoxError::from(
                "federation response authorization expired",
            ))));
        }
        Pin::new(&mut this.inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> rama::http::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Bound the entire connection lifetime, including the TLS handshake and
/// idle/finished-but-undrained HTTP/2 streams. Never await a free slot.
#[derive(Clone)]
pub(crate) struct Connections<S> {
    inner: S,
    slots: Arc<Semaphore>,
}

#[cfg(test)]
mod tests;

impl<S> Connections<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(8)),
        }
    }
}

impl<I: Send + 'static, S> Service<I> for Connections<S>
where
    S: Service<I, Error: Into<rama::error::BoxError>>,
{
    type Output = S::Output;
    type Error = rama::error::BoxError;
    async fn serve(&self, input: I) -> Result<Self::Output, Self::Error> {
        let _permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| rama::error::BoxError::from("federation connection limit exceeded"))?;
        self.inner.serve(input).await.map_err(Into::into)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Receipt(pub(super) Result<Ingress, Error>);
impl Extension for Receipt {}

#[derive(Clone)]
pub(crate) struct Gate<S> {
    inner: S,
    decoding: Arc<Semaphore>,
    /// A model upload streams for minutes; it never holds a decoding permit.
    upload: Arc<Semaphore>,
}

const UPLOAD: &str = "/mdma.federation.v1.Federation/Upload";

impl<S> Gate<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            decoding: Arc::new(Semaphore::new(2)),
            upload: Arc::new(Semaphore::new(1)),
        }
    }
}

/// The response quota and current authorization survive protobuf encoding
/// until the actual HTTP response body is consumed or dropped.
pub(crate) fn lease(response: Response) -> Response {
    let lease = response
        .extensions()
        .get_ref::<super::ResponseLease>()
        .cloned();
    match lease {
        Some(lease) => response.map(|inner| {
            Body::new(LeasedBody {
                inner,
                _charge: lease.charge,
                eligible: lease.eligible,
            })
        }),
        None => response,
    }
}

impl<S> Service<Request> for Gate<S>
where
    S: Service<Request, Output = Response, Error = Infallible>,
{
    type Output = Response;
    type Error = Infallible;

    async fn serve(&self, request: Request) -> Result<Response, Infallible> {
        // All RPC body decoding, including control calls, is bounded. No wait
        // queue precedes identity/rate admission.
        let permits = if request.uri().path().is_some_and(|path| path == UPLOAD) {
            &self.upload
        } else {
            &self.decoding
        };
        let Ok(_permit) = permits.clone().try_acquire_owned() else {
            return Ok(Status::resource_exhausted("limit_exceeded")
                .try_into_http()
                .expect("static gRPC status"));
        };
        let workload = request.uri().path().is_some_and(|path| {
            path == "/mdma.federation.v1.Federation/Synthesize"
                || path == "/mdma.federation.v1.Federation/Decide"
        });
        if !workload {
            return self.inner.serve(request).await;
        }
        let Ok(receipt) = Ingress::capture(request.headers()) else {
            return Ok(Status::invalid_argument("invalid_request")
                .try_into_http()
                .expect("static gRPC status"));
        };
        request.extensions().insert(Receipt(Ok(receipt)));
        // Includes body decoding: a stalled upload cannot hold this permit
        // beyond its fixed transport budget.
        let until = receipt.transport_deadline();
        let response = match tokio::time::timeout_at(until.into(), self.inner.serve(request)).await
        {
            Ok(response) => response?,
            Err(_) => Status::deadline_exceeded("deadline_exceeded")
                .try_into_http()
                .expect("static gRPC status"),
        };
        Ok(lease(response))
    }
}
