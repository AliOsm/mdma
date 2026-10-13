//! Recipient-filtered, local-only inventory. Constructors are crate-private:
//! callers cannot inject descriptors or claim an execution backend.

use super::catalog::{self, Kind, Limits, ModelDescriptor, ServerState};
use crate::backend::Backend;
use crate::grpc::Host;
use crate::power::{DegradedLimits, PowerStatus};
use crate::registry::{Phase, Registry};
use rama::futures::FutureExt;
use rama::graceful::WeakShutdownGuard;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub state: ServerState,
    pub models: Vec<ModelDescriptor>,
    pub host: Host,
}

impl Snapshot {
    /// The same state and models; live host figures and residency may differ.
    pub(crate) fn same_offer(&self, other: &Self) -> bool {
        self.state == other.state
            && self.models.len() == other.models.len()
            && self
                .models
                .iter()
                .zip(&other.models)
                .all(|(a, b)| a.same_selection(b))
    }
}

pub(crate) trait Inventory: Send + Sync {
    fn snapshot(&self, exports: &[String]) -> Snapshot;
}

pub(crate) fn draining(guard: Option<&WeakShutdownGuard>) -> bool {
    guard.is_some_and(|guard| guard.cancelled().now_or_never().is_some())
}

pub(crate) struct Empty;

impl Inventory for Empty {
    fn snapshot(&self, _: &[String]) -> Snapshot {
        Snapshot {
            state: ServerState::Serving,
            models: vec![],
            host: Host::default(),
        }
    }
}

pub(crate) struct Native<B: Backend> {
    pub registry: Arc<Registry<B>>,
    pub shutdown: Option<WeakShutdownGuard>,
    pub power: Option<(watch::Receiver<PowerStatus>, DegradedLimits)>,
    pub host: Arc<Host>,
    /// The service's output cap, advertised as `max_output_tokens`.
    pub max_output_tokens: usize,
}

impl<B: Backend> Native<B> {
    /// The request caps in force now: `Some` only while the power watcher
    /// reports `Degraded`. Read once per request, as native admission does.
    pub(crate) fn degraded_limits(&self) -> Option<DegradedLimits> {
        self.power
            .as_ref()
            .filter(|(rx, _)| rx.borrow().state == crate::power::ServerState::Degraded)
            .map(|(_, limits)| *limits)
    }
}

impl<B> Inventory for Native<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    fn snapshot(&self, exports: &[String]) -> Snapshot {
        let power = self
            .power
            .as_ref()
            .map(|(rx, _)| (*rx.borrow(), rx.has_changed().is_err()));
        let mut state = if draining(self.shutdown.as_ref()) {
            ServerState::Draining
        } else if let Some((power, closed)) = power {
            // A dead watcher cannot promise readiness from its last observation.
            if closed && power.state != crate::power::ServerState::Draining {
                ServerState::Critical
            } else {
                match power.state {
                    crate::power::ServerState::Serving => ServerState::Serving,
                    crate::power::ServerState::Degraded => ServerState::Degraded,
                    crate::power::ServerState::Critical => ServerState::Critical,
                    crate::power::ServerState::Draining => ServerState::Draining,
                }
            }
        } else {
            ServerState::Serving
        };
        if state == ServerState::Serving
            && self
                .registry
                .entries()
                .any(|entry| entry.phase == Phase::Failed)
        {
            state = ServerState::Degraded;
        }
        // Policy order is stable and bounded (<=256); no default-name fallback,
        // filesystem discovery or export inference.
        let mut models = Vec::new();
        for name in exports {
            if super::config::validate_model(name).is_err() {
                continue;
            }
            let Some(entry) = self.registry.entries().find(|entry| entry.name == name) else {
                continue;
            };
            if entry.phase == Phase::Failed {
                continue;
            }
            let info = entry.info;
            let Some(artifact) = self.registry.artifact_id(name) else {
                continue;
            };
            let Some(profile) = self.registry.execution_profile(name) else {
                continue;
            };
            let backend = profile
                .manifest()
                .as_object()
                .and_then(|manifest| manifest.get("backend"))
                .and_then(|value| value.as_str());
            let Some(backend @ ("cpu" | "cuda" | "metal" | "rocm" | "vulkan")) = backend else {
                continue;
            };
            let Ok(trained_ctx) = u32::try_from(info.trained_ctx) else {
                continue;
            };
            let (kind, limits) = match info.kind {
                crate::registry::Kind::Gguf => {
                    // The configured maxima: power caps apply at admission, so
                    // a battery flip changes the state, never the selection.
                    let n_ctx = info.n_ctx;
                    let output = n_ctx.min(self.max_output_tokens);
                    let (Ok(n_ctx), Ok(max_output_tokens)) =
                        (u32::try_from(n_ctx), u32::try_from(output))
                    else {
                        continue;
                    };
                    (
                        Kind::Gguf,
                        Limits::Synthesize {
                            n_ctx,
                            max_output_tokens,
                        },
                    )
                }
                crate::registry::Kind::Laya => (
                    Kind::Laya,
                    Limits::Decide {
                        max_questions: catalog::MAX_DECIDE_QUESTIONS,
                        max_result_bytes: catalog::MAX_DECIDE_RESULT_BYTES,
                    },
                ),
            };
            let descriptor = ModelDescriptor {
                model: entry.name.to_owned(),
                kind,
                artifact_id: artifact.as_str().into(),
                profile_id: profile.as_str().into(),
                architecture: info.architecture.clone(),
                // Taken from a concrete loader-created, still-current profile.
                backend: backend.into(),
                ready: !state.forces_unready(),
                max_input_bytes: catalog::MAX_MAX_INPUT_BYTES,
                limits,
                class: info.class,
                capabilities: info.capabilities.clone(),
                resident: entry.phase == Phase::Resident,
                parameters: info.parameters,
                quantization: info.quantization.clone(),
                trained_ctx,
                chat_format: info.chat_format,
            };
            // Invalid/unsupported local names or limits are unexportable.
            if descriptor.validate().is_ok() {
                models.push(descriptor);
            }
        }
        Snapshot {
            state,
            models,
            host: self.registry.live_host(&self.host),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
