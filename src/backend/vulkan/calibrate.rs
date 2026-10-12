//! Select f16 staging only when a fixed prefill probe meets both the
//! speedup and logit-drift thresholds.
use super::VulkanBackend;
use crate::backend::Result;
use crate::model::Model;
use std::time::Instant;

/// Probe length: long enough for several full 64-token tiles.
const PROBE_TOKENS: usize = 128;
/// f16 must beat f32 by this factor to be worth a numerical change.
const MIN_SPEEDUP: f64 = 1.1;
/// Largest allowed logit change, as a fraction of the largest |logit|.
const MAX_LOGIT_DIFF: f32 = 0.01;

#[derive(Debug, Clone, Copy)]
pub struct F16Calibration {
    /// f32 prefill time / f16 prefill time on the probe.
    pub speedup: f64,
    /// Largest |logit_f16 - logit_f32| and the largest |logit_f32|.
    pub max_logit_diff: f32,
    pub logit_scale: f32,
    pub argmax_same: bool,
    /// What the backend was set to.
    pub f16: bool,
}

impl F16Calibration {
    pub fn summary(&self) -> String {
        format!(
            "f16-staged prefill {}: {:.2}x {} than f32, max logit diff {:.3} of {:.1}, top-1 {}",
            if self.f16 { "on" } else { "off" },
            self.speedup,
            if self.speedup >= 1.0 {
                "faster"
            } else {
                "slower"
            },
            self.max_logit_diff,
            self.logit_scale,
            if self.argmax_same { "same" } else { "differs" }
        )
    }
}

impl VulkanBackend {
    /// Prefill a fixed probe with both tile flavours, choose, and set
    /// the backend accordingly. Leaves f32 on any failure.
    pub fn calibrate_f16(&self, model: &Model<Self>) -> Result<F16Calibration> {
        if !self.f16_weights_available() {
            self.set_f16_weights(false);
            return Err(crate::backend::Error::Backend(
                "vulkan: f16 staging unavailable".into(),
            ));
        }
        if self.int8() {
            return Err(crate::backend::Error::Backend(
                "vulkan: disable int8 before f16 calibration".into(),
            ));
        }
        let n_ctx = model.config().n_ctx.min(256).max(PROBE_TOKENS);
        let tokens: Vec<u32> = (0..PROBE_TOKENS as u32)
            .map(|i| 1 + (i * 7919 + 13) % 1000)
            .collect();
        let run = |f16: bool| -> Result<(Vec<f32>, f64)> {
            self.set_f16_weights(f16);
            let mut kv = model.new_kv_cache(self, n_ctx)?;
            let mut ws = model.alloc_workspace(self, PROBE_TOKENS)?;
            // Warm once (pipelines, memory), then time.
            model.prefill(self, &mut kv, &mut ws, &tokens, 0)?;
            let mut kv = model.new_kv_cache(self, n_ctx)?;
            let start = Instant::now();
            let logits = model.prefill(self, &mut kv, &mut ws, &tokens, 0)?;
            Ok((logits, start.elapsed().as_secs_f64()))
        };
        self.set_f16_weights(false);
        let (l32, t32) = run(false)?;
        let (l16, t16) = match run(true) {
            Ok(r) => r,
            Err(e) => {
                self.set_f16_weights(false);
                return Err(e);
            }
        };
        let argmax = |v: &[f32]| {
            v.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
        };
        let max_logit_diff = l32
            .iter()
            .zip(&l16)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        let logit_scale = l32.iter().map(|a| a.abs()).fold(0f32, f32::max);
        let speedup = t32 / t16;
        let f16 = speedup >= MIN_SPEEDUP && max_logit_diff <= MAX_LOGIT_DIFF * logit_scale;
        self.set_f16_weights(f16);
        Ok(F16Calibration {
            speedup,
            max_logit_diff,
            logit_scale,
            argmax_same: argmax(&l32) == argmax(&l16),
            f16,
        })
    }
}
