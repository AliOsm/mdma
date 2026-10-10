use mdma::backend::{Backend, cpu::CpuBackend};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(super) const NAMES: &[&str] = &["cpu", "vulkan", "rocm", "cuda", "metal", "level0"];

pub(super) trait Task {
    type Output;
    fn run<B: Backend>(self, backend: B) -> Result<Self::Output>;
}

pub(super) fn built(name: &str) -> bool {
    match name {
        "cpu" => true,
        "vulkan" => cfg!(feature = "vulkan"),
        "rocm" => cfg!(feature = "rocm"),
        "cuda" => cfg!(feature = "cuda"),
        "metal" => cfg!(all(feature = "metal", target_os = "macos")),
        "level0" => cfg!(feature = "level0"),
        _ => false,
    }
}

pub(super) fn dispatch<T: Task>(name: &str, task: T) -> Result<T::Output> {
    match name {
        "cpu" => task.run(CpuBackend),
        #[cfg(feature = "vulkan")]
        "vulkan" => task.run(mdma::backend::vulkan::VulkanBackend::new()?),
        #[cfg(feature = "rocm")]
        "rocm" => {
            let backend = if std::env::var("MDMA_HIP_F32").as_deref() == Ok("1") {
                mdma::backend::rocm::HipBackend::new_f32()?
            } else { mdma::backend::rocm::HipBackend::new()? };
            task.run(backend)
        }
        #[cfg(feature = "cuda")]
        "cuda" => task.run(mdma::backend::cuda::CudaBackend::new()?),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        "metal" => task.run(mdma::backend::metal::MetalBackend::new()?),
        #[cfg(feature = "level0")]
        "level0" => task.run(mdma::backend::level0::Level0Backend::new()?),
        name if NAMES.contains(&name) => Err(format!("{name} backend not built for this target; rebuild mdma with --features {name}").into()),
        _ => Err(format!("unknown backend {name:?}; expected {}", NAMES.join(", ")).into()),
    }
}

pub(super) fn mib(value: usize) -> Result<usize> {
    value.checked_mul(1024 * 1024).ok_or_else(|| "MiB byte count overflow".into())
}
