use mdma::backend::{Backend, cpu::CpuBackend};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
// Backends a target can host: Metal is macOS only; ROCm, CUDA and Level Zero have no
// macOS driver; the Coral Edge TPU has PCIe drivers for FreeBSD and Linux.
#[cfg(target_os = "macos")]
pub(super) const NAMES: &[&str] = &["cpu", "metal"];
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
pub(super) const NAMES: &[&str] = &["cpu", "vulkan", "rocm", "cuda", "level0", "coral"];
#[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "linux")))]
pub(super) const NAMES: &[&str] = &["cpu", "vulkan", "rocm", "cuda", "level0"];

pub(super) trait Task {
    type Output;
    fn run<B: Backend>(self, backend: B) -> Result<Self::Output>;
}

pub(super) fn built(name: &str) -> bool {
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

pub(super) fn dispatch<T: Task>(name: &str, task: T) -> Result<T::Output> {
    match name {
        "cpu" => task.run(CpuBackend),
        #[cfg(all(feature = "vulkan", not(target_os = "macos")))]
        "vulkan" => task.run(mdma::backend::vulkan::VulkanBackend::new()?),
        #[cfg(all(feature = "rocm", not(target_os = "macos")))]
        "rocm" => {
            let backend = if std::env::var("MDMA_HIP_F32").as_deref() == Ok("1") {
                mdma::backend::rocm::HipBackend::new_f32()?
            } else {
                mdma::backend::rocm::HipBackend::new()?
            };
            task.run(backend)
        }
        #[cfg(all(feature = "cuda", not(target_os = "macos")))]
        "cuda" => task.run(mdma::backend::cuda::CudaBackend::new()?),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        "metal" => task.run(mdma::backend::metal::MetalBackend::new()?),
        #[cfg(all(feature = "level0", not(target_os = "macos")))]
        "level0" => task.run(mdma::backend::level0::Level0Backend::new()?),
        #[cfg(all(feature = "coral", any(target_os = "freebsd", target_os = "linux")))]
        "coral" => task.run(mdma::backend::coral::CoralBackend::new()?),
        name if NAMES.contains(&name) => Err(format!(
            "{name} backend not built for this target; rebuild mdma with --features {name}"
        )
        .into()),
        _ => Err(format!("unknown backend {name:?}; expected {}", NAMES.join(", ")).into()),
    }
}

pub(super) fn mib(value: usize) -> Result<usize> {
    value
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "MiB byte count overflow".into())
}
