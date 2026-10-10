//! Preallocated per-layer KV buffers, updated in place.
//! The backend selects the element width through `Backend::kv_elem_bytes`
//! and converts f32 input rows on write.
use crate::backend::{Backend, KvView, Pool, Result, RopeSpec};
use crate::memory::{Reservation, product, sum};

pub struct KvCache<B: Backend> {
    k: Vec<B::Buf>,
    v: Vec<B::Buf>,
    row_elems: Vec<usize>,
    n_ctx: usize,
    elem_bytes: usize,
    len: usize,
    _memory: Option<Reservation>,
}

impl<B: Backend> KvCache<B> {
    pub fn new(backend: &B, n_layers: usize, n_kv_heads: usize, n_ctx: usize, head_dim: usize) -> Result<Self> {
        let width = product(&[n_kv_heads, head_dim])?;
        Self::with_geometry(backend, &vec![width; n_layers], n_ctx)
    }

    /// Only producer layers need storage; callers map shared layers to them.
    pub fn with_geometry(backend: &B, widths: &[usize], n_ctx: usize) -> Result<Self> {
        Self::with_geometry_reserved(backend, widths, n_ctx, None)
    }

    pub(crate) fn allocation_bytes(backend: &B, widths: &[usize], n_ctx: usize) -> Result<usize> {
        product(&[sum(widths.iter().copied())?, backend.kv_elem_bytes(), n_ctx, 2])
    }

    pub(crate) fn with_geometry_reserved(backend: &B, widths: &[usize], n_ctx: usize, memory: Option<Reservation>) -> Result<Self> {
        let elem_bytes = backend.kv_elem_bytes();
        let mut k = Vec::with_capacity(widths.len());
        let mut v = Vec::with_capacity(widths.len());
        if widths.is_empty() || widths.contains(&0) {
            return Err(crate::backend::Error::ShapeMismatch("KV layer geometry must be nonempty".into()));
        }
        Self::allocation_bytes(backend, widths, n_ctx)?;
        for &width in widths {
            let layer_bytes = product(&[width, elem_bytes, n_ctx])?;
            k.push(backend.alloc(layer_bytes, Pool::Kv)?);
            v.push(backend.alloc(layer_bytes, Pool::Kv)?);
        }
        Ok(Self { k, v, row_elems: widths.to_vec(), n_ctx, elem_bytes, len: 0, _memory: memory })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.n_ctx
    }

    /// Bytes of one stored row in the first layer. For heterogeneous caches,
    /// use `layer_row_bytes` with the producer layer's index.
    pub fn row_bytes(&self) -> usize {
        self.layer_row_bytes(0)
    }

    pub fn layer_row_bytes(&self, layer: usize) -> usize {
        self.row_elems[layer] * self.elem_bytes
    }

    /// Write `n_rows` consecutive K/V rows starting at `pos`, straight
    /// from device buffers `k`/`v` holding f32 rows (at least
    /// `n_rows * n_kv_heads * head_dim` floats from their own offset 0),
    /// without a host round trip. Caller drives layer order and calls
    /// `set_len` once after every layer has been written for this
    /// position range. `n_rows == 1` is decode; prefill passes more.
    pub fn write(&mut self, backend: &B, layer: usize, pos: usize, k: &B::Buf, v: &B::Buf, n_rows: usize) -> Result<()> {
        let row_elems = self.row_elems[layer];
        backend.kv_write(k, v, &mut self.k[layer], &mut self.v[layer], pos * row_elems, n_rows * row_elems)
    }

    /// `write` with the RoPE rotation of `q`/`k` folded in: rotates `q` in
    /// place and stores the rotated `k` and `v` rows at `pos`. `k` may not
    /// hold the rotated rows afterwards.
    #[allow(clippy::too_many_arguments)]
    pub fn rope_write(&mut self, backend: &B, layer: usize, pos: usize, q: &mut B::Buf, k: &mut B::Buf, v: &B::Buf, n_rows: usize, spec: &RopeSpec) -> Result<()> {
        let row_elems = self.row_elems[layer];
        backend.rope_kv_write(q, k, v, &mut self.k[layer], &mut self.v[layer], pos * row_elems, pos as u32, n_rows, spec, n_rows * row_elems)
    }

    pub fn set_len(&mut self, len: usize) {
        self.len = len;
    }

    pub fn view(&self, layer: usize) -> KvView<'_, B::Buf> {
        KvView { k: &self.k[layer], v: &self.v[layer], len: self.len }
    }
}

#[cfg(test)]
mod tests;
