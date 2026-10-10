//! Native-only admission. No catalog, upstream or default alias can confer an
//! execution grant. The service is still private until runtime activation gates.
use super::{
    deadline::Deadline,
    decision,
    ingress::Receipt,
    ledger::{Claim, Ledger, RequestId},
    native::{self, CompletionTask},
    synthesis,
};
use crate::{
    backend::Backend,
    federation::{
        catalog::{Limits, ModelDescriptor},
        config::{self, Operation, TlsRole},
        control::{DecideRequest, ExecutionEnvelope, SamplerConfig, SynthesizeRequest},
        inventory::{self, Inventory},
        tls::{Binding, utc_ms},
        trust::{Action, Store},
    },
    registry::Entry,
    session::{Session, tracked::Tokens, worker::Admission},
};
use rama::{
    extensions::{Extensions, ExtensionsRef, Ingress},
    http::grpc::{Request, Status},
};
use std::{sync::Arc, time::Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub(super) type Launch = Box<
    dyn FnOnce(
            Claim,
            CancellationToken,
            Arc<OwnedSemaphorePermit>,
            crate::session::tracked::Boundary,
        ) -> (Tokens, CompletionTask)
        + Send,
>;

pub(crate) struct Prepared {
    pub(super) admission: Admission,
    pub(super) launch: Launch,
}

pub(crate) trait Executor: Send + Sync {
    fn descriptor(&self, model: &str) -> Result<ModelDescriptor, Status>;
    fn prepare(
        &self,
        request: &SynthesizeRequest,
        descriptor: &ModelDescriptor,
    ) -> Result<Prepared, Status>;
    fn prepare_decide(
        &self,
        _: &DecideRequest,
        _: &ModelDescriptor,
        _: &crate::session::tracked::Boundary,
    ) -> Result<decision::Prepared, Status> {
        Err(Status::unimplemented("execution_disabled"))
    }
}

pub(crate) struct Native<B: Backend> {
    pub backend: Arc<B>,
    pub inventory: inventory::Native<B>,
    pub decision_budget: crate::memory::NativeMemoryBudget,
}

pub(super) fn native_error(error: &crate::backend::Error) -> Status {
    if error.is_memory_exhausted() || matches!(error, crate::backend::Error::ContextLimit { .. }) {
        Status::resource_exhausted("limit_exceeded")
    } else if matches!(error, crate::backend::Error::ShapeMismatch(_)) {
        Status::invalid_argument("invalid_request")
    } else {
        Status::internal("execution_failed")
    }
}

fn sampler(config: Option<&SamplerConfig>) -> crate::sampler::SamplerConfig {
    let defaults = crate::sampler::SamplerConfig::default();
    let Some(config) = config else {
        return defaults;
    };
    crate::sampler::SamplerConfig {
        temperature: config.temperature.unwrap_or(defaults.temperature),
        top_p: config.top_p.unwrap_or(defaults.top_p),
        min_p: config.min_p.unwrap_or(defaults.min_p),
        repetition_penalty: config
            .repetition_penalty
            .unwrap_or(defaults.repetition_penalty),
        repetition_window: config
            .repetition_window
            .map_or(defaults.repetition_window, |v| v as usize),
    }
}

impl<B> Executor for Native<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    fn prepare_decide(
        &self,
        request: &DecideRequest,
        descriptor: &ModelDescriptor,
        boundary: &crate::session::tracked::Boundary,
    ) -> Result<decision::Prepared, Status> {
        decision::prepare(
            &self.inventory,
            &self.decision_budget,
            request,
            descriptor,
            boundary,
        )
    }
    fn descriptor(&self, model: &str) -> Result<ModelDescriptor, Status> {
        // Permission is checked before calling this method.
        if self.inventory.registry.get(model).is_none() {
            return Err(Status::not_found("model_unavailable"));
        }
        let snapshot = self.inventory.snapshot(&[model.into()]);
        if snapshot.state.forces_unready() {
            return Err(Status::unavailable("not_accepting"));
        }
        snapshot
            .models
            .into_iter()
            .next()
            .ok_or_else(|| Status::failed_precondition("selection_changed"))
    }

    fn prepare(
        &self,
        request: &SynthesizeRequest,
        descriptor: &ModelDescriptor,
    ) -> Result<Prepared, Status> {
        let Limits::Synthesize {
            n_ctx,
            max_output_tokens,
        } = descriptor.limits
        else {
            return Err(Status::failed_precondition("selection_changed"));
        };
        if request.prompt.len() > descriptor.max_input_bytes as usize || request.max_tokens == 0 {
            return Err(Status::invalid_argument("invalid_request"));
        }
        let loaded = self
            .inventory
            .registry
            .get(&descriptor.model)
            .ok_or_else(|| Status::not_found("model_unavailable"))?;
        let Entry::Decoder(decoder) = &loaded.entry else {
            return Err(Status::failed_precondition("selection_changed"));
        };
        let session = Session::new(
            self.backend.clone(),
            decoder.model.clone(),
            decoder.tokenizer.clone(),
            sampler(request.sampler.as_ref()),
            n_ctx as usize,
        );
        let prepared = session
            .prepare_federated(
                &request.prompt,
                request.max_tokens.min(max_output_tokens) as usize,
            )
            .map_err(|error| native_error(&error))?;
        let admission = prepared.admission().clone();
        let guard = self.inventory.shutdown.clone().map(rama::graceful::WeakShutdownGuard::upgrade);
        Ok(Prepared {
            admission,
            launch: Box::new(move |claim, cancel, response_permit, boundary| {
                // Both the response and actual worker own this extra charge.
                // Worker finish does not free a still-undrained response, and
                // dropping a response does not free an unfinished worker.
                native::launch_with_charge(
                    prepared,
                    claim,
                    cancel,
                    guard,
                    response_permit,
                    boundary,
                )
            }),
        })
    }
}

#[derive(Clone)]
pub(crate) struct Service {
    pub(super) store: Arc<Store>,
    pub(super) executor: Arc<dyn Executor>,
    ledger: Arc<Ledger>,
    responses: Arc<Semaphore>,
}

pub(super) struct AdmissionGate {
    pub binding: Binding,
    pub deadline: Deadline,
    pub generation: u64,
    pub descriptor: ModelDescriptor,
    pub claim: Claim,
    pub response_permit: Arc<OwnedSemaphorePermit>,
}

impl Service {
    pub(crate) fn new(store: Arc<Store>, executor: Arc<dyn Executor>) -> Self {
        Self {
            store,
            executor,
            ledger: Ledger::new(),
            responses: Arc::new(Semaphore::new(1)),
        }
    }

    pub(super) fn authorize(
        &self,
        binding: &Binding,
        envelope: &ExecutionEnvelope,
    ) -> Result<u64, Status> {
        self.authorize_operation(binding, envelope, Operation::Synthesize)
    }

    pub(super) fn authorize_operation(
        &self,
        binding: &Binding,
        envelope: &ExecutionEnvelope,
        operation: Operation,
    ) -> Result<u64, Status> {
        let snapshot = binding
            .authorize(
                &self.store,
                Action::Execute {
                    model: &envelope.model,
                    operation,
                },
                utc_ms().map_err(|_| Status::unavailable("not_accepting"))?,
                Instant::now(),
            )
            .map_err(|_| Status::permission_denied("forbidden"))?;
        if snapshot.server_id != envelope.destination {
            return Err(Status::invalid_argument("invalid_request"));
        }
        Ok(snapshot.generation)
    }

    pub(super) fn selection(
        &self,
        envelope: &ExecutionEnvelope,
    ) -> Result<ModelDescriptor, Status> {
        let descriptor = self.executor.descriptor(&envelope.model)?;
        if descriptor.artifact_id != envelope.artifact_id
            || descriptor.profile_id != envelope.profile_id
        {
            return Err(Status::failed_precondition("selection_changed"));
        }
        Ok(descriptor)
    }

    #[expect(clippy::needless_pass_by_value, reason = "owned gRPC request, matching `decide`")]
    pub(crate) fn synthesize(
        &self,
        request: Request<SynthesizeRequest>,
    ) -> Result<synthesis::EventStream, Status> {
        let message = request.get_ref();
        let envelope = message
            .envelope
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("invalid_request"))?;
        let gate = self.begin(&request, envelope, Operation::Synthesize)?;
        let prepared = self.executor.prepare(message, &gate.descriptor)?;
        // Memory is reserved but no device allocation or inference has begun.
        self.final_admission(
            &gate.binding,
            envelope,
            gate.deadline,
            gate.generation,
            &gate.descriptor,
        )?;
        Ok(synthesis::stream(
            self.clone(),
            gate.binding,
            envelope.clone(),
            gate.deadline,
            gate.generation,
            gate.descriptor,
            prepared,
            gate.claim,
            gate.response_permit,
        ))
    }

    pub(super) fn begin<T>(
        &self,
        request: &Request<T>,
        envelope: &ExecutionEnvelope,
        operation: Operation,
    ) -> Result<AdmissionGate, Status> {
        config::validate_model(&envelope.model)
            .map_err(|_| Status::invalid_argument("invalid_request"))?;
        let id = RequestId::parse(&envelope.request_id).map_err(ledger_error)?;
        let ingress = request
            .extensions()
            .get_ref::<Ingress<Extensions>>()
            .ok_or_else(|| Status::unauthenticated("identity_mismatch"))?;
        let binding = ingress
            .0
            .get_ref::<Binding>()
            .ok_or_else(|| Status::unauthenticated("identity_mismatch"))?
            .clone();
        if binding.role() != TlsRole::ClientAuth || binding.server_id() != envelope.origin {
            return Err(Status::unauthenticated("identity_mismatch"));
        }
        let receipt = request
            .extensions()
            .get_ref::<Receipt>()
            .ok_or_else(|| Status::invalid_argument("invalid_request"))?
            .0
            .map_err(|_| Status::invalid_argument("invalid_request"))?;
        let deadline = receipt
            .bind(envelope.timeout_ms, &binding, Instant::now())
            .map_err(deadline_error)?;
        let generation = self.authorize_operation(&binding, envelope, operation)?;
        let snapshot = self
            .store
            .snapshot()
            .map_err(|_| Status::unavailable("not_accepting"))?;
        let limits = &snapshot
            .peer(&envelope.origin)
            .ok_or_else(|| Status::permission_denied("forbidden"))?
            .policy
            .limits;
        // Across both operations: validating work, including tokenization,
        // is charged before any model/input processing.
        let claim = self
            .ledger
            .claim(&envelope.origin, id, limits)
            .map_err(ledger_error)?;
        let response_permit = Arc::new(
            self.responses
                .clone()
                .try_acquire_owned()
                .map_err(|_| Status::resource_exhausted("limit_exceeded"))?,
        );
        let descriptor = self.selection(envelope)?;
        Ok(AdmissionGate {
            binding,
            deadline,
            generation,
            descriptor,
            claim,
            response_permit,
        })
    }

    pub(super) fn final_admission(
        &self,
        binding: &Binding,
        envelope: &ExecutionEnvelope,
        deadline: Deadline,
        generation: u64,
        descriptor: &ModelDescriptor,
    ) -> Result<(), Status> {
        self.final_admission_operation(
            binding,
            envelope,
            deadline,
            generation,
            descriptor,
            Operation::Synthesize,
        )
    }

    pub(super) fn final_admission_operation(
        &self,
        binding: &Binding,
        envelope: &ExecutionEnvelope,
        deadline: Deadline,
        generation: u64,
        descriptor: &ModelDescriptor,
        operation: Operation,
    ) -> Result<(), Status> {
        deadline.remaining(Instant::now()).map_err(deadline_error)?;
        if self.authorize_operation(binding, envelope, operation)? != generation {
            return Err(Status::aborted("authorization_changed"));
        }
        if &self.selection(envelope)? != descriptor {
            return Err(Status::aborted("selection_changed"));
        }
        Ok(())
    }
}

pub(super) fn deadline_error(error: super::deadline::Error) -> Status {
    if error == super::deadline::Error::Expired {
        Status::deadline_exceeded("deadline_exceeded")
    } else {
        Status::invalid_argument("invalid_request")
    }
}

fn ledger_error(error: super::ledger::Error) -> Status {
    use super::ledger::Error;
    match error {
        Error::Duplicate => Status::already_exists("duplicate_request_id"),
        Error::InvalidId | Error::InvalidPolicy => Status::invalid_argument("invalid_request"),
        Error::Unavailable => Status::unavailable("not_accepting"),
        Error::Rate | Error::Full | Error::Concurrency => {
            Status::resource_exhausted("limit_exceeded")
        }
    }
}

#[cfg(test)]
mod tests;
