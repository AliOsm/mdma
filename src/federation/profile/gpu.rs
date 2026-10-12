//! Concrete backend snapshots, never a capability asserted by `Backend` labels.
use super::super::canonical_json::Value;
use std::{
    any::Any,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Default)]
pub(crate) struct Health(Arc<AtomicBool>);

impl Health {
    #[cfg_attr(not(any(all(feature = "metal", target_os = "macos"), feature = "cuda", feature = "rocm", feature = "vulkan", test)), expect(dead_code, reason = "used by the GPU backends"))]
    pub(crate) fn withdraw(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[cfg_attr(not(any(all(feature = "metal", target_os = "macos"), feature = "cuda", feature = "rocm", feature = "vulkan", test)), expect(dead_code, reason = "used by the GPU backends"))]
    pub(crate) fn check<T>(&self, result: crate::backend::Result<T>) -> crate::backend::Result<T> {
        if result.is_err() {
            self.withdraw();
        }
        result
    }
}

#[derive(Clone)]
pub(crate) struct Gpu {
    pub backend: &'static str,
    pub activation: &'static str,
    pub kv: &'static str,
    pub modes: Value,
    revision: Option<(Arc<Mutex<u64>>, u64)>,
    health: Health,
}

impl Gpu {
    #[cfg_attr(not(any(all(feature = "metal", target_os = "macos"), feature = "cuda", feature = "rocm", feature = "vulkan", test)), expect(dead_code, reason = "used by the GPU backends"))]
    pub(crate) fn immutable(
        backend: &'static str,
        activation: &'static str,
        kv: &'static str,
        modes: Value,
        health: Health,
    ) -> Self {
        Self {
            backend,
            activation,
            kv,
            modes,
            revision: None,
            health,
        }
    }

    pub(super) fn current(&self) -> bool {
        !self.health.0.load(Ordering::Acquire)
            && self
                .revision
                .as_ref()
                .is_none_or(|(state, captured)| state.lock().is_ok_and(|now| *now == *captured))
    }
}

/// Mutable calibration controls withdraw every previously captured profile,
/// even if the old mode is restored. Snapshot and mutation share one lock.
#[cfg(any(feature = "vulkan", test))]
#[derive(Default)]
pub(crate) struct Modes {
    revision: Arc<Mutex<u64>>,
}

#[cfg(any(feature = "vulkan", test))]
impl Modes {
    pub(crate) fn change(&self, change: impl FnOnce()) {
        let mut revision = self.revision.lock().expect("GPU mode lock poisoned");
        *revision = revision
            .checked_add(1)
            .expect("GPU mode revision exhausted");
        change();
    }

    pub(crate) fn capture(&self, make: impl FnOnce() -> Gpu) -> io::Result<Gpu> {
        let revision = self
            .revision
            .lock()
            .map_err(|_| io::Error::other("GPU mode lock poisoned"))?;
        let mut gpu = make();
        gpu.revision = Some((self.revision.clone(), *revision));
        Ok(gpu)
    }
}

#[cfg_attr(not(any(all(feature = "metal", target_os = "macos"), feature = "cuda", feature = "rocm", feature = "vulkan", test)), expect(dead_code, reason = "used by the GPU backends"))]
pub(crate) fn fields<const N: usize>(values: [(&str, String); N]) -> Value {
    Value::object(
        values
            .into_iter()
            .map(|(key, value)| (key.into(), Value::String(value))),
    )
    .expect("fixed distinct GPU mode fields")
}

#[cfg_attr(not(any(all(feature = "metal", target_os = "macos"), feature = "cuda", feature = "rocm", feature = "vulkan")), expect(clippy::unnecessary_wraps, reason = "GPU builds return captured profiles"))]
pub(crate) fn capture(backend: &dyn Any) -> io::Result<Option<Gpu>> {
    #[cfg(all(feature = "metal", target_os = "macos"))]
    if let Some(backend) = backend.downcast_ref::<crate::backend::metal::MetalBackend>() {
        return backend.federation_profile().map(Some);
    }
    #[cfg(feature = "cuda")]
    if let Some(backend) = backend.downcast_ref::<crate::backend::cuda::CudaBackend>() {
        return backend.federation_profile().map(Some);
    }
    #[cfg(feature = "rocm")]
    if let Some(backend) = backend.downcast_ref::<crate::backend::rocm::HipBackend>() {
        return backend.federation_profile().map(Some);
    }
    #[cfg(feature = "vulkan")]
    if let Some(backend) = backend.downcast_ref::<crate::backend::vulkan::VulkanBackend>() {
        return backend.federation_profile().map(Some);
    }
    let _ = backend;
    Ok(None)
}
