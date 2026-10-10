//! Prepare persistent ROCm weights without starting a server or generating tokens.
use std::path::PathBuf;
use usage_rs::Cli;

#[derive(Debug, Cli)]
#[usage(bin = "mdma-cache", version = env!("CARGO_PKG_VERSION"))]
struct Args {
    /// Source GGUF; the file is never modified.
    model: PathBuf,
    /// Rebuild even when the SHA-keyed cache already exists.
    #[usage(long)]
    force: bool,
    /// Override MDMA_CACHE_DIR / XDG_CACHE_HOME/mdma / HOME/.cache/mdma.
    #[usage(long, value_hint = usage_rs::ValueHint::DirPath)]
    cache_dir: Option<PathBuf>,
}

#[cfg(feature = "rocm")]
fn prepare(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    use mdma::{backend::rocm::HipBackend, cache::ModelCache, gguf::GgufModel, tokenizer::Tokenizer};
    let mut source = GgufModel::open(&args.model)?;
    Tokenizer::from_gguf(&source).ok_or("unsupported or invalid tokenizer")?;
    let backend = HipBackend::new()?;
    let cache = match args.cache_dir {
        Some(root) if !root.as_os_str().is_empty() => ModelCache::new(root),
        Some(_) => return Err("--cache-dir must not be empty".into()),
        None => ModelCache::from_env()?,
    };
    let result = cache.prepare(&backend, &mut source, args.force)?;
    eprintln!("{} ROCm cache: SHA-256={}, {} packed tensors, {} weight bytes",
        if result.reused { "Reused" } else { "Prepared" }, result.source_sha256, result.packed_tensors, result.weight_bytes);
    println!("{}", result.path.display());
    Ok(())
}

#[cfg(not(feature = "rocm"))]
fn prepare(_args: Args) -> Result<(), Box<dyn std::error::Error>> {
    Err("mdma-cache currently supports ROCm; build with --features rocm".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    prepare(Args::parse())
}

#[cfg(test)]
#[path = "mdma-cache/tests.rs"]
mod tests;
