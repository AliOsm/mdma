//! Treat even an authenticated executor as untrusted protocol input. Success
//! needs the exact pinned identity, complete ordered events AND gRPC OK.
use crate::federation::{
    catalog::{Limits, ModelDescriptor},
    control::{
        Accepted, Completed, DecideRequest, DecideResponse, ExecutionEnvelope, SamplerConfig,
        SynthesizeEvent, SynthesizeRequest, synthesize_event::Event,
    },
};
use rama::http::grpc::Status;

fn failure() -> Status {
    Status::data_loss("federation_protocol_failure")
}

fn identity(
    envelope: &ExecutionEnvelope,
    id: &[u8],
    executor: &str,
    model: &str,
    artifact: &str,
    profile: &str,
) -> bool {
    id == envelope.request_id
        && executor == envelope.destination
        && model == envelope.model
        && artifact == envelope.artifact_id
        && profile == envelope.profile_id
}

fn selected(
    envelope: Option<&ExecutionEnvelope>,
    descriptor: &ModelDescriptor,
) -> Result<ExecutionEnvelope, Status> {
    let envelope = envelope.ok_or_else(failure)?;
    descriptor.validate().map_err(|_| failure())?;
    if envelope.request_id.len() != 16
        || envelope.origin == envelope.destination
        || envelope.model != descriptor.model
        || envelope.artifact_id != descriptor.artifact_id
        || envelope.profile_id != descriptor.profile_id
        || !descriptor.ready
    {
        return Err(failure());
    }
    Ok(envelope.clone())
}

pub(super) fn resolved_sampler(config: Option<&SamplerConfig>) -> SamplerConfig {
    let defaults = crate::sampler::SamplerConfig::default();
    let empty = SamplerConfig::default();
    let config = config.unwrap_or(&empty);
    SamplerConfig {
        temperature: Some(config.temperature.unwrap_or(defaults.temperature)),
        top_p: Some(config.top_p.unwrap_or(defaults.top_p)),
        min_p: Some(config.min_p.unwrap_or(defaults.min_p)),
        repetition_penalty: Some(
            config
                .repetition_penalty
                .unwrap_or(defaults.repetition_penalty),
        ),
        repetition_window: Some(
            config
                .repetition_window
                .unwrap_or(defaults.repetition_window as u32),
        ),
    }
}

fn valid_sampler(config: &SamplerConfig) -> bool {
    config.temperature.is_some_and(|v| v.is_finite() && v >= 0.)
        && config
            .top_p
            .is_some_and(|v| v.is_finite() && v > 0. && v <= 1.)
        && config
            .min_p
            .is_some_and(|v| v.is_finite() && (0. ..=1.).contains(&v))
        && config
            .repetition_penalty
            .is_some_and(|v| v.is_finite() && v > 0.)
        && config.repetition_window.is_some()
}

pub(super) struct Synthesis {
    envelope: ExecutionEnvelope,
    sampler: SamplerConfig,
    request_cap: u32,
    catalog_cap: u32,
    catalog_ctx: u32,
    accepted: Option<Accepted>,
    emitted: u32,
    completed: Option<Completed>,
    failed: bool,
}

impl Synthesis {
    pub(super) fn new(
        request: &SynthesizeRequest,
        descriptor: &ModelDescriptor,
    ) -> Result<Self, Status> {
        let envelope = selected(request.envelope.as_ref(), descriptor)?;
        let Limits::Synthesize {
            n_ctx,
            max_output_tokens,
        } = descriptor.limits
        else {
            return Err(failure());
        };
        let sampler = resolved_sampler(request.sampler.as_ref());
        if !valid_sampler(&sampler)
            || request.max_tokens == 0
            || request.prompt.len() > descriptor.max_input_bytes as usize
        {
            return Err(Status::invalid_argument("invalid_request"));
        }
        Ok(Self {
            envelope,
            sampler,
            request_cap: request.max_tokens,
            catalog_cap: max_output_tokens,
            catalog_ctx: n_ctx,
            accepted: None,
            emitted: 0,
            completed: None,
            failed: false,
        })
    }

    pub(super) fn event(&mut self, wire: &SynthesizeEvent) -> Result<(), Status> {
        if self.failed {
            return Err(failure());
        }
        if let Err(error) = self.check_event(wire) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn check_event(&mut self, wire: &SynthesizeEvent) -> Result<(), Status> {
        use rama::http::grpc::protobuf::prost::Message;
        if self.completed.is_some() || wire.encoded_len() > 64 * 1024 {
            return Err(failure());
        }
        match wire.event.as_ref().ok_or_else(failure)? {
            Event::Accepted(accepted) => {
                if self.accepted.is_some()
                    || !identity(
                        &self.envelope,
                        &accepted.request_id,
                        &accepted.executor,
                        &accepted.model,
                        &accepted.artifact_id,
                        &accepted.profile_id,
                    )
                    || accepted.sampler.as_ref() != Some(&self.sampler)
                    || accepted.prompt_tokens == 0
                    || accepted.n_ctx > self.catalog_ctx
                    || accepted.prompt_tokens >= accepted.n_ctx
                    || accepted.max_tokens == 0
                    || accepted.max_tokens > self.request_cap.min(self.catalog_cap)
                    || accepted.max_tokens > accepted.n_ctx - accepted.prompt_tokens
                {
                    return Err(failure());
                }
                self.accepted = Some(accepted.clone());
            }
            Event::Token(token) => {
                let accepted = self.accepted.as_ref().ok_or_else(failure)?;
                if token.index != self.emitted || self.emitted >= accepted.max_tokens {
                    return Err(failure());
                }
                self.emitted += 1;
            }
            Event::Completed(completed) => {
                let accepted = self.accepted.as_ref().ok_or_else(failure)?;
                // The executor's timings fit within the request's budget.
                if completed.request_id != self.envelope.request_id
                    || completed.emitted_tokens != self.emitted
                    || completed.prefill_ms.saturating_add(completed.decode_ms)
                        > self.envelope.timeout_ms
                {
                    return Err(failure());
                }
                let valid_reason = match completed.finish_reason.as_str() {
                    "eos" => self.emitted < accepted.max_tokens,
                    "max_tokens" => self.emitted == accepted.max_tokens,
                    "context_limit" => {
                        self.emitted == accepted.max_tokens
                            && self.emitted == accepted.n_ctx - accepted.prompt_tokens
                    }
                    _ => false,
                };
                if !valid_reason {
                    return Err(failure());
                }
                self.completed = Some(completed.clone());
            }
        }
        Ok(())
    }

    /// `None` is only obtained from the actual gRPC stream's validated OK EOF,
    /// never from the HTTP connection future or a locally closed channel.
    /// Returns the completion only once the whole stream has validated.
    pub(super) fn finish(self, terminal: Result<(), Status>) -> Result<Completed, Status> {
        terminal?;
        if self.failed {
            return Err(failure());
        }
        self.completed.ok_or_else(failure)
    }
}

mod decision;
pub(super) use decision::Decision;

#[cfg(test)]
mod tests;
