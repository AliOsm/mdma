//! Packed signed 4x8 dot products; no cooperative-matrix fragment layout.
use super::{Result, vk_err};
use ash::vk;
use std::ffi::CStr;

pub(super) const BM: u32 = 64;
pub(super) const BN: u32 = 32;

pub(super) fn probe(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> Result<bool> {
    // SAFETY: `pdev` was enumerated from `instance` by VulkanBackend::new, the only caller
    // (probe is pub(super)), and both stay live for this call.
    let extensions = unsafe { instance.enumerate_device_extension_properties(pdev) }
        .map_err(vk_err("enumerate device extensions"))?;
    let has = |name: &CStr| extensions.iter().any(|e| {
        // SAFETY: extensionName is NUL-terminated within its fixed array per the spec, and `e`
        // outlives the comparison.
        (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }) == name
    });
    if !has(ash::khr::shader_integer_dot_product::NAME)
        || !has(ash::ext::subgroup_size_control::NAME)
    {
        return Ok(false);
    }
    let mut sizes = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
    let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
    let mut dot = vk::PhysicalDeviceShaderIntegerDotProductProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default()
        .push_next(&mut sizes).push_next(&mut subgroup).push_next(&mut dot);
    // SAFETY: `pdev` belongs to the live `instance`; `sizes` and `dot` are chained only
    // after their extensions were found above, `subgroup` is core 1.1, and all three
    // outlive the call.
    unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
    let limits = props.properties.limits;
    if dot.integer_dot_product4x8_bit_packed_signed_accelerated == vk::FALSE
        || sizes.min_subgroup_size > 32 || sizes.max_subgroup_size < 32
        || !sizes.required_subgroup_size_stages.contains(vk::ShaderStageFlags::COMPUTE)
        || !subgroup.supported_stages.contains(vk::ShaderStageFlags::COMPUTE)
        || !subgroup.supported_operations.contains(vk::SubgroupFeatureFlags::SHUFFLE)
        || limits.max_compute_shared_memory_size < 15744
        || limits.max_compute_work_group_invocations < 256
        || limits.max_compute_work_group_size[0] < 256
    {
        return Ok(false);
    }
    let mut control = vk::PhysicalDeviceSubgroupSizeControlFeatures::default();
    let mut dot = vk::PhysicalDeviceShaderIntegerDotProductFeatures::default();
    let mut features = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut control).push_next(&mut dot);
    // SAFETY: `pdev` belongs to the live `instance`; `control` and `dot` follow the
    // extension checks above and outlive the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut features) };
    Ok(control.subgroup_size_control != vk::FALSE && dot.shader_integer_dot_product != vk::FALSE)
}

#[cfg(test)]
mod tests;
