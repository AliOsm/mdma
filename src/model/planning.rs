//! Metadata-only native tensor estimates and contiguous GPU-suffix placement.
use super::{
    Model, ModelConfig,
    arch::{gemma4, glm4, qwen},
    memory,
};
use crate::backend::{Backend, Error, Result, cpu::CpuBackend};
use crate::gguf::GgufModel;
use crate::memory::{product, sum};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Estimate {
    pub architecture: String,
    pub layers: usize,
    pub context: usize,
    pub prompt_tokens: usize,
    pub kv_element_bytes: usize,
    pub weights_bytes: usize,
    pub kv_bytes: usize,
    pub prefill_workspace_bytes: usize,
    pub decode_workspace_bytes: usize,
    pub load_staging_bytes: usize,
    pub peak_bytes: usize,
}

fn request(c: &ModelConfig, context: usize, prompt: usize) -> Result<()> {
    if context == 0 || context > c.n_ctx || prompt == 0 || prompt > context {
        return Err(Error::ShapeMismatch(
            "require 1 <= prompt tokens <= context <= model context".into(),
        ));
    }
    Ok(())
}

fn vocab(g: &GgufModel) -> usize {
    g.tensor_desc("output.weight")
        .or_else(|| g.tensor_desc("token_embd.weight"))
        .expect("validated decoders have an output projection")
        .shape[0]
}

fn staging(g: &GgufModel) -> usize {
    g.tensor_names()
        .map(|name| {
            g.tensor_desc(name)
                .expect("name comes from tensor_names")
                .n_bytes
        })
        .max()
        .unwrap_or(0)
}

pub fn estimate<B: Backend>(
    b: &B,
    g: &GgufModel,
    context: usize,
    prompt: usize,
) -> Result<Estimate> {
    let c = Model::validate(b, g)?;
    request(&c, context, prompt)?;
    let arch = g
        .metadata_str("general.architecture")
        .expect("validated architecture");
    let weights_bytes = memory::weight_bytes(g)?;
    let mut widths = vec![product(&[c.n_kv_heads, c.head_dim])?; c.n_layers];
    let mut extra_workspace = 0;
    let mut gemma = false;
    if arch == "gemma4" {
        let (_, config) = gemma4::Config::read(g)?;
        widths = config.kv_widths(&c)?;
        extra_workspace = config.workspace_bytes(&c)?;
        gemma = true;
    } else if qwen::is_arch(arch) {
        extra_workspace = qwen::Qwen::<B>::workspace_bytes(&c)?;
    } else if arch == "glm4" {
        extra_workspace = glm4::Glm4::<B>::workspace_bytes(&c)?;
    }
    let workspace = |tokens| {
        let mut sizes = memory::workspace_sizes(&c, vocab(g), tokens)?;
        if gemma {
            sizes[3] = sizes[0];
            sizes[6] = sizes[0];
        }
        sum([sum(sizes)?, extra_workspace])
    };
    let batch = if matches!(arch, "llama" | "internlm2") {
        prompt
    } else {
        1
    };
    let prefill_workspace_bytes = workspace(batch)?;
    let decode_workspace_bytes = workspace(1)?;
    let kv_bytes = product(&[sum(widths)?, b.kv_elem_bytes(), context, 2])?;
    let load_staging_bytes = staging(g);
    let peak_bytes = sum([
        weights_bytes,
        kv_bytes,
        prefill_workspace_bytes.max(decode_workspace_bytes),
    ])?
    .max(sum([weights_bytes, load_staging_bytes])?);
    Ok(Estimate {
        architecture: arch.into(),
        layers: c.n_layers,
        context,
        prompt_tokens: prompt,
        kv_element_bytes: b.kv_elem_bytes(),
        weights_bytes,
        kv_bytes,
        prefill_workspace_bytes,
        decode_workspace_bytes,
        load_staging_bytes,
        peak_bytes,
    })
}

#[derive(Debug, Serialize)]
pub struct DomainMemory {
    pub weights_bytes: usize,
    pub kv_bytes: usize,
    pub workspace_bytes: usize,
    pub peak_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct OffloadPlan {
    pub architecture: String,
    pub layers: usize,
    pub cpu_layers: usize,
    pub gpu_layers: usize,
    pub context: usize,
    pub prompt_tokens: usize,
    pub gpu_kv_element_bytes: usize,
    pub unified_memory: bool,
    pub host: DomainMemory,
    pub device: DomainMemory,
    pub bridge_bytes: usize,
    pub load_staging_bytes: usize,
}

impl OffloadPlan {
    pub fn fits(&self, host_budget: usize, device_budget: usize, headroom: usize) -> Result<bool> {
        let headroom = if self.gpu_layers > 0 { headroom } else { 0 };
        let host_need = if self.unified_memory {
            sum([self.host.peak_bytes, self.device.peak_bytes, headroom])?
        } else {
            self.host.peak_bytes
        };
        Ok(host_need <= host_budget && sum([self.device.peak_bytes, headroom])? <= device_budget)
    }
}

pub fn offload<B: Backend>(
    gpu: &B,
    g: &GgufModel,
    context: usize,
    prompt: usize,
    gpu_layers: usize,
) -> Result<OffloadPlan> {
    let c = super::hybrid::validate(gpu, g, gpu_layers)?;
    request(&c, context, prompt)?;
    let cpu_layers = c.n_layers - gpu_layers;
    let mut device_weights = 0;
    for name in g.tensor_names() {
        if let Some(layer) = layer_index(name)
            && layer >= cpu_layers
        {
            device_weights = sum([device_weights, g.weight_bytes(name)])?;
        }
    }
    let weights = memory::weight_bytes(g)?;
    let workspace = sum(memory::workspace_sizes(&c, vocab(g), prompt)?)?;
    let kv_width = product(&[c.n_kv_heads, c.head_dim, context, 2])?;
    let bridge_bytes = if gpu_layers > 0 {
        product(&[c.n_embd, prompt, 4])?
    } else {
        0
    };
    let load_staging_bytes = staging(g);
    let host_weights = weights - device_weights;
    let host_kv = product(&[cpu_layers, kv_width, 4])?;
    let host_peak = sum([host_weights, host_kv, workspace, bridge_bytes])?
        .max(sum([host_weights, load_staging_bytes])?);
    let device_kv = product(&[gpu_layers, kv_width, gpu.kv_elem_bytes()])?;
    let device_workspace = if gpu_layers > 0 { workspace } else { 0 };
    Ok(OffloadPlan {
        architecture: g
            .metadata_str("general.architecture")
            .expect("validated architecture")
            .into(),
        layers: c.n_layers,
        cpu_layers,
        gpu_layers,
        context,
        prompt_tokens: prompt,
        gpu_kv_element_bytes: gpu.kv_elem_bytes(),
        unified_memory: gpu.unified_memory(),
        host: DomainMemory {
            weights_bytes: host_weights,
            kv_bytes: host_kv,
            workspace_bytes: workspace,
            peak_bytes: host_peak,
        },
        device: DomainMemory {
            weights_bytes: device_weights,
            kv_bytes: device_kv,
            workspace_bytes: device_workspace,
            peak_bytes: sum([device_weights, device_kv, device_workspace])?,
        },
        bridge_bytes,
        load_staging_bytes,
    })
}

pub(super) fn layer_index(name: &str) -> Option<usize> {
    name.strip_prefix("blk.")?.split_once('.')?.0.parse().ok()
}

/// Largest executable suffix fitting both budgets. Headroom is not tensor memory.
pub fn auto_offload<B: Backend>(
    gpu: &B,
    g: &GgufModel,
    context: usize,
    prompt: usize,
    host_budget: usize,
    device_budget: usize,
    headroom: usize,
) -> Result<OffloadPlan> {
    let layers = Model::validate(&CpuBackend, g)?.n_layers;
    let mut unsupported = None;
    for count in (0..=layers).rev() {
        let plan = match offload(gpu, g, context, prompt, count) {
            Ok(plan) => plan,
            Err(Error::UnsupportedModel(message)) => {
                unsupported = Some(message);
                continue;
            }
            Err(error) => return Err(error),
        };
        if plan.fits(host_budget, device_budget, headroom)? {
            return Ok(plan);
        }
    }
    if let Some(message) = unsupported {
        return Err(Error::UnsupportedModel(message));
    }
    Err(Error::Backend(format!(
        "no placement fits RAM budget {host_budget} and GPU budget {device_budget} bytes with {headroom} bytes headroom"
    )))
}

#[cfg(test)]
mod tests;
