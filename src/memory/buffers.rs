use super::counter::{Charge, Counter};
use crate::backend::{Error, Result};
use std::sync::Arc;

/// Disjoint backing domains, not whether a buffer is CPU-mappable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryDomain {
    Device,
    Host,
}

impl std::fmt::Display for MemoryDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { Self::Device => "device", Self::Host => "host" })
    }
}

#[derive(Debug)]
struct Inner {
    device: Counter,
    host: Counter,
    limits: [Option<usize>; 2],
}

/// Explicit backend buffer allocations, including retained scratch and deferred
/// frees. Not a VRAM/process cap: driver internals and ordinary Rust allocations
/// are excluded. Independent of (and never summed with) NativeMemoryBudget.
#[derive(Clone, Debug)]
pub struct BufferBudget(Arc<Inner>);

impl Default for BufferBudget {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl BufferBudget {
    /// `None` tracks without a quota. Library callers may use zero to forbid a domain.
    pub fn new(device_bytes: Option<usize>, host_bytes: Option<usize>) -> Self {
        Self(Arc::new(Inner {
            device: Counter::new(device_bytes.unwrap_or(usize::MAX)),
            host: Counter::new(host_bytes.unwrap_or(usize::MAX)),
            limits: [device_bytes, host_bytes],
        }))
    }

    fn counter(&self, domain: MemoryDomain) -> &Counter {
        match domain { MemoryDomain::Device => &self.0.device, MemoryDomain::Host => &self.0.host }
    }

    pub fn limit(&self, domain: MemoryDomain) -> Option<usize> {
        self.0.limits[match domain { MemoryDomain::Device => 0, MemoryDomain::Host => 1 }]
    }

    /// Includes allocation calls in progress and buffers awaiting driver release.
    /// CUDA/HIP count requested bytes, Vulkan counts memory requirements.
    pub fn used(&self, domain: MemoryDomain) -> usize {
        self.counter(domain).used()
    }

    /// Only replace an unused, unshared ledger; never detach live allocations or twins.
    pub(crate) fn replace(&mut self, budget: Self) -> Result<()> {
        if Arc::strong_count(&self.0) != 1 || self.0.device.used() != 0 || self.0.host.used() != 0 {
            return Err(Error::Backend("buffer budget must be installed before allocations or sharing".into()));
        }
        *self = budget;
        Ok(())
    }

    /// The callback must only allocate: cleanup after successful allocation belongs
    /// to the returned owner. Failed allocations roll back; live charges fail closed.
    pub(crate) fn allocate<T>(&self, domain: MemoryDomain, bytes: usize, allocate: impl FnOnce() -> Result<T>) -> Result<(T, BufferCharge)> {
        let counter = self.counter(domain);
        let charge = counter.reserve(bytes)
            .map_err(|used| Error::BufferBudget { domain, requested: bytes, used, limit: counter.limit() })?;
        let raw = allocate()?;
        Ok((raw, BufferCharge(Some(charge))))
    }
}

/// Unlike a reservation, dropping a live allocation charge does NOT refund it.
/// The owner must confirm driver release, including after deferred destruction.
#[derive(Debug)]
pub(crate) struct BufferCharge(Option<Charge>);

impl BufferCharge {
    pub(crate) fn release(&mut self, free: impl FnOnce() -> bool) {
        if self.0.is_some() && free() {
            self.0.take();
        }
    }
}

impl Drop for BufferCharge {
    fn drop(&mut self) {
        if let Some(charge) = self.0.take() {
            charge.retain();
        }
    }
}

#[cfg(test)]
mod tests;
