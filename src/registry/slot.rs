//! One model's residency: on disk, loading, loaded or failed. Loads are single-flight.
use super::Info;
use super::native::Pinned;
use crate::artifact::ArtifactId;
use crate::backend::Backend;
use crate::backend::cpu::CpuBackend;
use crate::laya::Predictor;
use crate::model::Model;
use crate::tokenizer::Tokenizer;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

pub(super) enum Source {
    Gguf {
        pinned: Pinned,
        tokenizer: Arc<Tokenizer>,
    },
    Laya(PathBuf),
    /// Built by the caller; always loaded.
    Fixed,
}

pub struct Decoder<B: Backend> {
    pub model: Arc<Model<B>>,
    pub tokenizer: Arc<Tokenizer>,
}

impl<B: Backend> Clone for Decoder<B> {
    fn clone(&self) -> Self {
        Self {
            model: self.model.clone(),
            tokenizer: self.tokenizer.clone(),
        }
    }
}

pub(super) enum Resident<B: Backend> {
    Decoder(Decoder<B>),
    Laya(Arc<Predictor<CpuBackend>>),
}

impl<B: Backend> Clone for Resident<B> {
    fn clone(&self) -> Self {
        match self {
            Self::Decoder(decoder) => Self::Decoder(decoder.clone()),
            Self::Laya(predictor) => Self::Laya(predictor.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// On disk; the next use loads it.
    Available,
    Loading,
    Resident,
    /// Unusable until restart: corrupt, unsupported or changed on disk.
    Failed,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Loading => "loading",
            Self::Resident => "resident",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// Not enough memory now; a later use may succeed.
    #[error("{0}")]
    Memory(String),
    #[error("{0}")]
    Failed(String),
}

enum State<B: Backend> {
    Available,
    Loading,
    Resident(Resident<B>),
    Failed(String),
}

struct Inner<B: Backend> {
    state: State<B>,
    /// Counts finished loads, so waiters can tell their load's outcome from a new attempt.
    generation: u64,
    error: Option<String>,
    /// Callers holding this model loaded.
    leases: u32,
    used: Instant,
}

impl<B: Backend> Inner<B> {
    /// Loaded, unleased and not in use by any request.
    fn idle(&self) -> bool {
        self.leases == 0
            && match &self.state {
                State::Resident(Resident::Decoder(decoder)) => {
                    Arc::strong_count(&decoder.model) == 1
                }
                State::Resident(Resident::Laya(predictor)) => Arc::strong_count(predictor) == 1,
                _ => false,
            }
    }
}

pub(super) struct Slot<B: Backend> {
    pub(super) info: Info,
    pub(super) source: Source,
    inner: Mutex<Inner<B>>,
    changed: Condvar,
    artifact: Mutex<Option<ArtifactId>>,
}

/// A loaded model in use. Holding it keeps the weights loaded.
pub struct Use<B: Backend> {
    resident: Resident<B>,
}

impl<B: Backend> Use<B> {
    pub const fn decoder(&self) -> Option<&Decoder<B>> {
        match &self.resident {
            Resident::Decoder(decoder) => Some(decoder),
            Resident::Laya(_) => None,
        }
    }

    pub const fn laya(&self) -> Option<&Arc<Predictor<CpuBackend>>> {
        match &self.resident {
            Resident::Laya(predictor) => Some(predictor),
            Resident::Decoder(_) => None,
        }
    }
}

/// Puts an interrupted load back on disk.
struct Ticket<'a, B: Backend>(Option<&'a Slot<B>>);

impl<B: Backend> Ticket<'_, B> {
    fn finish(mut self, outcome: Result<Resident<B>, LoadError>) {
        self.0.take().expect("ticket finishes once").finish(outcome);
    }
}

impl<B: Backend> Drop for Ticket<'_, B> {
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            slot.finish(Err(LoadError::Memory("load interrupted".into())));
        }
    }
}

impl<B: Backend> Slot<B> {
    pub(super) fn new(info: Info, source: Source) -> Self {
        Self::with_state(info, source, State::Available)
    }

    pub(super) fn fixed(info: Info, decoder: Decoder<B>) -> Self {
        Self::with_state(
            info,
            Source::Fixed,
            State::Resident(Resident::Decoder(decoder)),
        )
    }

    fn with_state(info: Info, source: Source, state: State<B>) -> Self {
        Self {
            info,
            source,
            inner: Mutex::new(Inner {
                state,
                generation: 0,
                error: None,
                leases: 0,
                used: Instant::now(),
            }),
            changed: Condvar::new(),
            artifact: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<B>> {
        self.inner.lock().expect("model slot lock poisoned")
    }

    pub(super) fn phase(&self) -> (Phase, Option<String>) {
        let inner = self.lock();
        match &inner.state {
            State::Available => (Phase::Available, inner.error.clone()),
            State::Loading => (Phase::Loading, None),
            State::Resident(_) => (Phase::Resident, None),
            State::Failed(error) => (Phase::Failed, Some(error.clone())),
        }
    }

    pub(super) fn resident(&self) -> Option<Use<B>> {
        match &self.lock().state {
            State::Resident(resident) => Some(Use {
                resident: resident.clone(),
            }),
            _ => None,
        }
    }

    pub(super) fn artifact(&self) -> Option<ArtifactId> {
        self.artifact
            .lock()
            .expect("artifact lock poisoned")
            .clone()
    }

    pub(super) fn set_artifact(&self, id: ArtifactId) {
        *self.artifact.lock().expect("artifact lock poisoned") = Some(id);
    }

    /// The loaded model, loading it first if needed. Concurrent callers share one load;
    /// a cancelled caller does not cancel it.
    pub(super) fn acquire(
        &self,
        load: impl FnOnce() -> Result<Resident<B>, LoadError>,
    ) -> Result<Use<B>, LoadError> {
        let mut inner = self.lock();
        let start = inner.generation;
        let mut load = Some(load);
        loop {
            match &inner.state {
                State::Resident(resident) => {
                    let resident = resident.clone();
                    inner.used = Instant::now();
                    return Ok(Use { resident });
                }
                State::Failed(error) => return Err(LoadError::Failed(error.clone())),
                State::Loading => {
                    inner = self.changed.wait(inner).expect("model slot lock poisoned");
                }
                State::Available if inner.generation != start => {
                    return Err(LoadError::Memory(inner.error.clone().unwrap_or_default()));
                }
                State::Available => {
                    inner.state = State::Loading;
                    drop(inner);
                    let ticket = Ticket(Some(self));
                    ticket.finish(load.take().expect("one load per acquire")());
                    inner = self.lock();
                }
            }
        }
    }

    /// The loaded model for a request, without loading it; marks it used.
    pub(super) fn try_use(&self) -> Option<Use<B>> {
        let mut inner = self.lock();
        let State::Resident(resident) = &inner.state else {
            return None;
        };
        let resident = resident.clone();
        inner.used = Instant::now();
        Some(Use { resident })
    }

    /// When it was last used, if it could be unloaded now.
    pub(super) fn idle_since(&self) -> Option<Instant> {
        let inner = self.lock();
        inner.idle().then_some(inner.used)
    }

    /// Unloads the model if it is still idle; the weights are freed as the
    /// last reference drops.
    pub(super) fn unload_if_idle(&self) -> bool {
        let resident = {
            let mut inner = self.lock();
            if !inner.idle() {
                return false;
            }
            inner.generation += 1;
            std::mem::replace(&mut inner.state, State::Available)
        };
        drop(resident);
        true
    }

    pub(super) fn leases(&self) -> u32 {
        self.lock().leases
    }

    pub(super) fn add_lease(&self) {
        self.lock().leases += 1;
    }

    pub(super) fn drop_lease(&self) {
        let mut inner = self.lock();
        inner.leases = inner.leases.saturating_sub(1);
    }

    fn finish(&self, outcome: Result<Resident<B>, LoadError>) {
        let mut inner = self.lock();
        inner.generation += 1;
        (inner.state, inner.error) = match outcome {
            Ok(resident) => (State::Resident(resident), None),
            Err(LoadError::Memory(error)) => (State::Available, Some(error)),
            Err(LoadError::Failed(error)) => (State::Failed(error), None),
        };
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests;
