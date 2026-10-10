//! Independent native tensor admission and backend buffer quotas.
use crate::backend::{Error, Result};
mod counter;
#[cfg_attr(not(any(test, feature = "rocm", feature = "cuda", feature = "vulkan")), allow(dead_code))]
mod buffers;
pub use buffers::{BufferBudget, MemoryDomain};
#[cfg(any(feature = "rocm", feature = "cuda", feature = "vulkan"))]
pub(crate) use buffers::BufferCharge;

/// Share one budget across native models and their sessions. Counts weight
/// payloads, KV and workspace reservations only. Backend-private allocations,
/// driver overhead, staging and tokenizers are outside this budget. The registry
/// decoder budget excludes Laya; federated Laya uses a separate bounded logical
/// request-workspace instance, not shared decoder/model-weight accounting.
#[derive(Clone, Debug)]
pub struct NativeMemoryBudget(counter::Counter);

impl NativeMemoryBudget {
    pub fn new(limit: usize) -> Self {
        Self(counter::Counter::new(limit))
    }

    pub fn limit(&self) -> usize {
        self.0.limit()
    }

    /// Live and reserved logical tensor bytes; not measured physical residency.
    pub fn used(&self) -> usize {
        self.0.used()
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Result<Reservation> {
        let charge = self.0.reserve(bytes)
            .map_err(|used| Error::MemoryBudget { requested: bytes, used, limit: self.limit() })?;
        Ok(Reservation { budget: self.clone(), charge })
    }
}

#[derive(Debug)]
pub(crate) struct Reservation {
    pub(crate) budget: NativeMemoryBudget,
    charge: counter::Charge,
}

impl Reservation {
    pub(crate) fn split(&mut self, bytes: usize) -> Self {
        Self { budget: self.budget.clone(), charge: self.charge.split(bytes) }
    }

    pub(crate) fn covers(&self, budget: &NativeMemoryBudget, bytes: usize) -> bool {
        self.charge.covers(&budget.0, bytes)
    }
}

pub(crate) fn product(values: &[usize]) -> Result<usize> {
    values.iter().try_fold(1usize, |n, &value| n.checked_mul(value)).ok_or(Error::SizeOverflow)
}

pub(crate) fn sum(values: impl IntoIterator<Item = usize>) -> Result<usize> {
    values.into_iter().try_fold(0usize, usize::checked_add).ok_or(Error::SizeOverflow)
}

#[cfg(test)]
mod tests;
