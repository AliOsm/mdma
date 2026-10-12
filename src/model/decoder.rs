use super::{Model, Workspace};
use crate::backend::{Backend, Result};
use crate::kv_cache::KvCache;

/// A decode executor borrows all weights, KV and workspace until it is dropped.
/// Drop it before prefill or replacing any of those buffers.
pub trait Decoder {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>>;
    fn is_graph(&self) -> bool {
        false
    }
}

pub(crate) struct EagerDecoder<'a, B: Backend> {
    pub backend: &'a B,
    pub model: &'a Model<B>,
    pub kv: &'a mut KvCache<B>,
    pub ws: &'a mut Workspace<B>,
}

impl<B: Backend> Decoder for EagerDecoder<'_, B> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.model
            .forward(self.backend, self.kv, self.ws, token, pos)
    }
}
