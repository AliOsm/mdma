//! Residual addition and weighted RMSNorm in one dispatch.
use super::*;

#[repr(C)]
#[derive(Clone, Copy)]
struct PcAddNorm {
    w: u64,
    a: u64,
    b: u64,
    sum: u64,
    y: u64,
    dim: u32,
    eps: f32,
}

impl VulkanBackend {
    pub fn with_fused_norm(self, on: bool) -> Self {
        self.set_fused_norm(on);
        self
    }

    pub fn set_fused_norm(&self, on: bool) {
        self.modes
            .change(|| self.fused_norm.store(on, Ordering::Relaxed));
    }

    pub fn fused_norm(&self) -> bool {
        self.fused_norm.load(Ordering::Relaxed)
    }

    pub(super) fn residual_norm(
        &self,
        w: &VkBuf,
        a: &VkBuf,
        b: &VkBuf,
        sum: &mut VkBuf,
        y: &mut VkBuf,
        eps: f32,
    ) -> Result<()> {
        let op = "add_rmsnorm";
        let dim = shape::elems(op, w.len)?;
        let n = shape::elementwise(
            op,
            &[shape::elems(op, a.len)?, shape::elems(op, b.len)?],
            &[shape::elems(op, sum.len)?, shape::elems(op, y.len)?],
        )?;
        let tokens = shape::rows(op, dim, n, y.len / 4)?;
        let dim = u32::try_from(dim).map_err(|_| Error::SizeOverflow)?;
        u32::try_from(n).map_err(|_| Error::SizeOverflow)?;
        let tokens = u32::try_from(tokens).map_err(|_| Error::SizeOverflow)?;
        if self.fused_norm() {
            let pc = PcAddNorm {
                w: w.addr,
                a: a.addr,
                b: b.addr,
                sum: sum.addr,
                y: y.addr,
                dim,
                eps,
            };
            self.launch("add_rmsnorm", &push(&pc), [tokens, 1, 1])
        } else {
            self.add(a, b, sum)?;
            // An oversized sum buffer's canary tail is not another input row.
            let pc = PcNorm {
                w: w.addr,
                x: sum.addr,
                y: y.addr,
                dim,
                eps,
            };
            self.launch("rmsnorm", &push(&pc), [tokens, 1, 1])
        }
    }
}

#[cfg(test)]
mod tests;
