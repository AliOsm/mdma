//! Gemma primitives (kernels/decoder_ops.cu): elementwise ops, split-half
//! RoPE and windowed attention over the f16 KV cache up to 512-wide heads.
use super::*;

/// head_dim ceiling of attn_windowed_f16kv (DW_MAX_HEAD_DIM in decoder_ops.cu).
pub(super) const WINDOWED_MAX_HEAD_DIM: usize = 512;

impl CudaBackend {
    /// op 0 scales, op 1 softcaps (unary_f32).
    pub(super) fn unary(&self, x: &mut CudaBuf, value: f32, op: i32) -> Result<()> {
        let n = shape::elems("unary", x.len)?;
        if n == 0 {
            return Ok(());
        }
        let (mut x_ptr, mut n_i, mut value, mut op) = (x.ptr, n as i32, value, op);
        self.launch(
            "unary_f32",
            [(n as u32).div_ceil(256), 1, 1],
            [256, 1, 1],
            &mut params!(x_ptr, n_i, value, op),
        )
    }

    pub(super) fn gelu(&self, a: &CudaBuf, b: &CudaBuf, y: &mut CudaBuf) -> Result<()> {
        let n = shape::elementwise("gelu_tanh_mul", &[shape::elems("gelu", a.len)?,
            shape::elems("gelu", b.len)?], &[shape::elems("gelu", y.len)?])?;
        if n == 0 {
            return Ok(());
        }
        let (mut a_ptr, mut b_ptr, mut y_ptr, mut n_i) = (a.ptr, b.ptr, y.ptr, n as i32);
        self.launch(
            "gelu_tanh_mul_f32",
            [(n as u32).div_ceil(256), 1, 1],
            [256, 1, 1],
            &mut params!(a_ptr, b_ptr, y_ptr, n_i),
        )
    }

    pub(super) fn split_rope(&self, x: &mut CudaBuf, pos: u32, spec: &RopeSpec, factors: &CudaBuf) -> Result<()> {
        self.eager_only("rope_split_half")?;
        let n = shape::elems("rope_split_half", x.len)?;
        shape::rope_split_half(n, shape::elems("rope_split_half", factors.len)?, spec)?;
        let mut fr = rope_freqs("rope_split_half", spec)?;
        let (mut x_ptr, mut f_ptr) = (x.ptr, factors.ptr);
        let (mut n_i, mut hd, mut pos) = (n as i32, spec.head_dim as i32, pos);
        self.launch(
            "rope_split_half_f32",
            [(n as u32 / 2).div_ceil(256), 1, 1],
            [256, 1, 1],
            &mut params!(x_ptr, f_ptr, n_i, hd, pos, fr),
        )
    }

    pub(super) fn attention_windowed(&self, q: &CudaBuf, kv: &KvView<CudaBuf>, spec: &AttnSpec,
        scale: f32, window: usize, o: &mut CudaBuf) -> Result<()> {
        self.eager_only("attn_windowed")?;
        shape::attention_scale(scale)?;
        if !kv.k.len.is_multiple_of(2) || !kv.v.len.is_multiple_of(2) {
            return Err(Error::ShapeMismatch("attn: partial f16 KV element".into()));
        }
        let shape::Attn { n_q, prefix } = shape::attn(spec, shape::elems("attn", q.len)?,
            kv.len, kv.k.len / 2, kv.v.len / 2, shape::elems("attn", o.len)?)?;
        if spec.head_dim > WINDOWED_MAX_HEAD_DIM {
            return Err(Error::ShapeMismatch(format!(
                "attn: head_dim {} exceeds {WINDOWED_MAX_HEAD_DIM}",
                spec.head_dim
            )));
        }
        let (mut q_ptr, mut k_ptr, mut v_ptr, mut o_ptr) = (q.ptr, kv.k.ptr, kv.v.ptr, o.ptr);
        let (mut hd, mut qh, mut kh) = (spec.head_dim as i32, spec.n_q_heads as i32, spec.n_kv_heads as i32);
        let (mut prefix, mut scale) = (prefix as i32, scale);
        let mut window = window.min(kv.len) as i32;
        self.launch(
            "attn_windowed_f16kv",
            [spec.n_q_heads as u32, n_q as u32, 1],
            [128, 1, 1],
            &mut params!(q_ptr, k_ptr, v_ptr, o_ptr, hd, qh, kh, prefix, scale, window),
        )
    }

    /// Q5_K matvec (gemv_q5k): never the int8 or gemv_q paths, which only
    /// know Q4_K and Q6_K.
    pub(super) fn qmatmul_q5k(&self, w: &QWeight<CudaBuf>, x: &CudaBuf, y: &mut CudaBuf,
        n_out: usize, n_in: usize, n_tokens: usize) -> Result<()> {
        shape::kquant_aligned("qmatmul", w.dtype, n_in)?;
        let (mut w_ptr, mut x_ptr, mut y_ptr) = (w.buf.ptr, x.ptr, y.ptr);
        let (mut n_out_i, mut n_in_i) = (n_out as i32, n_in as i32);
        self.launch(
            "gemv_q5k",
            [(n_out as u32).div_ceil(WARPS), n_tokens as u32, 1],
            [32, WARPS, 1],
            &mut params!(w_ptr, x_ptr, y_ptr, n_out_i, n_in_i),
        )
    }
}
