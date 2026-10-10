//! Accepted-first streaming, with actual worker join as the only successful
//! completion barrier. The independent supervisor runs even when unpolled.
use super::{
    admission::{Prepared, Service, deadline_error, native_error},
    deadline::Deadline,
    ledger::Claim,
    native::CompletionTask,
};
use crate::{
    federation::{
        catalog::ModelDescriptor,
        control::{
            Accepted, Completed, ExecutionEnvelope, SamplerConfig, SynthesizeEvent, Token,
            synthesize_event::Event,
        },
        tls::Binding,
    },
    session::{
        tracked::Tokens,
        worker::{FinishReason, WorkerEnd},
    },
};
use rama::http::grpc::Status;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;

pub(crate) type EventStream =
    Pin<Box<dyn Stream<Item = Result<SynthesizeEvent, Status>> + Send + Sync + 'static>>;

// Rama requires Sync bodies; the mutable poll state includes a FnOnce job and
// an opaque join future. Serialize only poll_next, never block/await this lock.
struct StreamBody(Arc<std::sync::Mutex<Synthesis>>);
impl Stream for StreamBody {
    type Item = Result<SynthesizeEvent, Status>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Ok(mut stream) = self.0.lock() else {
            return Poll::Ready(Some(Err(Status::internal("execution_failed"))));
        };
        Pin::new(&mut *stream).poll_next(cx)
    }
}

pub(super) struct Supervisor {
    service: Service,
    binding: Binding,
    envelope: ExecutionEnvelope,
    deadline: Deadline,
    cancel: CancellationToken,
    stop: CancellationToken,
    status: watch::Receiver<Option<Status>>,
}

impl Supervisor {
    fn new(
        service: Service,
        binding: Binding,
        envelope: ExecutionEnvelope,
        deadline: Deadline,
    ) -> Self {
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let (_, status) = watch::channel(None);
        Self {
            service,
            binding,
            envelope,
            deadline,
            cancel,
            stop,
            status,
        }
    }

    fn start(&mut self, state: std::sync::Weak<std::sync::Mutex<Synthesis>>) {
        let service = self.service.clone();
        let deadline = self.deadline;
        let (send, status) = watch::channel(None);
        self.status = status.clone();
        let mut changes = service.store.subscribe();
        let monitor = Self {
            service,
            binding: self.binding.clone(),
            envelope: self.envelope.clone(),
            deadline,
            cancel: self.cancel.clone(),
            stop: self.stop.clone(),
            status,
        };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    biased;
                    () = monitor.stop.cancelled() => break,
                    () = tokio::time::sleep_until(deadline.at().into()) => {},
                    _ = changes.changed() => {},
                    _ = tick.tick() => {},
                }
                if let Err(error) = monitor.check() {
                    let _ = send.send(Some(error));
                    monitor.cancel.cancel();
                    if let Some(state) = state.upgrade()
                        && let Ok(mut stream) = state.lock()
                        && !matches!(stream.state, State::End)
                    {
                        // Expiry/revocation must release prelaunch
                        // reservations even if the body is never
                        // polled. Running jobs retain their own charge.
                        stream.state = State::Invalidated;
                    }
                    break;
                }
            }
            // Avoid monitor Drop stopping a still-valid stream.
            // All clones here share cancellation state, so no custom Drop.
        });
    }

    fn check(&self) -> Result<(), Status> {
        let status = self.status.borrow().clone();
        if let Some(error) = status {
            return Err(error);
        }
        self.deadline
            .remaining(Instant::now())
            .map_err(deadline_error)?;
        self.service.authorize(&self.binding, &self.envelope)?;
        Ok(())
    }
}

type Finish = Pin<Box<dyn Future<Output = crate::backend::Result<WorkerEnd>> + Send>>;

enum State {
    Admitted {
        accepted: Accepted,
        prepared: Prepared,
        claim: Claim,
    },
    Launch {
        prepared: Prepared,
        claim: Claim,
    },
    Running {
        tokens: Tokens,
        completion: CompletionTask,
    },
    Joining(Finish),
    VerifyEnd,
    Invalidated,
    End,
}

struct Synthesis {
    supervisor: Supervisor,
    generation: u64,
    descriptor: ModelDescriptor,
    response_permit: Arc<OwnedSemaphorePermit>,
    state: State,
    emitted: u32,
    yield_pending: bool,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Drop for Synthesis {
    fn drop(&mut self) {
        self.supervisor.cancel.cancel();
        self.supervisor.stop.cancel();
    }
}

impl Stream for Synthesis {
    type Item = Result<SynthesizeEvent, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if matches!(this.state, State::End) {
            return Poll::Ready(None);
        }
        // Register even during an outstanding kernel operation or job join.
        let _ = this.cancelled.as_mut().poll(cx);
        // Check even after Completed, before allowing gRPC OK/EOF.
        if let Err(error) = this.supervisor.check() {
            this.supervisor.cancel.cancel();
            this.state = State::End;
            return Poll::Ready(Some(Err(error)));
        }
        if this.yield_pending {
            this.yield_pending = false;
            cx.waker().wake_by_ref();
            // Force Rama's gRPC encoder to flush after EVERY event, not batch
            // arbitrarily many tiny tokens up to its byte-yield threshold.
            return Poll::Pending;
        }
        loop {
            let state = std::mem::replace(&mut this.state, State::End);
            let event = match state {
                State::Admitted {
                    accepted,
                    prepared,
                    claim,
                } => {
                    if let Err(error) = this.before_launch() {
                        return Poll::Ready(Some(Err(error)));
                    }
                    this.state = State::Launch { prepared, claim };
                    Event::Accepted(accepted)
                }
                State::Launch { prepared, claim } => {
                    if let Err(error) = this.before_launch() {
                        return Poll::Ready(Some(Err(error)));
                    }
                    let service = this.supervisor.service.clone();
                    let binding = this.supervisor.binding.clone();
                    let envelope = this.supervisor.envelope.clone();
                    let deadline = this.supervisor.deadline;
                    let boundary = Arc::new(move || {
                        deadline.remaining(Instant::now()).is_ok()
                            && service.authorize(&binding, &envelope).is_ok()
                    });
                    let (tokens, completion) = (prepared.launch)(
                        claim,
                        this.supervisor.cancel.clone(),
                        this.response_permit.clone(),
                        boundary,
                    );
                    this.state = State::Running { tokens, completion };
                    continue;
                }
                State::Running {
                    mut tokens,
                    completion,
                } => match Pin::new(&mut tokens).poll_next(cx) {
                    Poll::Pending => {
                        this.state = State::Running { tokens, completion };
                        return Poll::Pending;
                    }
                    Poll::Ready(Some(token)) => {
                        let index = this.emitted;
                        this.emitted += 1;
                        let token = Token {
                            index,
                            token_id: token.token().token_id,
                            text: token.token().text.clone(),
                        };
                        this.state = State::Running { tokens, completion };
                        Event::Token(token)
                    }
                    Poll::Ready(None) => {
                        this.state = State::Joining(Box::pin(completion.finish()));
                        continue;
                    }
                },
                State::Joining(mut finish) => match finish.as_mut().poll(cx) {
                    Poll::Pending => {
                        this.state = State::Joining(finish);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Some(Err(native_error(&error)))),
                    Poll::Ready(Ok(WorkerEnd::Cancelled)) => {
                        return Poll::Ready(Some(Err(Status::cancelled("cancelled"))));
                    }
                    Poll::Ready(Ok(WorkerEnd::Completed(completion))) => {
                        if completion.emitted != this.emitted as usize {
                            return Poll::Ready(Some(Err(Status::data_loss("protocol_violation"))));
                        }
                        if let Err(error) = this.supervisor.check() {
                            return Poll::Ready(Some(Err(error)));
                        }
                        this.state = State::VerifyEnd;
                        Event::Completed(Completed {
                            request_id: this.supervisor.envelope.request_id.clone(),
                            emitted_tokens: this.emitted,
                            finish_reason: match completion.reason {
                                FinishReason::Eos => "eos",
                                FinishReason::MaxTokens => "max_tokens",
                                FinishReason::ContextLimit => "context_limit",
                            }
                            .into(),
                        })
                    }
                },
                State::Invalidated => {
                    return Poll::Ready(Some(Err(Status::cancelled("cancelled"))));
                }
                State::End | State::VerifyEnd => return Poll::Ready(None),
            };
            this.yield_pending = true;
            return Poll::Ready(Some(Ok(SynthesizeEvent { event: Some(event) })));
        }
    }
}

impl Synthesis {
    fn before_launch(&self) -> Result<(), Status> {
        self.supervisor.check()?;
        self.supervisor.service.final_admission(
            &self.supervisor.binding,
            &self.supervisor.envelope,
            self.supervisor.deadline,
            self.generation,
            &self.descriptor,
        )
    }
}

#[expect(clippy::too_many_arguments, reason = "assembles one joined stream from admission outputs")]
pub(super) fn stream(
    service: Service,
    binding: Binding,
    envelope: ExecutionEnvelope,
    deadline: Deadline,
    generation: u64,
    descriptor: ModelDescriptor,
    prepared: Prepared,
    claim: Claim,
    response_permit: Arc<OwnedSemaphorePermit>,
) -> EventStream {
    let admission = &prepared.admission;
    let s = &admission.sampler;
    let accepted = Accepted {
        request_id: envelope.request_id.clone(),
        executor: envelope.destination.clone(),
        model: envelope.model.clone(),
        artifact_id: envelope.artifact_id.clone(),
        profile_id: envelope.profile_id.clone(),
        sampler: Some(SamplerConfig {
            temperature: Some(s.temperature),
            top_p: Some(s.top_p),
            min_p: Some(s.min_p),
            repetition_penalty: Some(s.repetition_penalty),
            repetition_window: Some(s.repetition_window as u32),
        }),
        prompt_tokens: admission.prompt_tokens as u32,
        max_tokens: admission.max_tokens as u32,
        n_ctx: admission.n_ctx as u32,
    };
    let supervisor = Supervisor::new(service, binding, envelope, deadline);
    let cancelled = Box::pin(supervisor.cancel.clone().cancelled_owned());
    let state = Arc::new(std::sync::Mutex::new(Synthesis {
        supervisor,
        generation,
        descriptor,
        cancelled,
        response_permit,
        state: State::Admitted {
            accepted,
            prepared,
            claim,
        },
        emitted: 0,
        yield_pending: false,
    }));
    state
        .lock()
        .expect("new synthesis state")
        .supervisor
        .start(Arc::downgrade(&state));
    Box::pin(StreamBody(state))
}
