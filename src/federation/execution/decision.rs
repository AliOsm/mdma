//! Local CPU Laya only. Preparation, scratch charge and actual joined worker
//! lifetime are shared with synthesis accounting, not upstream delegation.
use super::admission::{Service, deadline_error, native_error};
use super::ledger::Outcome;
use crate::{
    backend::Backend,
    federation::{
        catalog::{self, Limits, ModelDescriptor},
        config::Operation,
        control::{DecideRequest, DecideResponse},
        inventory,
    },
    memory::NativeMemoryBudget,
    registry::Entry,
    session::tracked::Boundary,
};
use rama::{
    extensions::Extension,
    http::grpc::{Request, Status},
};
use std::{
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

type Job = Box<dyn FnOnce(CancellationToken, Boundary) -> Result<Option<String>, Status> + Send>;
pub(crate) struct Prepared {
    job: Job,
}

#[derive(Clone)]
pub(crate) struct ResponseLease {
    pub(super) charge: Arc<OwnedSemaphorePermit>,
    pub(super) eligible: Boundary,
}
impl std::fmt::Debug for ResponseLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseLease").finish_non_exhaustive()
    }
}
impl Extension for ResponseLease {}
impl ResponseLease {
    pub(crate) fn new(charge: Arc<OwnedSemaphorePermit>, eligible: Boundary) -> Self {
        Self { charge, eligible }
    }
}

#[derive(Debug)]
pub(crate) struct Decided {
    pub response: DecideResponse,
    pub lease: ResponseLease,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn laya_error(error: crate::laya::Error) -> Status {
    match error {
        crate::laya::Error::Question(_) | crate::laya::Error::Json(_) => {
            Status::invalid_argument("invalid_request")
        }
        crate::laya::Error::Backend(error) if error.is_memory_exhausted() => {
            Status::resource_exhausted("limit_exceeded")
        }
        _ => Status::internal("execution_failed"),
    }
}

pub(super) fn prepare<B: Backend>(
    native: &inventory::Native<B>,
    budget: &NativeMemoryBudget,
    request: &DecideRequest,
    descriptor: &ModelDescriptor,
    boundary: &Boundary,
) -> Result<Prepared, Status> {
    let Limits::Decide {
        max_questions,
        max_result_bytes,
    } = descriptor.limits
    else {
        return Err(Status::failed_precondition("selection_changed"));
    };
    if request
        .state_json
        .len()
        .saturating_add(request.questions_json.len())
        > descriptor.max_input_bytes as usize
    {
        return Err(Status::invalid_argument("invalid_request"));
    }
    let loaded = native
        .registry
        .get(&descriptor.model)
        .ok_or_else(|| Status::not_found("model_unavailable"))?;
    let Entry::Laya(predictor) = &loaded.entry else {
        return Err(Status::failed_precondition("selection_changed"));
    };
    let state = serde_json::from_str(&request.state_json)
        .map_err(|_| Status::invalid_argument("invalid_request"))?;
    let questions = serde_json::from_str::<serde_json::Value>(&request.questions_json)
        .map_err(|_| Status::invalid_argument("invalid_request"))?;
    let serde_json::Value::Object(questions) = questions else {
        return Err(Status::invalid_argument("invalid_request"));
    };
    if questions.len() > max_questions as usize {
        return Err(Status::invalid_argument("invalid_request"));
    }
    let prepared = predictor
        .prepare_checked(&state, &questions, || boundary())
        .map_err(laya_error)?
        .ok_or_else(|| Status::cancelled("cancelled"))?;
    let bytes = predictor
        .prepared_workspace_bytes(&prepared)
        .map_err(laya_error)?;
    let reservation = budget.reserve(bytes).map_err(|error| native_error(&error))?;
    let ids: Vec<_> = questions.into_iter().map(|(id, _)| id).collect();
    let predictor = predictor.clone();
    let guard = native.shutdown.clone().map(rama::graceful::WeakShutdownGuard::upgrade);
    Ok(Prepared {
        job: Box::new(move |cancel, boundary| {
            let _reservation = reservation;
            let _guard = guard;
            let eligible = || !cancel.is_cancelled() && boundary();
            let result = predictor
                .predict_prepared(&prepared, &ids, eligible)
                .map_err(laya_error)?;
            let Some(result) = result else {
                return Ok(None);
            };
            let result = encode(&result, max_result_bytes as usize)?;
            if !eligible() {
                return Ok(None);
            }
            Ok(Some(result))
        }),
    })
}

// Never serialize an oversized result into an unbounded String first. The
// result Value is already bounded by validated questions/checkpoint geometry.
fn encode(value: &serde_json::Value, limit: usize) -> Result<String, Status> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("result limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let limit = limit.min(catalog::MAX_DECIDE_RESULT_BYTES as usize);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit)
        .map_err(|_| Status::resource_exhausted("limit_exceeded"))?;
    let mut output = Bounded { bytes, limit };
    serde_json::to_writer(&mut output, value)
        .map_err(|_| Status::resource_exhausted("limit_exceeded"))?;
    String::from_utf8(output.bytes).map_err(|_| Status::internal("execution_failed"))
}

impl Service {
    pub(crate) async fn decide(&self, request: Request<DecideRequest>) -> Result<Decided, Status> {
        let message = request.get_ref();
        let envelope = message
            .envelope
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("invalid_request"))?;
        let gate = self.begin(&request, envelope, Operation::Decide)?;
        let cancel = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let service = self.clone();
        let binding = gate.binding.clone();
        let selected = envelope.clone();
        let deadline = gate.deadline;
        let boundary: Boundary = Arc::new(move || {
            deadline.remaining(Instant::now()).is_ok()
                && service
                    .authorize_operation(&binding, &selected, Operation::Decide)
                    .is_ok()
        });
        let prepared = self
            .executor
            .prepare_decide(message, &gate.descriptor, &boundary);
        deadline.remaining(Instant::now()).map_err(deadline_error)?;
        self.authorize_operation(&gate.binding, envelope, Operation::Decide)?;
        let prepared = prepared?;
        self.final_admission_operation(
            &gate.binding,
            envelope,
            deadline,
            gate.generation,
            &gate.descriptor,
            Operation::Decide,
        )?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let job = prepared.job;
        let worker_cancel = cancel.clone();
        let worker_charge = gate.response_permit.clone();
        let output_boundary = boundary.clone();
        let mut worker = gate
            .claim
            .spawn_dedicated(move || {
                let result = job(worker_cancel, boundary);
                drop(worker_charge);
                let outcome = match &result {
                    Ok(Some(_)) => Outcome::Completed,
                    Ok(None) => Outcome::Cancelled,
                    Err(_) => Outcome::Failed,
                };
                let _ = send.send(result);
                outcome
            })
            .map_err(|_| Status::internal("execution_failed"))?;
        let mut changes = self.store.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline.at().into()) =>
                    return Err(Status::deadline_exceeded("deadline_exceeded")),
                _ = changes.changed() => {},
                _ = tick.tick() => {},
                joined = &mut worker => {
                    let outcome = joined.map_err(|_| Status::internal("execution_failed"))?;
                    let report = receive.await.map_err(|_| Status::internal("execution_failed"))?;
                    deadline.remaining(Instant::now()).map_err(deadline_error)?;
                    self.authorize_operation(&gate.binding, envelope, Operation::Decide)?;
                    let result = report?;
                    let Some(result_json) = result else { return Err(Status::cancelled("cancelled")); };
                    if outcome != Outcome::Completed { return Err(Status::internal("execution_failed")); }
                    return Ok(Decided {
                        response: DecideResponse { request_id: envelope.request_id.clone(),
                            executor: envelope.destination.clone(), model: envelope.model.clone(),
                            artifact_id: envelope.artifact_id.clone(), profile_id: envelope.profile_id.clone(), result_json },
                        lease: ResponseLease { charge: gate.response_permit, eligible: output_boundary },
                    });
                }
            }
            deadline.remaining(Instant::now()).map_err(deadline_error)?;
            self.authorize_operation(&gate.binding, envelope, Operation::Decide)?;
        }
    }
}

#[cfg(test)]
mod tests;
