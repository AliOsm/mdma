//! Shared metadata-only admission and tensor validation for native decoders.
use super::{
    ModelConfig,
    arch::{gemma4, glm4, llama, qwen, qwen35},
};
use crate::backend::{Backend, Error, Result};
use crate::gguf::{GgmlDType, GgufModel, TensorDesc};
use std::collections::HashSet;

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
        qwen35::ARCH => qwen35::Qwen35::validate(backend, gguf)?.0,
        _ => {
            return Err(unsupported(format!(
                "architecture {arch:?}; native text decoders support llama, {}, gemma4, glm4, qwen2, qwen2vl, qwen3 and qwen35",
                llama::ARCH
            )));
        }
    };
    gguf.validate_tensor_ranges()
        .map_err(|e| unsupported(e.to_string()))?;
    Ok(config)
}

/// `{arch}.{suffix}` as a positive integer.
pub(super) fn positive(g: &GgufModel, arch: &str, suffix: &str) -> Result<usize> {
    let key = format!("{arch}.{suffix}");
    g.metadata_u32(&key)
        .filter(|v| *v > 0)
        .map(|v| v as usize)
        .ok_or_else(|| unsupported(format!("{key} must be a positive integer")))
}

/// `{arch}.{suffix}` as a finite positive float.
pub(super) fn positive_f32(g: &GgufModel, arch: &str, suffix: &str) -> Result<f32> {
    let key = format!("{arch}.{suffix}");
    g.metadata_f32(&key)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| unsupported(format!("{key} must be a finite positive float")))
}

/// Tensor-by-tensor layout check: each expected tensor is validated and
/// struck off, and whatever the decoder would not run is rejected.
pub(super) struct Layout<'a, B> {
    backend: &'a B,
    gguf: &'a GgufModel,
    remaining: HashSet<&'a str>,
}

impl<'a, B: Backend> Layout<'a, B> {
    /// Checks the embedding, the final norm and the output projection, which
    /// may be tied to the embedding when `tied`. `{arch}.vocab_size`, when
    /// present, must match the embedding. Returns the vocabulary size too.
    pub fn new(
        backend: &'a B,
        gguf: &'a GgufModel,
        arch: &str,
        c: &ModelConfig,
        tied: bool,
    ) -> Result<(Self, usize)> {
        let remaining: HashSet<_> = gguf.tensor_names().collect();
        // Bound descriptor work even if a corrupt header claims billions of layers.
        if c.n_layers > remaining.len() {
            return Err(unsupported("block_count exceeds tensor count"));
        }
        let embd = gguf
            .tensor_desc("token_embd.weight")
            .ok_or_else(|| unsupported("missing tensor token_embd.weight"))?;
        let vocab = match embd.shape.as_slice() {
            &[vocab, width] if vocab > 0 && width == c.n_embd => vocab,
            shape => {
                return Err(unsupported(format!(
                    "token_embd.weight: expected [vocab > 0, {}], got {shape:?}",
                    c.n_embd
                )));
            }
        };
        let key = format!("{arch}.vocab_size");
        if gguf.has_metadata(&key) && gguf.metadata_u32(&key).map(|v| v as usize) != Some(vocab) {
            return Err(unsupported(format!("{key} must match token_embd.weight")));
        }
        let mut layout = Self {
            backend,
            gguf,
            remaining,
        };
        layout.tensor("token_embd.weight", &[vocab, c.n_embd], false)?;
        layout.tensor("output_norm.weight", &[c.n_embd], true)?;
        if !tied || gguf.tensor_desc("output.weight").is_some() {
            layout.tensor("output.weight", &[vocab, c.n_embd], false)?;
        }
        Ok((layout, vocab))
    }

    pub fn tensor(&mut self, name: &str, shape: &[usize], norm: bool) -> Result<()> {
        let desc = self
            .gguf
            .tensor_desc(name)
            .ok_or_else(|| unsupported(format!("missing tensor {name}")))?;
        validate_tensor(self.backend, name, desc, shape, norm)?;
        self.remaining.remove(name);
        Ok(())
    }

    /// Strikes off, unchecked, every remaining tensor `skipped` names.
    pub fn skip(&mut self, skipped: impl Fn(&str) -> bool) {
        self.remaining.retain(|name| !skipped(name));
    }

    /// Rejects any tensor not struck off; `reason` says what is not implemented.
    pub fn finish(self, reason: &str) -> Result<()> {
        match self.remaining.into_iter().min() {
            Some(name) => Err(unsupported(format!("unexpected tensor {name}; {reason}"))),
            None => Ok(()),
        }
    }
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
