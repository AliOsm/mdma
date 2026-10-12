//! Gemma primitives and bounded-memory attention for f16/f32 KV caches.
use super::*;

impl HipBackend {
    pub(super) fn unary(&self, x: &mut HipBuf, mut value: f32, mut op: i32) -> Result<()> {
        let n = shape::elems("unary", x.len)?;
        if n == 0 {
            return Ok(());
        }
        let mut n_i = n as i32;
        self.launch1d(
            "unary_kernel",
            n,
            &mut [arg!(x.as_device_ptr()), arg!(n_i), arg!(value), arg!(op)],
        )
    }

    pub(super) fn split_rope(
        &self,
        x: &mut HipBuf,
        mut pos: u32,
        spec: &RopeSpec,
        factors: &HipBuf,
    ) -> Result<()> {
        let n = shape::elems("rope_split_half", x.len)?;
        shape::rope_split_half(n, shape::elems("rope_split_half", factors.len)?, spec)?;
        let mut n_i = n as i32;
        let mut hd = spec.head_dim as i32;
        let mut theta = spec.theta;
        self.launch1d(
            "rope_split_half_kernel",
            n / 2,
            &mut [
                arg!(x.as_device_ptr()),
                arg!(factors.as_device_ptr()),
                arg!(n_i),
                arg!(hd),
                arg!(pos),
                arg!(theta),
            ],
        )
    }

    pub(super) fn attention_generic(
        &self,
        q: &HipBuf,
        kv: &KvView<HipBuf>,
        spec: &AttnSpec,
        mut scale: f32,
        window: usize,
        o: &mut HipBuf,
    ) -> Result<()> {
        shape::attention_scale(scale)?;
        let bytes = self.kv_elem_bytes();
        if !kv.k.len.is_multiple_of(bytes) || !kv.v.len.is_multiple_of(bytes) {
            return Err(Error::ShapeMismatch("attn: partial KV element".into()));
        }
        let shape::Attn { n_q, prefix } = shape::attn(
            spec,
            shape::elems("attn", q.len)?,
            kv.len,
            kv.k.len / bytes,
            kv.v.len / bytes,
            shape::elems("attn", o.len)?,
        )?;
        if spec.head_dim > 512 {
            return Err(Error::ShapeMismatch("attn: head_dim exceeds 512".into()));
        }
        let mut hd = spec.head_dim as i32;
        let mut qh = spec.n_q_heads as i32;
        let mut kh = spec.n_kv_heads as i32;
        let mut prefix = prefix as i32;
        let mut window = window.min(kv.len) as i32;
        let mut f32_kv = i32::from(self.f32_compute);
        let _guard = self.lock.lock().unwrap();
        self.q8_invalidate();
        let func = self
            .module
            .get_function("attn_windowed_kernel")
            .map_err(hip_err)?;
        func.launch(
            Dim3::new_2d(qh as u32, n_q as u32),
            Dim3::new_1d(128),
            0,
            self.stream.as_ref(),
            &mut [
                arg!(q.as_device_ptr()),
                arg!(kv.k.as_device_ptr()),
                arg!(kv.v.as_device_ptr()),
                arg!(o.as_device_ptr()),
                arg!(hd),
                arg!(qh),
                arg!(kh),
                arg!(prefix),
                arg!(scale),
                arg!(window),
                arg!(f32_kv),
            ],
        )
        .map_err(hip_err)?;
        Ok(())
    }

    pub(super) fn partial_rope(
        &self,
        x: &mut HipBuf,
        mut pos: u32,
        spec: &RopeSpec,
        rotary_dim: usize,
    ) -> Result<()> {
        let pairs = shape::rope_partial_interleaved(
            shape::elems("rope_partial_interleaved", x.len)?,
            spec,
            rotary_dim,
        )?;
        let mut pairs_i = i32::try_from(pairs).map_err(|_| Error::SizeOverflow)?;
        let mut hd = i32::try_from(spec.head_dim).map_err(|_| Error::SizeOverflow)?;
        let mut rot = i32::try_from(rotary_dim).map_err(|_| Error::SizeOverflow)?;
        let mut theta = spec.theta;
        self.launch1d(
            "rope_partial_interleaved_kernel",
            pairs,
            &mut [
                arg!(x.as_device_ptr()),
                arg!(pairs_i),
                arg!(hd),
                arg!(rot),
                arg!(pos),
                arg!(theta),
            ],
        )
    }
}
