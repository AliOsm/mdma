//! Normal client listener integration. The default policy is a pass-through;
//! routing captures transport facts without changing ordinary local semantics.
use crate::federation::{execution, routing::selection::Receipt, tls};
use rama::{
    Service,
    error::BoxError,
    extensions::ExtensionsRef,
    http::{Request, Response, grpc::Status},
};
use std::{convert::Infallible, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone, Copy, Default)]
pub struct ClientPolicy {
    pub(super) routing: bool,
    pub(super) expires_ms: u64,
}
impl ClientPolicy {
    pub fn http_service<S>(self, inner: S) -> ClientGate<S> {
        ClientGate {
            inner,
            enabled: self.routing,
            decoding: Arc::new(Semaphore::new(2)),
        }
    }
    pub fn tls_service<S>(self, inner: S) -> tls::BindCallerService<S> {
        tls::BindCallerService::new(inner, self.expires_ms)
    }
    pub fn connection_service<S>(self, inner: S) -> ClientConnections<S> {
        ClientConnections {
            inner,
            enabled: self.routing,
            slots: Arc::new(Semaphore::new(8)),
        }
    }
}

#[derive(Clone)]
pub struct ClientConnections<S> {
    inner: S,
    enabled: bool,
    slots: Arc<Semaphore>,
}
impl<I: Send + 'static, S> Service<I> for ClientConnections<S>
where
    S: Service<I, Error: Into<BoxError>>,
{
    type Output = S::Output;
    type Error = BoxError;
    async fn serve(&self, input: I) -> Result<Self::Output, Self::Error> {
        let _permit = if self.enabled {
            Some(
                self.slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| BoxError::from("client connection limit exceeded"))?,
            )
        } else {
            None
        };
        self.inner.serve(input).await.map_err(Into::into)
    }
}

#[derive(Clone)]
pub struct ClientGate<S> {
    inner: S,
    enabled: bool,
    decoding: Arc<Semaphore>,
}
impl<S> Service<Request> for ClientGate<S>
where
    S: Service<Request, Output = Response, Error = Infallible>,
{
    type Output = Response;
    type Error = Infallible;
    async fn serve(&self, request: Request) -> Result<Response, Infallible> {
        if !self.enabled {
            return self.inner.serve(request).await;
        }
        let Ok(_permit) = self.decoding.clone().try_acquire_owned() else {
            return Ok(Status::resource_exhausted("limit_exceeded")
                .try_into_http()
                .expect("static gRPC status"));
        };
        let receipt = execution::deadline::Ingress::capture(request.headers());
        request.extensions().insert(Receipt(receipt));
        // Local RPCs still allow an absent deadline. Routed handlers require
        // this private receipt to contain a valid deadline. A supplied valid
        // deadline includes decoding instead of resetting at handler entry.
        let response = match receipt {
            Ok(receipt) => match tokio::time::timeout_at(
                receipt.transport_deadline().into(),
                self.inner.serve(request),
            )
            .await
            {
                Ok(response) => response?,
                Err(_) => Status::deadline_exceeded("deadline_exceeded")
                    .try_into_http()
                    .expect("static gRPC status"),
            },
            Err(_) => self.inner.serve(request).await?,
        };
        Ok(execution::ingress::lease(response))
    }
}

#[cfg(test)]
mod tests;
