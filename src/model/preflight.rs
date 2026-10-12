//! Shared metadata-only admission and tensor validation for native decoders.
use super::{
    ModelConfig,
    arch::{gemma4, glm4, llama, qwen},
};
use crate::backend::{Backend, Error, Result};
use crate::gguf::{GgmlDType, GgufModel, TensorDesc};

pub(super) fn unsupported(message: impl Into<String>) -> Error {
    Error::UnsupportedModel(message.into())
}

pub(super) fn validate<B: Backend>(backend: &B, gguf: &GgufModel) -> Result<ModelConfig> {
    if let Some(profile) = gguf.cache_profile()
        && backend.weight_cache_profile() != Some(profile)
    {
        return Err(unsupported(
            "prepared cache belongs to a different backend/target/kernel build",
        ));
    }
    gguf.check_cache_source()?;
    let arch = gguf
        .metadata_str("general.architecture")
        .ok_or_else(|| unsupported("general.architecture must be a string"))?;
    if gguf.has_metadata("general.type") && gguf.metadata_str("general.type") != Some("model") {
        return Err(unsupported(
            "general.type must be \"model\"; adapters and projectors are not native decoders",
        ));
    }
    let config = match arch {
        "gemma4" => gemma4::Gemma::validate(backend, gguf)?.0,
        "glm4" => glm4::Glm4::validate(backend, gguf)?,
        "llama" | llama::ARCH => llama::Llama::validate(backend, gguf, arch)?,
        arch if qwen::is_arch(arch) => qwen::Qwen::validate(backend, gguf, arch)?,
        _ => {
            return Err(unsupported(format!(
                "architecture {arch:?}; native text decoders support llama, {}, gemma4, glm4, qwen2, qwen2vl and qwen3",
                llama::ARCH
            )));
        }
    };
    gguf.validate_tensor_ranges()
        .map_err(|e| unsupported(e.to_string()))?;
    Ok(config)
}

pub(super) fn validate_tensor<B: Backend>(
    backend: &B,
    name: &str,
    desc: &TensorDesc,
    shape: &[usize],
    norm: bool,
) -> Result<()> {
    if desc.shape != shape {
        return Err(unsupported(format!(
            "{name}: expected shape {shape:?}, got {:?}",
            desc.shape
        )));
    }
    if norm && desc.dtype != GgmlDType::F32 {
        return Err(unsupported(format!(
            "{name}: norm weight must be F32, got {:?}",
            desc.dtype
        )));
    }
    if !norm && !backend.supports_weight_dtype(desc.dtype) {
        return Err(unsupported(format!(
            "{name}: backend does not support {:?} weights",
            desc.dtype
        )));
    }
    if !shape[shape.len() - 1].is_multiple_of(desc.dtype.block_size()) {
        return Err(unsupported(format!(
            "{name}: row width is not aligned to {:?} blocks",
            desc.dtype
        )));
    }
    Ok(())
}
