use super::{Route, Selection, validate};
use crate::{
    federation::{
        catalog_cache::Cache, config::Operation, control, execution::ResponseLease,
        runtime::outbound::Credentials, tls::Binding,
    },
    grpc::{self, TokenStream},
};
use rama::{
    error::BoxError,
    extensions::ExtensionsRef,
    http::grpc::{Request, Response, Status},
    rt::Executor,
};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Router {
    routes: Arc<BTreeMap<String, Route>>,
    credentials: Credentials,
    cache: Cache,
    executor: Executor,
    slots: Arc<Semaphore>,
}
impl Router {
    pub(crate) fn new(
        routes: Vec<Route>,
        credentials: Credentials,
        cache: Cache,
        executor: Executor,
        local_names: &[String],
    ) -> Result<Self, BoxError> {
        if routes.is_empty() || routes.len() > 256 {
            return Err("routing requires 1..256 explicit aliases".into());
        }
        let mut indexed = BTreeMap::new();
        for route in routes {
            route.validate()?;
            if local_names.contains(&route.alias)
                || indexed.insert(route.alias.clone(), route).is_some()
            {
                return Err(
                    "route aliases must be unique and cannot shadow a local/default model".into(),
                );
            }
        }
        Ok(Self {
            routes: Arc::new(indexed),
            credentials,
            cache,
            executor,
            slots: Arc::new(Semaphore::new(1)),
        })
    }

    pub(crate) fn contains(&self, alias: &str) -> bool {
        self.routes.contains_key(alias)
    }

    pub(crate) fn statuses<T>(&self, request: &Request<T>, ready: bool) -> Vec<grpc::ModelStatus> {
        self.routes
            .values()
            .filter_map(|route| {
                let descriptor =
                    Selection::visible(&self.credentials.store, &self.cache, route, request)
                        .ok()?;
                // Peer labels are untrusted; never multiply the catalog size bound
                // by the number of configured aliases or expose its endpoint.
                if descriptor.architecture.len() > 128 || descriptor.backend.len() > 64 {
                    return None;
                }
                Some(grpc::ModelStatus {
                    name: route.alias.clone(),
                    kind: descriptor.kind.as_str().into(),
                    backend: format!("federated/{}", descriptor.backend),
                    architecture: descriptor.architecture,
                    n_ctx: match descriptor.limits {
                        crate::federation::catalog::Limits::Synthesize { n_ctx, .. } => n_ctx,
                        crate::federation::catalog::Limits::Decide { .. } => 0,
                    },
                    ready,
                    weight_bytes: 0,
                    upstream: String::new(),
                })
            })
            .collect()
    }

    fn begin<T>(
        &self,
        alias: &str,
        operation: Operation,
        request: &Request<T>,
    ) -> Result<OperationContext, Status> {
        let route = self
            .routes
            .get(alias)
            .filter(|r| r.operation == operation)
            .ok_or_else(|| Status::permission_denied("route_denied"))?;
        let charge = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("limit_exceeded"))?;
        let selection = Selection::new(&self.credentials.store, &self.cache, route, request)?;
        Ok(OperationContext {
            router: self.clone(),
            selection: Arc::new(selection),
            binding: Arc::new(Mutex::new(None)),
            charge: Arc::new(charge),
        })
    }

    #[expect(clippy::unused_async, reason = "awaited by the gRPC service like the async `decide` route")]
    pub(crate) async fn synthesize(
        &self,
        request: Request<grpc::SynthesizeRequest>,
    ) -> Result<Response<TokenStream>, Status> {
        let operation = self.begin(&request.get_ref().model, Operation::Synthesize, &request)?;
        let input = request.into_inner();
        let mut message = control::SynthesizeRequest {
            envelope: Some(operation.selection.envelope.clone()),
            prompt: input.prompt,
            max_tokens: input.max_tokens,
            sampler: input.sampler.map(|s| control::SamplerConfig {
                temperature: s.temperature,
                top_p: s.top_p,
                min_p: s.min_p,
                repetition_penalty: s.repetition_penalty,
                repetition_window: s.repetition_window,
            }),
        };
        let validator = validate::Synthesis::new(&message, &operation.selection.descriptor)?;
        // Declare the origin's effective-parameter constraints on the wire.
        // A different executor build must not substitute its own defaults and
        // then appear to violate an untransmitted local constraint.
        message.sampler = Some(validate::resolved_sampler(message.sampler.as_ref()));
        let cancel = CancellationToken::new();
        // One event, not an unbounded output collector. A paused downstream
        // backpressures remote HTTP/2 while deadline/revocation supervision
        // continues independently of downstream polling.
        let (send, receive) = mpsc::channel(1);
        let task_operation = operation.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let result = task_operation
                .supervise(
                    &task_cancel,
                    task_operation.generate(message, validator, &send),
                )
                .await;
            // Also bound a terminal event waiting behind a retained token.
            tokio::select! {
                () = task_cancel.cancelled() => {},
                () = tokio::time::sleep_until(task_operation.selection.deadline.at().into()) => {},
                _ = send.send(Message::Terminal(result)) => {},
            }
        });
        let stream = RoutedStream {
            operation,
            cancel,
            receiver: receive,
            ended: false,
        };
        let gate = stream.operation.clone();
        let response = Response::new(Box::pin(stream) as TokenStream);
        let charge = gate.charge.clone();
        response.extensions().insert(ResponseLease::new(
            charge,
            Arc::new(move || gate.authorize().is_ok()),
        ));
        Ok(response)
    }

    pub(crate) async fn decide(
        &self,
        request: Request<grpc::DecideRequest>,
    ) -> Result<Response<grpc::DecideResponse>, Status> {
        let operation = self.begin(&request.get_ref().model, Operation::Decide, &request)?;
        let input = request.into_inner();
        let message = control::DecideRequest {
            envelope: Some(operation.selection.envelope.clone()),
            state_json: input.state_json,
            questions_json: input.questions_json,
        };
        let validator = validate::Decision::new(&message, &operation.selection.descriptor)?;
        let cancel = CancellationToken::new();
        let result = operation
            .supervise(&cancel, operation.predict(message, validator))
            .await?;
        operation.authorize()?;
        let response = Response::new(grpc::DecideResponse {
            result_json: result,
        });
        let gate = operation.clone();
        response.extensions().insert(ResponseLease::new(
            operation.charge.clone(),
            Arc::new(move || gate.authorize().is_ok()),
        ));
        Ok(response)
    }
}

#[derive(Clone)]
struct OperationContext {
    router: Router,
    selection: Arc<Selection>,
    binding: Arc<Mutex<Option<Binding>>>,
    charge: Arc<OwnedSemaphorePermit>,
}
impl OperationContext {
    fn authorize(&self) -> Result<(), Status> {
        let binding = self
            .binding
            .lock()
            .map_err(|_| Status::internal("route_state_failed"))?;
        self.selection
            .authorize(&self.router.credentials.store, binding.as_ref())
    }

    async fn supervise<T>(
        &self,
        cancel: &CancellationToken,
        operation: impl Future<Output = Result<T, Status>>,
    ) -> Result<T, Status> {
        let mut changes = self.router.credentials.store.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let stopping = async {
            match self.router.executor.guard() {
                Some(guard) => guard.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(operation, stopping);
        loop {
            self.authorize()?;
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Status::cancelled("cancelled")),
                () = &mut stopping => return Err(Status::unavailable("not_accepting")),
                () = tokio::time::sleep_until(self.selection.deadline.at().into()) =>
                    return Err(Status::deadline_exceeded("deadline_exceeded")),
                changed = changes.changed() => {
                    changed.map_err(|_| Status::permission_denied("route_denied"))?;
                }
                _ = tick.tick() => {},
                result = &mut operation => {
                    self.authorize()?;
                    return result;
                }
            }
        }
    }

    async fn connect(&self) -> Result<crate::federation::runtime::outbound::Connected, Status> {
        let connected = self
            .router
            .credentials
            .connect(
                &self.selection.route.peer_id,
                self.selection.action(),
                self.router.executor.clone(),
            )
            .await
            .map_err(|_| Status::unavailable("federation_transport_failed"))?;
        if connected.generation != self.selection.generation {
            return Err(Status::permission_denied("route_denied"));
        }
        self.selection
            .authorize(&self.router.credentials.store, Some(&connected.binding))?;
        *self
            .binding
            .lock()
            .map_err(|_| Status::internal("route_state_failed"))? = Some(connected.binding.clone());
        Ok(connected)
    }

    fn envelope(&self, binding: &Binding) -> Result<control::ExecutionEnvelope, Status> {
        self.selection
            .dispatch(&self.router.credentials.store, &self.router.cache, binding)
    }

    async fn generate(
        &self,
        mut message: control::SynthesizeRequest,
        mut validator: validate::Synthesis,
        send: &mpsc::Sender<Message>,
    ) -> Result<(), Status> {
        let connected = self.connect().await?;
        let client = connected.client.with_max_decoding_message_size(64 * 1024);
        let exchange = async {
            let envelope = self.envelope(&connected.binding)?;
            let timeout = envelope.timeout_ms;
            message.envelope = Some(envelope);
            let mut request = Request::new(message);
            request
                .try_set_timeout(Duration::from_millis(u64::from(timeout)))
                .map_err(|_| Status::internal("deadline_failed"))?;
            self.authorize()?;
            // Exactly one send; no status (including no Accepted) permits retry.
            let mut stream = client
                .synthesize(request)
                .await
                .map_err(|status| remote_error(&status))?
                .into_inner();
            while let Some(wire) = stream.next().await {
                self.authorize()?;
                let wire = wire.map_err(|status| remote_error(&status))?;
                validator.event(&wire)?;
                if let Some(control::synthesize_event::Event::Token(token)) = wire.event {
                    send.send(Message::Token(grpc::SynthesizeResponse {
                        token_id: token.token_id,
                        text: token.text,
                    }))
                    .await
                    .map_err(|_| Status::cancelled("cancelled"))?;
                }
            }
            self.authorize()?;
            validator.finish(Ok(()))
        };
        tokio::select! {
            // If both complete together, validated gRPC EOF wins. Merely
            // finishing the HTTP connection can NEVER produce successful EOF.
            biased;
            result = exchange => result,
            () = connected.drive => Err(Status::unavailable("federation_transport_failed")),
        }
    }

    async fn predict(
        &self,
        mut message: control::DecideRequest,
        validator: validate::Decision,
    ) -> Result<String, Status> {
        let connected = self.connect().await?;
        let exchange = async {
            let envelope = self.envelope(&connected.binding)?;
            let timeout = envelope.timeout_ms;
            message.envelope = Some(envelope);
            let mut request = Request::new(message);
            request
                .try_set_timeout(Duration::from_millis(u64::from(timeout)))
                .map_err(|_| Status::internal("deadline_failed"))?;
            self.authorize()?;
            let wire = connected
                .client
                .decide(request)
                .await
                .map_err(|status| remote_error(&status))?
                .into_inner();
            self.authorize()?;
            validator.response(&wire)?;
            Ok(wire.result_json)
        };
        tokio::select! {
            biased;
            result = exchange => result,
            () = connected.drive => Err(Status::unavailable("federation_transport_failed")),
        }
    }
}

fn remote_error(status: &Status) -> Status {
    // Preserve partial-result/non-OK category, not remote payload/path text.
    Status::new(status.code(), "federation_execution_failed")
}

enum Message {
    Token(grpc::SynthesizeResponse),
    Terminal(Result<(), Status>),
}
struct RoutedStream {
    operation: OperationContext,
    cancel: CancellationToken,
    receiver: mpsc::Receiver<Message>,
    ended: bool,
}
impl Stream for RoutedStream {
    type Item = Result<grpc::SynthesizeResponse, Status>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        if let Err(error) = this.operation.authorize() {
            this.ended = true;
            this.cancel.cancel();
            return Poll::Ready(Some(Err(error)));
        }
        match this.receiver.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Message::Token(token))) => Poll::Ready(Some(Ok(token))),
            Poll::Ready(Some(Message::Terminal(result))) => {
                this.ended = true;
                this.cancel.cancel();
                Poll::Ready(result.err().map(Err))
            }
            Poll::Ready(None) => {
                this.ended = true;
                this.cancel.cancel();
                Poll::Ready(Some(Err(Status::internal("route_task_failed"))))
            }
        }
    }
}
impl Drop for RoutedStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests;
