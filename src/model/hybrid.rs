//! CPU prefix / GPU suffix execution. Weights and KV stay with their layers.
use super::{Model, ModelConfig, Workspace, arch::{Decoder, llama::Llama}, load_norm, load_weight, memory};
use crate::backend::{Backend, Error, Result, cpu::CpuBackend};
use crate::gguf::GgufModel;
use crate::kv_cache::KvCache;
use crate::memory::product;

pub struct HybridModel<G: Backend> {
    host: Model<CpuBackend>,
    device: Llama<G>,
}

pub struct HybridKv<G: Backend> {
    host: Option<KvCache<CpuBackend>>,
    device: Option<KvCache<G>>,
    capacity: usize,
    len: usize,
}

impl<G: Backend> HybridKv<G> {
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn capacity(&self) -> usize { self.capacity }
}

pub struct HybridWorkspace<G: Backend> {
    host: Workspace<CpuBackend>,
    device: Option<Workspace<G>>,
    bridge: Vec<u8>,
}

pub(crate) fn validate<G: Backend>(gpu: &G, g: &GgufModel, gpu_layers: usize) -> Result<ModelConfig> {
    if !matches!(g.metadata_str("general.architecture"), Some("llama" | "internlm2")) {
        return Err(Error::UnsupportedModel("layer offloading currently supports llama and internlm2; use ordinary CPU/GPU execution for other architectures".into()));
    }
    let c = Model::validate(&CpuBackend, g)?;
    if gpu_layers > c.n_layers {
        return Err(Error::ShapeMismatch("GPU layer count exceeds model layers".into()));
    }
    if gpu_layers > 0 {
        if gpu.max_head_dim().is_some_and(|max| c.head_dim > max) {
            return Err(Error::UnsupportedModel("GPU attention head dimension is unsupported".into()));
        }
        for name in g.tensor_names() {
            if super::planning::layer_index(name).is_some_and(|i| i >= c.n_layers - gpu_layers)
                && let dtype = g.tensor_desc(name).expect("name comes from tensor_names").dtype
                && !gpu.supports_weight_dtype(dtype)
            {
                return Err(Error::UnsupportedModel(format!("{name}: GPU does not support {dtype:?}")));
            }
        }
    }
    Ok(c)
}

impl<G: Backend> HybridModel<G> {
    pub fn load(gpu: &G, g: &mut GgufModel, gpu_layers: usize) -> Result<Self> {
        let config = validate(gpu, g, gpu_layers)?;
        let split = config.n_layers - gpu_layers;
        let cpu = CpuBackend;
        let host = Model {
            token_embd: load_weight(&cpu, g, "token_embd.weight")?,
            output_norm: load_norm(&cpu, g, "output_norm.weight")?,
            output: Some(load_weight(&cpu, g, "output.weight")?),
            architecture: Decoder::Llama(Llama { layers: Llama::load_layers(&cpu, g, 0..split)? }),
            config, memory: None,
        };
        let device = Llama { layers: Llama::load_layers(gpu, g, split..host.config.n_layers)? };
        Ok(Self { host, device })
    }

    pub fn config(&self) -> &ModelConfig { self.host.config() }
    pub fn gpu_layers(&self) -> usize { self.device.layers.len() }

    pub fn new_kv_cache(&self, gpu: &G, context: usize) -> Result<HybridKv<G>> {
        if context == 0 || context > self.config().n_ctx {
            return Err(Error::ShapeMismatch("hybrid context outside model capacity".into()));
        }
        let c = self.config();
        let gpu_layers = self.gpu_layers();
        let cpu_layers = c.n_layers - gpu_layers;
        Ok(HybridKv {
            host: if cpu_layers > 0 { Some(KvCache::new(&CpuBackend, cpu_layers, c.n_kv_heads, context, c.head_dim)?) } else { None },
            device: if gpu_layers > 0 { Some(KvCache::new(gpu, gpu_layers, c.n_kv_heads, context, c.head_dim)?) } else { None },
            capacity: context, len: 0,
        })
    }

    pub fn alloc_workspace(&self, gpu: &G, tokens: usize) -> Result<HybridWorkspace<G>> {
        if tokens == 0 || tokens > self.config().n_ctx {
            return Err(Error::ShapeMismatch("hybrid workspace outside model context".into()));
        }
        let sizes = memory::workspace_sizes(self.config(), self.host.output_weight().shape[0], tokens)?;
        Ok(HybridWorkspace {
            host: Workspace::basic(&CpuBackend, tokens, sizes)?,
            device: if self.gpu_layers() > 0 { Some(Workspace::basic(gpu, tokens, sizes)?) } else { None },
            bridge: if self.gpu_layers() > 0 { vec![0; product(&[tokens, self.config().n_embd, 4])?] } else { Vec::new() },
        })
    }

    pub fn forward(&self, gpu: &G, kv: &mut HybridKv<G>, ws: &mut HybridWorkspace<G>, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.prefill(gpu, kv, ws, &[token], pos)
    }

    pub fn prefill(&self, gpu: &G, kv: &mut HybridKv<G>, ws: &mut HybridWorkspace<G>, tokens: &[u32], pos: u32) -> Result<Vec<f32>> {
        if tokens.is_empty() || tokens.len() != ws.host.n_tokens || pos as usize > kv.len
            || tokens.len() > kv.capacity.saturating_sub(pos as usize)
            || tokens.iter().any(|&t| t as usize >= self.host.token_embd.shape[0])
        {
            return Err(Error::ShapeMismatch("hybrid requires matching workspace, valid tokens and initialized KV prefix within capacity".into()));
        }
        let cpu = CpuBackend;
        cpu.embed(&self.host.token_embd, tokens, &mut ws.host.x)?;
        let Decoder::Llama(host) = &self.host.architecture else { unreachable!() };
        if let Some(host_kv) = &mut kv.host {
            let norm = if self.gpu_layers() == 0 { Some(&self.host.output_norm) } else { None };
            host.run_layers(&cpu, self.config(), host_kv, &mut ws.host, pos, tokens.len(), norm)?;
        }
        if let (Some(device_kv), Some(device_ws)) = (&mut kv.device, &mut ws.device) {
            cpu.download(&ws.host.x, 0, &mut ws.bridge)?;
            gpu.upload(&mut device_ws.x, 0, &ws.bridge)?;
            self.device.run_layers(gpu, self.config(), device_kv, device_ws, pos, tokens.len(), None)?;
            gpu.download(&device_ws.x, 0, &mut ws.bridge)?;
            cpu.upload(&mut ws.host.x, 0, &ws.bridge)?;
            cpu.rmsnorm(&self.host.output_norm, &ws.host.x, &mut ws.host.normed, self.config().rms_eps)?;
        }
        let row_bytes = product(&[self.config().n_embd, 4])?;
        cpu.copy(&ws.host.normed, (tokens.len() - 1) * row_bytes, &mut ws.host.last_row, 0, row_bytes)?;
        cpu.qmatmul(self.host.output_weight(), &ws.host.last_row, &mut ws.host.logits)?;
        let logits = self.host.download_logits(&cpu, &ws.host)?;
        kv.len = pos as usize + tokens.len();
        Ok(logits)
    }
}

#[cfg(test)]
mod tests;
