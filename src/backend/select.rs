//! Backends by name, for the command-line tools.
use super::{Backend, cpu::CpuBackend};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Backends a target can host: Metal is macOS only; ROCm, CUDA and Level Zero have no
// macOS driver; the Coral Edge TPU has PCIe drivers for FreeBSD and Linux.
#[cfg(target_os = "macos")]
pub const NAMES: &[&str] = &["cpu", "metal"];
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
pub const NAMES: &[&str] = &["cpu", "vulkan", "rocm", "cuda", "level0", "coral"];
#[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "linux")))]
pub const NAMES: &[&str] = &["cpu", "vulkan", "rocm", "cuda", "level0"];

/// Work generic over the backend `dispatch` builds.
pub trait Task {
    type Output;
    fn run<B: Backend>(self, backend: B) -> Result<Self::Output>;
}

pub fn built(name: &str) -> bool {
    match name {
        "cpu" => true,
        #[cfg(not(target_os = "macos"))]
        "vulkan" => cfg!(feature = "vulkan"),
        #[cfg(not(target_os = "macos"))]
        "rocm" => cfg!(feature = "rocm"),
        #[cfg(not(target_os = "macos"))]
        "cuda" => cfg!(feature = "cuda"),
        #[cfg(target_os = "macos")]
        "metal" => cfg!(feature = "metal"),
        #[cfg(not(target_os = "macos"))]
        "level0" => cfg!(feature = "level0"),
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        "coral" => cfg!(feature = "coral"),
        _ => false,
    }
}

/// Builds backend `name` and runs `task` on it. `MDMA_HIP_F32=1` selects
/// ROCm's f32 compute and KV.
pub fn dispatch<T: Task>(name: &str, task: T) -> Result<T::Output> {
    match name {
        "cpu" => task.run(CpuBackend),
        #[cfg(all(feature = "vulkan", not(target_os = "macos")))]
        "vulkan" => task.run(super::vulkan::VulkanBackend::new()?),
        #[cfg(all(feature = "rocm", not(target_os = "macos")))]
        "rocm" => {
            let backend = if std::env::var("MDMA_HIP_F32").as_deref() == Ok("1") {
                super::rocm::HipBackend::new_f32()?
            } else {
                super::rocm::HipBackend::new()?
            };
            task.run(backend)
        }
        #[cfg(all(feature = "cuda", not(target_os = "macos")))]
        "cuda" => task.run(super::cuda::CudaBackend::new()?),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        "metal" => task.run(super::metal::MetalBackend::new()?),
        #[cfg(all(feature = "level0", not(target_os = "macos")))]
        "level0" => task.run(super::level0::Level0Backend::new()?),
        #[cfg(all(feature = "coral", any(target_os = "freebsd", target_os = "linux")))]
        "coral" => task.run(super::coral::CoralBackend::new()?),
        name if NAMES.contains(&name) => Err(format!(
            "{name} backend not built for this target; rebuild mdma with --features {name}"
        )
        .into()),
        _ => Err(format!("unknown backend {name:?}; expected {}", NAMES.join(", ")).into()),
    }
}
