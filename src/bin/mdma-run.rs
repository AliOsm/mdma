//! Local, deterministic model smoke runner. Serving still uses mTLS.
use mdma::backend::{Backend, cpu::CpuBackend};
use mdma::{gguf::GgufModel, inference};

fn run<B: Backend>(backend: &B, path: &str, prompt: &str, max: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut gguf = GgufModel::open(path)?;
    // Compatibility runner accepts an explicit suffix. Auto placement and
    // budget preflight live in `mdma run --gpu-layers auto`.
    let count = std::env::var("MDMA_GPU_LAYERS").ok().map(|s| s.parse::<usize>()).transpose()?;
    let generated = inference::generate(backend, &mut gguf, prompt, max, count)?;
    if std::env::var_os("MDMA_JSON").is_some() {
        println!("{}", serde_json::to_string(&generated)?);
    } else {
        println!("{}", generated.text);
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=3).contains(&args.len()) { return Err("usage: mdma-run MODEL.gguf PROMPT [MAX_TOKENS]".into()); }
    let max = args.get(2).map(|n| n.parse()).transpose()?.unwrap_or(32);
    match std::env::var("MDMA_BACKEND").as_deref().unwrap_or("cpu") {
        "cpu" => run(&CpuBackend, &args[0], &args[1], max),
        #[cfg(feature = "rocm")]
        "rocm" => {
            let backend = if std::env::var("MDMA_HIP_F32").as_deref() == Ok("1") {
                mdma::backend::rocm::HipBackend::new_f32()?
            } else {
                mdma::backend::rocm::HipBackend::new()?
            };
            run(&backend, &args[0], &args[1], max)
        },
        #[cfg(feature = "vulkan")]
        "vulkan" => run(&mdma::backend::vulkan::VulkanBackend::new()?, &args[0], &args[1], max),
        #[cfg(feature = "cuda")]
        "cuda" => run(&mdma::backend::cuda::CudaBackend::new()?, &args[0], &args[1], max),
        #[cfg(all(feature = "metal", target_os = "macos"))]
        "metal" => run(&mdma::backend::metal::MetalBackend::new()?, &args[0], &args[1], max),
        _ => Err("mdma-run supports cpu, rocm, vulkan, cuda, or metal (macOS); GPU backends require the matching build feature".into()),
    }
}
