//! Shared parameter loading and bidirectional transformer building blocks.
use super::weights::{Result as LoadResult, Weights};
use crate::backend::{Backend, EncoderSpec, Pool, QWeight, Result};

pub(crate) struct Norm<B: Backend> {
    w: B::Buf,
    b: Option<B::Buf>,
}

impl<B: Backend> Norm<B> {
    pub fn load(backend: &B, w: &Weights, name: &str, bias: bool) -> LoadResult<Self> {
        Ok(Self {
            w: w.vector(backend, &format!("{name}.weight"), None)?,
            b: if bias { Some(w.vector(backend, &format!("{name}.bias"), None)?) } else { None },
        })
    }

    pub fn forward(&self, backend: &B, x: &B::Buf, y: &mut B::Buf, eps: f32) -> Result<()> {
        backend.layernorm(&self.w, self.b.as_ref(), x, y, eps)
    }
}

pub(crate) struct Linear<B: Backend> {
    pub w: QWeight<B::Buf>,
    b: Option<B::Buf>,
}

impl<B: Backend> Linear<B> {
    pub fn load(backend: &B, w: &Weights, name: &str, bias: bool) -> LoadResult<Self> {
        Ok(Self {
            w: w.matrix(backend, &format!("{name}.weight"), None)?,
            b: if bias { Some(w.vector(backend, &format!("{name}.bias"), None)?) } else { None },
        })
    }

    /// Row slice `i` of a fused `[parts * width, in]` projection and bias.
    pub fn part(backend: &B, w: &Weights, wname: &str, bname: &str, bias: bool, i: usize, width: usize) -> LoadResult<Self> {
        Ok(Self {
            w: w.matrix(backend, wname, Some(i * width..(i + 1) * width))?,
            b: if bias { Some(w.vector(backend, bname, Some(i * width..(i + 1) * width))?) } else { None },
        })
    }

    pub fn forward(&self, backend: &B, x: &B::Buf, y: &mut B::Buf) -> Result<()> {
        backend.qmatmul(&self.w, x, y)?;
        if let Some(b) = &self.b { backend.add_bias(b, y)?; }
        Ok(())
    }
}

/// Shared activation buffers for an encoder and any subsequent head layers.
pub(crate) struct AttentionWorkspace<B: Backend> {
    pub x: B::Buf,
    pub x_alt: B::Buf,
    pub normed: B::Buf,
    q: B::Buf,
    k: B::Buf,
    v: B::Buf,
    attn_out: B::Buf,
    pub proj: B::Buf,
}

impl<B: Backend> AttentionWorkspace<B> {
    pub fn alloc(backend: &B, rows: usize, width: usize) -> Result<Self> {
        let alloc = || backend.alloc(rows * width * 4, Pool::Scratch);
        Ok(Self {
            x: alloc()?, x_alt: alloc()?, normed: alloc()?,
            q: alloc()?, k: alloc()?, v: alloc()?, attn_out: alloc()?, proj: alloc()?,
        })
    }

    /// `x += wo(attn(q(normed), k(normed), v(normed)))`.
    #[allow(clippy::too_many_arguments)]
    pub fn attention(&mut self, backend: &B, lens: &[u32], spec: &EncoderSpec, rope: Option<f32>,
        wq: &Linear<B>, wk: &Linear<B>, wv: &Linear<B>, wo: &Linear<B>) -> Result<()> {
        wq.forward(backend, &self.normed, &mut self.q)?;
        wk.forward(backend, &self.normed, &mut self.k)?;
        wv.forward(backend, &self.normed, &mut self.v)?;
        if let Some(theta) = rope { backend.rope_neox(&mut self.q, &mut self.k, spec, theta)?; }
        backend.attn_bidir(&self.q, &self.k, &self.v, lens, spec, &mut self.attn_out)?;
        wo.forward(backend, &self.attn_out, &mut self.proj)?;
        backend.add(&self.x, &self.proj, &mut self.x_alt)?;
        std::mem::swap(&mut self.x, &mut self.x_alt);
        Ok(())
    }
}
