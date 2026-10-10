//! Native decoder primitives, including Gemma's non-interleaved RoPE.
use super::*;

#[repr(C)]
#[derive(Clone, Copy)]
struct PcUnary {
    x: u64,
    n: u32,
    value: f32,
    op: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcSplitRope {
    x: u64,
    factors: u64,
    n: u32,
    head_dim: u32,
    pos: u32,
    theta: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcAttn {
    q: u64,
    k: u64,
    v: u64,
    o: u64,
    head_dim: u32,
    n_q_heads: u32,
    n_kv_heads: u32,
    n_q: u32,
    prefix: u32,
    scale: f32,
    window: u32,
    pad: u32,
}

impl VulkanBackend {
    pub(super) fn unary(&self, x: &mut VkBuf, value: f32, op: u32) -> Result<()> {
        let n = shape::elems("unary", x.len)? as u32;
        let pc = PcUnary { x: x.addr, n, value, op, pad: 0 };
        self.launch("unary", &push(&pc), [n.div_ceil(UNARY_THREADS), 1, 1])
    }

    pub(super) fn split_rope(&self, x: &mut VkBuf, pos: u32, spec: &RopeSpec, factors: &VkBuf) -> Result<()> {
        let n = shape::elems("rope_split_half", x.len)?;
        shape::rope_split_half(n, shape::elems("rope_split_half", factors.len)?, spec)?;
        let pc = PcSplitRope {
            x: x.addr, factors: factors.addr, n: n as u32,
            head_dim: spec.head_dim as u32, pos, theta: spec.theta,
        };
        self.launch("rope_split_half", &push(&pc), [(n as u32 / 2).div_ceil(SPLIT_ROPE_THREADS), 1, 1])
    }

    pub(super) fn attention(&self, q: &VkBuf, kv: &KvView<VkBuf>, spec: &AttnSpec,
        scale: f32, window: usize, o: &mut VkBuf) -> Result<()> {
        shape::attention_scale(scale)?;
        let shape::Attn { n_q, prefix } = shape::attn(spec,
            shape::elems("attn", q.len)?, kv.len, shape::elems("attn", kv.k.len)?,
            shape::elems("attn", kv.v.len)?, shape::elems("attn", o.len)?)?;
        if spec.head_dim > MAX_HEAD_DIM {
            return Err(Error::ShapeMismatch(format!(
                "attn: head_dim {} exceeds the kernel's {MAX_HEAD_DIM}", spec.head_dim)));
        }
        // Device addresses are u64; element indices inside both shaders are u32.
        u32::try_from(q.len / 4).map_err(|_| Error::SizeOverflow)?;
        let cached = kv.len.checked_mul(spec.n_kv_heads)
            .and_then(|n| n.checked_mul(spec.head_dim)).ok_or(Error::SizeOverflow)?;
        u32::try_from(cached).map_err(|_| Error::SizeOverflow)?;
        let pc = PcAttn {
            q: q.addr, k: kv.k.addr, v: kv.v.addr, o: o.addr,
            head_dim: spec.head_dim as u32, n_q_heads: spec.n_q_heads as u32,
            n_kv_heads: spec.n_kv_heads as u32, n_q: n_q as u32, prefix: prefix as u32,
            scale, window: window.min(kv.len) as u32, pad: 0,
        };
        if self.tiled_attention() && attention::eligible(spec.head_dim, n_q) {
            self.launch("attn_tile", &push(&pc),
                [pc.n_q_heads, pc.n_q.div_ceil(ATTN_QUERY_TILE), 1])
        } else {
            self.launch("attn", &push(&pc), [pc.n_q_heads, pc.n_q, 1])
        }
    }
}
