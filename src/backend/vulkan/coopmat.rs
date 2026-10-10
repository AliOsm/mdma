//! Optional wave32 int8 cooperative matrices. Unknown fragment layouts fail closed.
use super::{Error, Result, vk_err};
use ash::vk;
use std::ffi::CStr;

pub(super) const BM: u32 = 64;
pub(super) const BN: u32 = 64;
pub(super) const Q8_BLOCK_BYTES: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Generation {
    Rdna3,
    Rdna4,
}

impl Generation {
    pub(super) fn specialization(self) -> u32 {
        u32::from(self == Self::Rdna4)
    }
}

fn generation(vendor: u32, driver: vk::DriverId, name: &str) -> Option<Generation> {
    if vendor != 0x1002 || driver != vk::DriverId::MESA_RADV {
        return None;
    }
    if name.contains("(RADV GFX12") {
        Some(Generation::Rdna4)
    } else if name.contains("(RADV GFX11") {
        Some(Generation::Rdna3)
    } else {
        None
    }
}

fn matrix_supported(p: &vk::CooperativeMatrixPropertiesKHR<'_>) -> bool {
    p.scope == vk::ScopeKHR::SUBGROUP && p.m_size == 16 && p.n_size == 16 && p.k_size == 16
        && p.a_type == vk::ComponentTypeKHR::SINT8 && p.b_type == vk::ComponentTypeKHR::SINT8
        && p.c_type == vk::ComponentTypeKHR::SINT32 && p.result_type == vk::ComponentTypeKHR::SINT32
        && p.saturating_accumulation == vk::FALSE
}

fn quantize_grid(n_blocks: u32) -> [u32; 3] {
    let groups = n_blocks.div_ceil(16);
    let x = groups.clamp(1, 65535);
    [x, groups.div_ceil(x), 1]
}

pub(super) fn probe(
    entry: &ash::Entry,
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
) -> Result<Option<Generation>> {
    // SAFETY: `pdev` was enumerated from `instance` by VulkanBackend::new, the only caller
    // (probe is pub(super)), and both stay live for this call.
    let extensions = unsafe { instance.enumerate_device_extension_properties(pdev) }
        .map_err(vk_err("enumerate device extensions"))?;
    let has = |name: &CStr| extensions.iter().any(|e| {
        // SAFETY: extensionName is NUL-terminated within its fixed array per the spec, and `e`
        // outlives the comparison.
        (unsafe { CStr::from_ptr(e.extension_name.as_ptr()) }) == name
    });
    if !has(ash::khr::cooperative_matrix::NAME) || !has(ash::ext::subgroup_size_control::NAME) {
        return Ok(None);
    }
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut sizes = vk::PhysicalDeviceSubgroupSizeControlProperties::default();
    let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
    let mut matrix = vk::PhysicalDeviceCooperativeMatrixPropertiesKHR::default();
    let mut props = vk::PhysicalDeviceProperties2::default()
        .push_next(&mut driver).push_next(&mut sizes)
        .push_next(&mut subgroup).push_next(&mut matrix);
    // SAFETY: `pdev` belongs to the live `instance`. `sizes` and `matrix` are chained only
    // after their extensions were found above; `driver` (core 1.2) and `subgroup` (core
    // 1.1) assume a 1.2 device. All four outlive the call.
    unsafe { instance.get_physical_device_properties2(pdev, &mut props) };
    let limits = props.properties.limits;
    // SAFETY: deviceName is NUL-terminated within its fixed array per the spec, and `props`
    // outlives the borrowed Cow.
    let name = unsafe { CStr::from_ptr(props.properties.device_name.as_ptr()) }.to_string_lossy();
    let Some(generation) = generation(props.properties.vendor_id, driver.driver_id, &name) else {
        return Ok(None);
    };
    if sizes.min_subgroup_size > 32 || sizes.max_subgroup_size < 32
        || !sizes.required_subgroup_size_stages.contains(vk::ShaderStageFlags::COMPUTE)
        || !matrix.cooperative_matrix_supported_stages.contains(vk::ShaderStageFlags::COMPUTE)
        || !subgroup.supported_operations.contains(vk::SubgroupFeatureFlags::SHUFFLE)
        || limits.max_compute_shared_memory_size < 22528
        || limits.max_compute_work_group_invocations < 128
        || limits.max_compute_work_group_size[0] < 128
    {
        return Ok(None);
    }
    let mut control = vk::PhysicalDeviceSubgroupSizeControlFeatures::default();
    let mut coop = vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default();
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut features = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut control).push_next(&mut coop).push_next(&mut f12);
    // SAFETY: `pdev` belongs to the live `instance`; `control` and `coop` follow the
    // extension checks above, `f12` is core 1.2, and all three outlive the call.
    unsafe { instance.get_physical_device_features2(pdev, &mut features) };
    if control.subgroup_size_control == vk::FALSE || coop.cooperative_matrix == vk::FALSE
        || f12.vulkan_memory_model == vk::FALSE
    {
        return Ok(None);
    }
    let loader = ash::khr::cooperative_matrix::Instance::new(entry, instance);
    // SAFETY: `loader` was built from the same entry and instance, and `pdev` supports
    // VK_KHR_cooperative_matrix (checked above); ash sizes the output from the count query.
    let matrices = unsafe {
        loader.get_physical_device_cooperative_matrix_properties(pdev)
    }.map_err(vk_err("query cooperative matrices"))?;
    Ok(matrices.iter().any(matrix_supported).then_some(generation))
}

pub(super) fn scratch_bytes(n_in: usize, n_tokens: usize) -> Result<usize> {
    n_in.checked_div(32).and_then(|blocks| blocks.checked_mul(n_tokens))
        .and_then(|blocks| blocks.checked_mul(Q8_BLOCK_BYTES))
        .ok_or(Error::SizeOverflow)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcQuantize {
    x: u64,
    q8: u64,
    n_blocks: u32,
    _pad: u32,
}

impl super::VulkanBackend {
    pub(super) fn qmatmul_int8(
        &self,
        w: &super::QWeight<super::VkBuf>,
        x: &super::VkBuf,
        y: &mut super::VkBuf,
        shape: super::shape::Matmul,
        code: i32,
        dot: bool,
    ) -> Result<()> {
        let super::shape::Matmul { n_out, n_in, n_tokens } = shape;
        let bytes = scratch_bytes(n_in, n_tokens)?;
        let n_blocks = u32::try_from(bytes / Q8_BLOCK_BYTES).map_err(|_| Error::SizeOverflow)?;
        // Shader addresses are u64, but indices into each buffer are u32.
        u32::try_from(bytes / 4).map_err(|_| Error::SizeOverflow)?;
        let weight_bytes = (n_in / 256).checked_mul(super::dtype_code::block_bytes(w.dtype) as usize)
            .and_then(|row| row.checked_mul(n_out)).ok_or(Error::SizeOverflow)?;
        u32::try_from(weight_bytes).map_err(|_| Error::SizeOverflow)?;
        u32::try_from(n_out.checked_mul(n_tokens).ok_or(Error::SizeOverflow)?)
            .map_err(|_| Error::SizeOverflow)?;
        u32::try_from(n_in.checked_mul(n_tokens).ok_or(Error::SizeOverflow)?)
            .map_err(|_| Error::SizeOverflow)?;
        let pc = super::PcGemv {
            w: w.buf.addr, x: 0, y: y.addr,
            n_out: u32::try_from(n_out).map_err(|_| Error::SizeOverflow)?,
            n_in: u32::try_from(n_in).map_err(|_| Error::SizeOverflow)?,
            n_tokens: u32::try_from(n_tokens).map_err(|_| Error::SizeOverflow)?,
            dtype: code as u32,
            block_bytes: super::dtype_code::block_bytes(w.dtype) as u32,
        };
        // Hold across both dispatches: another matmul cannot overwrite its Q8
        // input between quantization and consumption. Queue barriers order reuse.
        let mut scratch = self.int8_scratch.lock().unwrap();
        if scratch.as_ref().is_none_or(|b| b.len < bytes) {
            *scratch = Some(self.buffer(bytes, false)?);
        }
        let q8 = scratch.as_ref().unwrap();
        let quant = PcQuantize { x: x.addr, q8: q8.addr, n_blocks, _pad: 0 };
        self.launch("quantize_q8", &super::push(&quant), quantize_grid(n_blocks))?;
        let pc = super::PcGemv { x: q8.addr, ..pc };
        let (name, bm, bn) = match (dot, code == super::dtype_code::Q4K) {
            (false, true) => ("gemm_q4k_i8", BM, BN),
            (false, false) => ("gemm_q6k_i8", BM, BN),
            (true, true) => ("gemm_q4k_dot", super::dot::BM, super::dot::BN),
            (true, false) => ("gemm_q6k_dot", super::dot::BM, super::dot::BN),
        };
        self.launch(name, &super::push(&pc), [pc.n_out.div_ceil(bm), pc.n_tokens.div_ceil(bn), 1])
    }
}

#[cfg(test)]
pub(super) mod tests;
