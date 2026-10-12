//! F32 wave32 attention with K/V shared across a query tile.
use super::*;
use std::ffi::CStr;

pub(super) fn eligible(head_dim: usize, n_q: usize) -> bool {
    (32..=128).contains(&head_dim) && n_q >= 4
}

pub(super) fn probe(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> Result<bool> {
    // SAFETY: `pdev` was enumerated from `instance` by VulkanBackend::new, the only caller
    // (probe is pub(super)), and both stay live for this call.
    let extensions = unsafe { instance.enumerate_device_extension_properties(pdev) }
        .map_err(vk_err("enumerate device extensions"))?;
    if !extensions.iter().any(|e| {
        // SAFETY: extensionName is NUL-terminated within its fixed array per the spec, and `e`
        // outlives the comparison.
        (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) })
            == ash::ext::subgroup_size_control::NAME
    }) {
        return Ok(false);
    }
    let mut sizes = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
    let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default()
        .push_next(&mut sizes)
        .push_next(&mut subgroup);
    // SAFETY: `pdev` belongs to the live `instance`; `sizes` is chained only after the
    // VK_EXT_subgroup_size_control check above, `subgroup` is core 1.1, and both outlive
    // the call.
    unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
    let limits = props.properties.limits;
    if sizes.min_subgroup_size > 32
        || sizes.max_subgroup_size < 32
        || sizes.max_compute_workgroup_subgroups < 4
        || !sizes
            .required_subgroup_size_stages
            .contains(vk::ShaderStageFlags::COMPUTE)
        || !subgroup
            .supported_stages
            .contains(vk::ShaderStageFlags::COMPUTE)
        || !subgroup
            .supported_operations
            .contains(vk::SubgroupFeatureFlags::ARITHMETIC)
        || limits.max_compute_shared_memory_size < 17920
        || limits.max_compute_work_group_invocations < 128
        || limits.max_compute_work_group_size[0] < 128
    {
        return Ok(false);
    }
    let mut control = vk::PhysicalDeviceSubgroupSizeControlFeatures::default();
    let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut control);
    // SAFETY: `pdev` belongs to the live `instance`; `control` follows the
    // VK_EXT_subgroup_size_control check above and outlives the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut features) };
    Ok(control.subgroup_size_control != vk::FALSE)
}

impl VulkanBackend {
    pub fn with_tiled_attention(self, on: bool) -> Self {
        self.set_tiled_attention(on);
        self
    }

    pub fn set_tiled_attention(&self, on: bool) {
        self.modes.change(|| {
            self.tiled_attention
                .store(on && self.tiled_attention_available(), Ordering::Relaxed)
        });
    }

    pub fn tiled_attention_available(&self) -> bool {
        self.kernels.contains_key("attn_tile")
    }

    pub fn tiled_attention(&self) -> bool {
        self.tiled_attention.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests;
