use mdma::backend::Backend;
use mdma::model::Model;

/// Auto f16 is enabled only if every decoder on the shared backend accepts it.
pub(super) fn calibrate<B: Backend + 'static>(backend: &B, model: &Model<B>, all_f16: &mut bool)
where
    B::Buf: 'static,
{
    #[cfg(feature = "vulkan")]
    {
        use mdma::backend::vulkan::VulkanBackend;
        let vk = (backend as &dyn std::any::Any).downcast_ref::<VulkanBackend>();
        let vk_model = (model as &dyn std::any::Any).downcast_ref::<Model<VulkanBackend>>();
        if let (Some(vk), Some(vk_model)) = (vk, vk_model) {
            if vk.int8() {
                let path = if vk.int8_dot() { "packed-dot" } else { "cooperative-matrix" };
                eprintln!("mdma vulkan: experimental int8 {path} prefill on; skipping f16 calibration");
            } else if VulkanBackend::f16_weights_auto() {
                calibrate_shared(
                    all_f16,
                    || match vk.calibrate_f16(vk_model) {
                        Ok(c) => {
                            eprintln!("mdma vulkan: {}", c.summary());
                            Ok(c.f16)
                        }
                        Err(e) => {
                            eprintln!("mdma vulkan: f16 calibration failed, staying f32: {e}");
                            Err(e)
                        }
                    },
                    |on| vk.set_f16_weights(on),
                );
                eprintln!("mdma vulkan: shared f16-staged prefill {}", if *all_f16 { "on" } else { "off" });
            } else {
                eprintln!("mdma vulkan: f16-staged prefill {} (MDMA_VK_F16_WEIGHTS)", if vk.f16_weights() { "on" } else { "off" });
            }
        }
    }
    let _ = (backend, model, all_f16);
}

#[cfg(any(feature = "vulkan", test))]
fn calibrate_shared<E>(all_f16: &mut bool, probe: impl FnOnce() -> Result<bool, E>, apply: impl FnOnce(bool)) {
    // The probe itself changes the backend flag; always restore the aggregate.
    *all_f16 &= probe().unwrap_or(false);
    apply(*all_f16);
}

#[cfg(test)]
#[path = "prefill/tests.rs"]
mod tests;
