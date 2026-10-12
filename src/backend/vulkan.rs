//! Vulkan 1.2 compute with device-local buffers and staged host transfers.
//! Kernels receive buffer addresses through push constants. Operations
//! accumulate in one command buffer until a host transfer submits it.
//! Buffer destruction is deferred until referencing commands complete.
use super::{
    AttnSpec, Backend, Error, KvView, Operation, Pool, QWeight, Result, RopeSpec, dtype_code, shape,
};
use ash::vk;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

mod abi;
mod attention;
mod calibrate;
mod coopmat;
mod decoder;
mod dot;
mod norm;
use abi::*;
pub use calibrate::F16Calibration;

const PUSH_BYTES: u32 = 128;
const STAGING_KEEP: usize = 4 << 20;
const MAX_HEAD_DIM: usize = 512;
/// Output tile of gemm_q.comp (its `BM` x `BN`) and rows per workgroup
/// in gemv_q.comp (its `NR`).
const GEMM_BM: u32 = 128;
const GEMM_BN: u32 = 64;
const GEMV_NR: u32 = 2;
const INT8_OFF: u8 = 0;
const INT8_COOPMAT: u8 = 1;
const INT8_DOT: u8 = 2;

const KERNELS: &[(&str, &[u8])] = &[
    (
        "unary",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/unary.spv")),
    ),
    (
        "gelu_tanh_mul",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gelu_tanh_mul.spv")),
    ),
    (
        "rope_split_half",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/rope_split_half.spv")),
    ),
    (
        "add",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/add.spv")),
    ),
    (
        "silu_mul",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/silu_mul.spv")),
    ),
    (
        "rmsnorm",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/rmsnorm.spv")),
    ),
    (
        "add_rmsnorm",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/add_rmsnorm.spv")),
    ),
    (
        "rope",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/rope.spv")),
    ),
    (
        "attn",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/attn.spv")),
    ),
    (
        "attn_tile",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/attn_tile.spv")),
    ),
    (
        "gemv_f",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemv_f.spv")),
    ),
    (
        "gemv_q",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemv_q.spv")),
    ),
    (
        "gemm_q",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q.spv")),
    ),
    (
        "gemm_q_f16",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q_f16.spv")),
    ),
    (
        "quantize_q8",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/quantize_q8.spv")),
    ),
    (
        "gemm_q4k_i8",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q4k_i8.spv")),
    ),
    (
        "gemm_q6k_i8",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q6k_i8.spv")),
    ),
    (
        "gemm_q4k_dot",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q4k_dot.spv")),
    ),
    (
        "gemm_q6k_dot",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/gemm_q6k_dot.spv")),
    ),
    (
        "embed",
        include_bytes!(concat!(env!("MDMA_VK_SPV_DIR"), "/embed.spv")),
    ),
];

fn vk_err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> Error + '_ {
    move |e| Error::Backend(format!("vulkan {what}: {e}"))
}

pub struct VkBuf {
    inner: Arc<Inner>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Mapped pointer; null for device-local buffers.
    ptr: *mut u8,
    addr: u64,
    len: usize,
    charge: Option<crate::memory::BufferCharge>,
}

impl VkBuf {
    fn host_visible(&self) -> bool {
        !self.ptr.is_null()
    }
}

// SAFETY: the only non-Send field is `ptr`, the host mapping of `memory`, which this
// VkBuf exclusively owns. The mapping stays valid on any thread until Inner::destroy
// unmaps it after the VkBuf is gone; the buffer/memory handles are plain u64 handles
// with no thread affinity, and `inner` is Send + Sync.
unsafe impl Send for VkBuf {}
// SAFETY: through `&VkBuf` the backend only reads the mapping (download, after a fence
// wait) or hands it out as a raw pointer (as_ptr; dereferencing is the caller's unsafe
// contract). Host writes need `&mut VkBuf` (upload) or a buffer the writer just created
// or exclusively holds (alloc, embed, staging). The externally synchronized
// unmap/destroy/free runs once, from Drop.
unsafe impl Sync for VkBuf {}

/// A buffer's Vulkan handles once its owner is gone.
struct Raw {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: bool,
    charge: crate::memory::BufferCharge,
}

impl Drop for VkBuf {
    fn drop(&mut self) {
        let raw = Raw {
            buffer: self.buffer,
            memory: self.memory,
            mapped: self.host_visible(),
            charge: self.charge.take().expect("live buffer charge"),
        };
        // Recorded commands may still name this buffer; only a flush
        // (which waits) makes destroying it safe. With nothing recorded
        // and the recording lock held, nothing is in flight either. A
        // drop on the thread that holds the lock takes the deferred path.
        match self.inner.pending.try_lock() {
            Ok(pending) if pending.submission.is_idle() => self.inner.destroy(raw),
            _ => self.inner.graveyard.lock().unwrap().push(raw),
        }
    }
}

/// The device, shared by the backend and every buffer: a buffer may
/// outlive the backend, and the device outlives them all.
struct Inner {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    mem: vk::PhysicalDeviceMemoryProperties,
    pdev: vk::PhysicalDevice,
    memory_budget: bool,
    pending: Mutex<Pending>,
    /// Buffers dropped while commands that may reference them were
    /// recorded or in flight; destroyed after the next fence wait.
    graveyard: Mutex<Vec<Raw>>,
}

impl Inner {
    fn destroy(&self, mut raw: Raw) {
        // SAFETY: `raw` comes from a dropped VkBuf, the sole owner of these handles, so no other
        // host access to them exists. Callers guarantee no recorded or in-flight command names
        // them: VkBuf::drop calls this only under the pending lock with an idle submission, and
        // bury() runs after a fence wait (flush), under the lock with an idle submission
        // (keep_staging), or after device_wait_idle (both Drops). The memory is unmapped only
        // if buffer() mapped it, and the buffer is destroyed before its memory is freed.
        raw.charge.release(|| unsafe {
            if raw.mapped {
                self.device.unmap_memory(raw.memory);
            }
            self.device.destroy_buffer(raw.buffer, None);
            self.device.free_memory(raw.memory, None);
            true
        });
    }

    fn bury(&self) {
        let dead: Vec<Raw> = std::mem::take(&mut *self.graveyard.lock().unwrap());
        for raw in dead {
            self.destroy(raw);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: last Arc<Inner>: the backend and every VkBuf are gone, so nothing else touches
        // the device. device_wait_idle joins all queue work before the graveyard is freed (on
        // failure everything leaks). VulkanBackend::drop already destroyed its pool, fence,
        // layout, pipelines and modules, unless its own wait failed and leaked them (then a
        // successful wait here destroys the device with live children). The device goes before
        // its instance; `_entry` (the loader) drops only after this body.
        unsafe {
            if self.device.device_wait_idle().is_err() {
                eprintln!("mdma: Vulkan device join failed; deferred buffers remain charged");
                return;
            }
            self.bury();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

struct Kernel {
    module: vk::ShaderModule,
    pipeline: vk::Pipeline,
}

pub struct VulkanBackend {
    inner: Arc<Inner>,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    layout: vk::PipelineLayout,
    kernels: HashMap<&'static str, Kernel>,
    /// Host-visible, coherent (and device-local when the GPU has such a
    /// type): staging and token ids.
    memory_type: u32,
    /// Device-local, preferring a type the host cannot see: every buffer
    /// a kernel reads or writes.
    device_type: u32,
    /// Prefill stages its tiles as f16 (gemm_q_f16) with MDMA_VK_F16_WEIGHTS=1.
    f16_weights: AtomicBool,
    fused_norm: AtomicBool,
    tiled_attention: AtomicBool,
    int8: AtomicU8,
    int8_scratch: Mutex<Option<VkBuf>>,
    buffers: crate::memory::BufferBudget,
    modes: crate::federation::profile::gpu::Modes,
    health: crate::federation::profile::gpu::Health,
    identity: Arc<()>,
}

/// Recording state of the single command buffer: whether it is open with
/// unsubmitted work.
struct Pending {
    submission: super::submission::Submission,
    /// Host-visible bounce buffer for uploads and downloads up to
    /// STAGING_KEEP bytes (logits every token); larger transfers (weights
    /// at load) use a temporary one.
    staging: Option<VkBuf>,
    /// MDMA_VK_PROFILE=1: every op is flushed on its own and its wall time
    /// summed here by name, printed when the backend drops.
    profile: Option<HashMap<String, (u32, std::time::Duration)>>,
}

// SAFETY: every field is a Vulkan handle (no thread affinity), an Arc, an atomic or a
// Mutex; the backend's Vulkan objects are destroyed only in Drop, on whichever thread
// owns it last.
unsafe impl Send for VulkanBackend {}
// SAFETY: the externally synchronized objects (queue, command buffer and its pool,
// fence) are only touched while holding `inner.pending` (begin, barrier, record_copy,
// launch_labeled, alloc, flush) or through `&mut self` in Drop. join_execution's
// unlocked device_wait_idle runs only after the submission is marked failed or the lock
// is poisoned, which stops every other path before it reaches the queue. Kernels,
// layout and memory types are immutable after new().
unsafe impl Sync for VulkanBackend {}

impl Drop for VulkanBackend {
    fn drop(&mut self) {
        self.health.withdraw();
        // SAFETY: `&mut self` excludes every other use of the queue, command buffer and fence.
        // sync() submits and waits for recorded work; device_wait_idle then joins everything,
        // and on failure we return and leak rather than destroy objects that may be in use.
        // Pipelines, modules, layout, fence and pool were created from this device in new() and
        // are destroyed exactly once here (freeing the pool frees `cmd`); the device itself
        // stays alive through `inner`.
        unsafe {
            let _ = self.sync();
            // Dropped outside the lock: VkBuf::drop takes it.
            let staging = self.inner.pending.lock().unwrap().staging.take();
            drop(staging);
            drop(self.int8_scratch.get_mut().unwrap().take());
            if let Some(prof) = self.inner.pending.lock().unwrap().profile.take() {
                let mut rows: Vec<_> = prof.into_iter().collect();
                rows.sort_by_key(|(_, (_, t))| std::cmp::Reverse(*t));
                for (name, (n, t)) in rows {
                    eprintln!(
                        "vulkan profile: {name:28} {n:7} calls {:9.1} ms {:7.1} us/call",
                        t.as_secs_f64() * 1e3,
                        t.as_secs_f64() * 1e6 / f64::from(n)
                    );
                }
            }
            let d = &self.inner.device;
            if d.device_wait_idle().is_err() {
                return;
            }
            self.inner.pending.lock().unwrap().submission = Default::default();
            self.inner.bury();
            for k in self.kernels.values() {
                d.destroy_pipeline(k.pipeline, None);
                d.destroy_shader_module(k.module, None);
            }
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.pool, None);
            // The device itself goes with the last VkBuf (Inner::drop).
        }
    }
}

impl VulkanBackend {
    pub fn new() -> Result<Self> {
        // SAFETY: dlopens the system Vulkan loader, trusted to be a conforming libvulkan
        // (libloading's contract). The Entry moves into Inner::_entry, which outlives every
        // instance/device call made through it; error paths below leak handles but call
        // nothing after it drops.
        let entry = unsafe { ash::Entry::load() }.map_err(vk_err("load libvulkan"))?;
        let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 2, 0));
        // SAFETY: the create info and the `app` it points to outlive the call; no allocation
        // callbacks are passed.
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .map_err(vk_err("create instance"))?;

        // Prefer a discrete GPU, then an integrated one; never the CPU ICD.
        // SAFETY: `instance` was created just above and is not destroyed in this function.
        let pdevs = unsafe { instance.enumerate_physical_devices() }
            .map_err(vk_err("enumerate devices"))?;
        let rank = |t: vk::PhysicalDeviceType| match t {
            vk::PhysicalDeviceType::DISCRETE_GPU => 0,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
            vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
            _ => 9,
        };
        let mut best: Option<(vk::PhysicalDevice, u32, i32)> = None;
        for pdev in pdevs {
            // SAFETY: `pdev` was enumerated from the live `instance`.
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            if props.device_type == vk::PhysicalDeviceType::CPU {
                continue;
            }
            // SAFETY: `pdev` was enumerated from the live `instance`; ash sizes the output from
            // the count query.
            let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let Some(qf) = families
                .iter()
                .position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE))
            else {
                continue;
            };
            let r = rank(props.device_type);
            if best.is_none_or(|(_, _, br)| r < br) {
                best = Some((pdev, qf as u32, r));
            }
        }
        let (pdev, qf, _) =
            best.ok_or_else(|| Error::Backend("vulkan: no GPU with a compute queue".into()))?;

        // gemm_q reduces with subgroupAdd and keeps one partial per
        // subgroup for up to 8 subgroups of a 64-lane workgroup.
        let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut subgroup);
        // SAFETY: `pdev` belongs to the live `instance`; the only chained struct, `subgroup`
        // (core 1.1), outlives the call. The device's apiVersion is assumed, not checked, to
        // be >= 1.2.
        unsafe { instance.get_physical_device_properties2(pdev, &mut props2) };
        if subgroup.subgroup_size < 8
            || !subgroup
                .supported_stages
                .contains(vk::ShaderStageFlags::COMPUTE)
            || !subgroup
                .supported_operations
                .contains(vk::SubgroupFeatureFlags::ARITHMETIC)
        {
            return Err(Error::Backend(format!(
                "vulkan: need compute subgroups of >= 8 lanes with arithmetic ops (size {}, ops {:?})",
                subgroup.subgroup_size, subgroup.supported_operations
            )));
        }

        let coopmat = match coopmat::probe(&entry, &instance, pdev) {
            Ok(support) => support,
            Err(e) => {
                eprintln!(
                    "mdma: Vulkan int8 capability probe failed, keeping existing kernels: {e}"
                );
                None
            }
        };
        let dot = match dot::probe(&instance, pdev) {
            Ok(support) => support,
            Err(e) => {
                eprintln!("mdma: Vulkan packed-dot probe failed, keeping existing kernels: {e}");
                false
            }
        };
        let tiled_attention = match attention::probe(&instance, pdev) {
            Ok(support) => support,
            Err(e) => {
                eprintln!(
                    "mdma: Vulkan tiled-attention probe failed, keeping existing kernel: {e}"
                );
                false
            }
        };
        let priorities = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&priorities)];
        let features = vk::PhysicalDeviceFeatures::default().shader_int64(true);
        // shaderInt8: the matvec converts nibbles with unpack8.
        // FP16 arithmetic is optional: older GPUs can run the baseline
        // kernels (including packed f16 weights) without the staging experiment.
        let mut supported12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut supported = vk::PhysicalDeviceFeatures2::default().push_next(&mut supported12);
        // SAFETY: `pdev` belongs to the live `instance`; the chained `supported12` outlives
        // the call. PhysicalDeviceVulkan12Features assumes a 1.2 device (apiVersion not
        // checked).
        unsafe { instance.get_physical_device_features2(pdev, &mut supported) };
        let shader_float16 = supported12.shader_float16 == vk::TRUE;
        let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
            .buffer_device_address(true)
            .shader_int8(true)
            .shader_float16(shader_float16)
            .vulkan_memory_model(coopmat.is_some());
        let mut coop_features =
            vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default().cooperative_matrix(true);
        let mut size_features =
            vk::PhysicalDeviceSubgroupSizeControlFeatures::default().subgroup_size_control(true);
        let mut dot_features = vk::PhysicalDeviceShaderIntegerDotProductFeatures::default()
            .shader_integer_dot_product(true);
        // SAFETY: `pdev` was enumerated from the live `instance`.
        let memory_budget = unsafe { instance.enumerate_device_extension_properties(pdev) }
            .map_err(vk_err("enumerate extensions"))?
            .iter()
            .any(|extension| {
                // SAFETY: the spec requires extensionName to be NUL-terminated within its
                // fixed array, which lives in the Vec for the whole comparison.
                (unsafe { std::ffi::CStr::from_ptr(extension.extension_name.as_ptr()) })
                    == ash::ext::memory_budget::NAME
            });
        let mut extensions = Vec::new();
        if memory_budget {
            extensions.push(ash::ext::memory_budget::NAME.as_ptr());
        }
        if coopmat.is_some() || dot || tiled_attention {
            extensions.push(ash::ext::subgroup_size_control::NAME.as_ptr());
        }
        if coopmat.is_some() {
            extensions.push(ash::khr::cooperative_matrix::NAME.as_ptr());
        }
        if dot {
            extensions.push(ash::khr::shader_integer_dot_product::NAME.as_ptr());
        }
        let mut dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qci)
            .enabled_features(&features)
            .enabled_extension_names(&extensions)
            .push_next(&mut f12);
        if coopmat.is_some() || dot || tiled_attention {
            dci = dci.push_next(&mut size_features);
        }
        if coopmat.is_some() {
            dci = dci.push_next(&mut coop_features);
        }
        if dot {
            dci = dci.push_next(&mut dot_features);
        }
        let device =
            // SAFETY: everything `dci` points to (qci and its priorities, features, the 'static
            // extension names, the f12/size/coop/dot pNext structs) outlives the call. Each
            // extension is enabled only when the memory_budget scan or the matching probe found it
            // on `pdev`, and each feature struct is chained only with its extension; core feature
            // bits the device lacks make the call fail with FEATURE_NOT_PRESENT. Chaining
            // PhysicalDeviceVulkan12Features assumes a 1.2 device; apiVersion is not checked.
            unsafe { instance.create_device(pdev, &dci, None) }.map_err(vk_err("create device"))?;
        // SAFETY: `device` was created with one queue (qci) in family `qf`, so index 0 exists.
        let queue = unsafe { device.get_device_queue(qf, 0) };

        // SAFETY: `pdev` was enumerated from the live `instance`.
        let mem = unsafe { instance.get_physical_device_memory_properties(pdev) };
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let unified = host | vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let types = &mem.memory_types[..mem.memory_type_count as usize];
        let memory_type = types
            .iter()
            .position(|t| t.property_flags.contains(unified))
            .or_else(|| types.iter().position(|t| t.property_flags.contains(host)))
            .ok_or_else(|| Error::Backend("vulkan: no host-visible coherent memory type".into()))?
            as u32;
        let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let device_type = types
            .iter()
            .position(|t| {
                t.property_flags.contains(local)
                    && !t
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
            })
            .or_else(|| types.iter().position(|t| t.property_flags.contains(local)))
            .map_or(memory_type, |i| i as u32);

        // SAFETY: `device` is live and `qf` is the family its queue was created in; the create
        // info outlives the call.
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qf)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .map_err(vk_err("create command pool"))?;
        // SAFETY: `pool` was just created from `device`; the allocate info outlives the call.
        let cmd = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(vk_err("allocate command buffer"))?[0];
        // SAFETY: `device` is live; the fence is created unsignaled, as flush() requires.
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(vk_err("create fence"))?;

        let range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(PUSH_BYTES)];
        // SAFETY: `range` outlives the call; its PUSH_BYTES (128) size is the spec-guaranteed
        // minimum maxPushConstantsSize.
        let layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&range),
                None,
            )
        }
        .map_err(vk_err("create pipeline layout"))?;

        let mut kernels = HashMap::new();
        for (name, spv) in KERNELS {
            let coop_kernel = matches!(*name, "gemm_q4k_i8" | "gemm_q6k_i8");
            let dot_kernel = matches!(*name, "gemm_q4k_dot" | "gemm_q6k_dot");
            let int8_kernel = *name == "quantize_q8" || coop_kernel || dot_kernel;
            let tiled_kernel = *name == "attn_tile";
            let f16_kernel = *name == "gemm_q_f16";
            let optional_kernel = int8_kernel || tiled_kernel || f16_kernel;
            if (coop_kernel && coopmat.is_none())
                || (dot_kernel && !dot)
                || (*name == "quantize_q8" && coopmat.is_none() && !dot)
                || (tiled_kernel && !tiled_attention)
                || (f16_kernel && !shader_float16)
            {
                continue;
            }
            let words = ash::util::read_spv(&mut Cursor::new(spv)).map_err(vk_err("read spirv"))?;
            // SAFETY: read_spv returned u32 words with the SPIR-V magic checked, from kernels
            // glslang compiled at build time. Kernels needing optional capabilities (coopmat, dot,
            // attn_tile, f16) were skipped above unless the device supports them; the baseline
            // int8/int64/buffer-device-address features were required at device creation.
            let module = unsafe {
                device
                    .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
            }
            .map_err(vk_err("create shader module"))?;
            let spec_value = coopmat
                .map_or(0, coopmat::Generation::specialization)
                .to_ne_bytes();
            let spec_entries = [vk::SpecializationMapEntry::default()
                .constant_id(0)
                .offset(0)
                .size(4)];
            let spec = vk::SpecializationInfo::default()
                .map_entries(&spec_entries)
                .data(&spec_value);
            let mut required_size =
                vk::PipelineShaderStageRequiredSubgroupSizeCreateInfo::default()
                    .required_subgroup_size(32);
            let mut stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            if int8_kernel || tiled_kernel {
                stage = stage.push_next(&mut required_size);
                if coop_kernel {
                    stage = stage.specialization_info(&spec);
                }
            }
            let ci = [vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(layout)];
            // SAFETY: `ci`, `stage`, `spec` (one 4-byte entry at offset 0 of the 4-byte
            // `spec_value`) and `required_size` outlive the call; `module` and `layout` are live
            // handles of `device`. The required-subgroup-size struct is chained only for kernels
            // whose probe confirmed subgroupSizeControl with 32 in [min, max] for compute. Device
            // limits are only partly probed: maxComputeWorkgroupSubgroups only for attn_tile, and
            // nothing for the baseline kernels (256-lane workgroups, 24 KiB shared in gemm_q).
            let pipeline = match unsafe {
                device.create_compute_pipelines(vk::PipelineCache::null(), &ci, None)
            } {
                Ok(pipelines) => pipelines[0],
                Err((partial, e)) => {
                    // SAFETY: the partial pipelines (possibly VK_NULL_HANDLE, which destroy
                    // accepts) and `module` were never bound or recorded; each is destroyed
                    // once before we move on.
                    unsafe {
                        for pipeline in partial {
                            device.destroy_pipeline(pipeline, None);
                        }
                        device.destroy_shader_module(module, None);
                    }
                    if optional_kernel {
                        eprintln!("mdma: Vulkan optional pipeline {name} failed: {e}");
                        continue;
                    }
                    return Err(Error::Backend(format!(
                        "vulkan create pipeline {name}: {e}"
                    )));
                }
            };
            kernels.insert(*name, Kernel { module, pipeline });
        }
        // Each optional path fails independently; both need the quantizer.
        for group in [
            ["gemm_q4k_i8", "gemm_q6k_i8"],
            ["gemm_q4k_dot", "gemm_q6k_dot"],
        ] {
            if !kernels.contains_key("quantize_q8")
                || !group.iter().all(|name| kernels.contains_key(name))
            {
                for name in group {
                    if let Some(kernel) = kernels.remove(name) {
                        // SAFETY: the backend does not exist yet, so no command buffer has
                        // recorded this kernel; it left the map, so its pipeline and module are
                        // destroyed exactly once.
                        unsafe {
                            device.destroy_pipeline(kernel.pipeline, None);
                            device.destroy_shader_module(kernel.module, None);
                        }
                    }
                }
            }
        }
        if !kernels.contains_key("gemm_q4k_i8")
            && !kernels.contains_key("gemm_q4k_dot")
            && let Some(kernel) = kernels.remove("quantize_q8")
        {
            // SAFETY: no command buffer has been recorded yet (the backend is built below); the
            // kernel left the map, so its pipeline and module are destroyed exactly once.
            unsafe {
                device.destroy_pipeline(kernel.pipeline, None);
                device.destroy_shader_module(kernel.module, None);
            }
        }

        let inner = Arc::new(Inner {
            pdev,
            memory_budget,
            _entry: entry,
            instance,
            device,
            mem,
            pending: Mutex::new(Pending {
                submission: Default::default(),
                staging: None,
                profile: std::env::var_os("MDMA_VK_PROFILE")
                    .is_some_and(|v| v == "1")
                    .then(HashMap::new),
            }),
            graveyard: Mutex::new(Vec::new()),
        });
        let backend = Self {
            inner,
            queue,
            pool,
            cmd,
            fence,
            layout,
            kernels,
            memory_type,
            device_type,
            f16_weights: AtomicBool::new(false),
            fused_norm: AtomicBool::new(
                !std::env::var_os("MDMA_VK_FUSED_NORM").is_some_and(|v| v == "0"),
            ),
            tiled_attention: AtomicBool::new(false),
            int8: AtomicU8::new(INT8_OFF),
            int8_scratch: Mutex::new(None),
            buffers: crate::memory::BufferBudget::default(),
            modes: Default::default(),
            health: Default::default(),
            identity: Arc::new(()),
        };
        let requested_f16 = std::env::var_os("MDMA_VK_F16_WEIGHTS").is_some_and(|v| v == "1");
        backend.set_f16_weights(requested_f16);
        if requested_f16 && !backend.f16_weights() {
            eprintln!("mdma: requested Vulkan f16 staging unavailable; using f32 prefill kernels");
        }
        let requested = std::env::var_os("MDMA_VK_INT8");
        backend
            .set_tiled_attention(!std::env::var_os("MDMA_VK_TILED_ATTN").is_some_and(|v| v == "0"));
        if requested.as_ref().is_some_and(|v| v == "dot") {
            backend.set_int8_dot(true);
        } else if requested.as_ref().is_some_and(|v| v == "1") {
            backend.set_int8(true);
        }
        if requested.is_some_and(|v| v == "1" || v == "dot") && !backend.int8() {
            eprintln!(
                "mdma: requested Vulkan int8 path unavailable; using existing prefill kernels"
            );
        }
        Ok(backend)
    }

    pub(crate) fn federation_profile(
        &self,
    ) -> std::io::Result<crate::federation::profile::gpu::Gpu> {
        use crate::federation::profile::gpu::{Gpu, fields};
        use sha2::{Digest, Sha256};
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut ids = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default()
            .push_next(&mut driver)
            .push_next(&mut ids);
        // SAFETY: `inner.pdev` was enumerated from `inner.instance`, both live while `self` is;
        // the chained `driver` (core 1.2) and `ids` (core 1.1) outlive the call.
        unsafe {
            self.inner
                .instance
                .get_physical_device_properties2(self.inner.pdev, &mut properties)
        };
        let properties = properties.properties;
        // SAFETY: deviceName, driverName and driverInfo are NUL-terminated within their fixed
        // arrays per the spec; `properties` and `driver` are locals that outlive these Cows.
        let name =
            unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
        let driver_name =
            unsafe { std::ffi::CStr::from_ptr(driver.driver_name.as_ptr()) }.to_string_lossy();
        let driver_info =
            unsafe { std::ffi::CStr::from_ptr(driver.driver_info.as_ptr()) }.to_string_lossy();
        let mut kernels: Vec<_> = KERNELS
            .iter()
            .filter(|(name, _)| self.kernels.contains_key(name))
            .map(|(name, bytes)| format!("{name}:{}", hex::encode(Sha256::digest(bytes))))
            .collect();
        kernels.sort();
        self.modes.capture(|| {
            Gpu::immutable(
                "vulkan",
                "f32",
                "f32",
                fields([
                    ("vocabulary", "mdma.vulkan.v1".into()),
                    ("workload", "decoder".into()),
                    ("device_name", name.into_owned()),
                    ("device_uuid", hex::encode(ids.device_uuid)),
                    ("vendor_id", properties.vendor_id.to_string()),
                    ("device_id", properties.device_id.to_string()),
                    ("driver_id", driver.driver_id.as_raw().to_string()),
                    ("driver_version", properties.driver_version.to_string()),
                    ("driver_name", driver_name.into_owned()),
                    ("driver_info", driver_info.into_owned()),
                    ("driver_uuid", hex::encode(ids.driver_uuid)),
                    ("api_version", properties.api_version.to_string()),
                    ("kernel_ids", kernels.join(",")),
                    ("weights", "gguf-raw-matrices-f32-norms".into()),
                    ("f16_staging", self.f16_weights().to_string()),
                    ("int8_mode", self.int8.load(Ordering::Relaxed).to_string()),
                    ("fused_norm", self.fused_norm().to_string()),
                    ("tiled_attention", self.tiled_attention().to_string()),
                    ("execution", "recorded-commands-fence-joined".into()),
                    ("device_memory_type", self.device_type.to_string()),
                    ("staging_memory_type", self.memory_type.to_string()),
                    ("target_arch", std::env::consts::ARCH.into()),
                    ("target_os", std::env::consts::OS.into()),
                ]),
                self.health.clone(),
            )
        })
    }

    /// Prefill with the f16-staged weight tile (the MDMA_VK_F16_WEIGHTS
    /// experiment), for tests that measure it without touching the env.
    /// Unavailable devices retain f32 staging.
    pub fn with_f16_weights(self, on: bool) -> Self {
        self.set_f16_weights(on);
        self
    }

    pub fn set_f16_weights(&self, on: bool) {
        self.modes.change(|| {
            self.f16_weights
                .store(on && self.f16_weights_available(), Ordering::Relaxed)
        });
    }

    /// Whether the device supports FP16 arithmetic and its staging pipeline
    /// was successfully created. Packed f16 model weights do not require this.
    pub fn f16_weights_available(&self) -> bool {
        self.kernels.contains_key("gemm_q_f16")
    }

    pub fn f16_weights(&self) -> bool {
        self.f16_weights.load(Ordering::Relaxed)
    }

    /// Experimental Q4_K/Q6_K prefill with requantized Q8_1 activations.
    /// Unavailable devices retain their existing kernels.
    pub fn with_int8(self, on: bool) -> Self {
        self.set_int8(on);
        self
    }

    pub fn set_int8(&self, on: bool) {
        let mode = if !on {
            INT8_OFF
        } else if self.int8_coopmat_available() {
            INT8_COOPMAT
        } else if self.int8_dot_available() {
            INT8_DOT
        } else {
            INT8_OFF
        };
        self.modes
            .change(|| self.int8.store(mode, Ordering::Relaxed));
    }

    pub fn int8_available(&self) -> bool {
        self.int8_coopmat_available() || self.int8_dot_available()
    }

    pub fn int8_coopmat_available(&self) -> bool {
        ["quantize_q8", "gemm_q4k_i8", "gemm_q6k_i8"]
            .iter()
            .all(|name| self.kernels.contains_key(name))
    }

    /// Force the RDNA2-compatible packed-dot path, including on newer GPUs.
    /// Missing accelerated signed 4x8 dot support leaves int8 disabled.
    pub fn with_int8_dot(self, on: bool) -> Self {
        self.set_int8_dot(on);
        self
    }

    pub fn set_int8_dot(&self, on: bool) {
        self.modes.change(|| {
            self.int8.store(
                if on && self.int8_dot_available() {
                    INT8_DOT
                } else {
                    INT8_OFF
                },
                Ordering::Relaxed,
            )
        });
    }

    pub fn int8_dot_available(&self) -> bool {
        ["quantize_q8", "gemm_q4k_dot", "gemm_q6k_dot"]
            .iter()
            .all(|name| self.kernels.contains_key(name))
    }

    pub fn int8_dot(&self) -> bool {
        self.int8.load(Ordering::Relaxed) == INT8_DOT
    }

    pub fn int8(&self) -> bool {
        self.int8.load(Ordering::Relaxed) != INT8_OFF
    }

    fn dev(&self) -> &ash::Device {
        &self.inner.device
    }

    /// The preferred type for `host`/device buffers if the buffer's
    /// requirements allow it, else the first allowed type with the same
    /// properties.
    fn memory_type_for(&self, bits: u32, host: bool) -> Result<u32> {
        let preferred = if host {
            self.memory_type
        } else {
            self.device_type
        };
        if bits & (1 << preferred) != 0 {
            return Ok(preferred);
        }
        let need = if host {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        } else {
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        };
        let mem = &self.inner.mem;
        mem.memory_types[..mem.memory_type_count as usize]
            .iter()
            .enumerate()
            .find(|(i, t)| bits & (1 << i) != 0 && t.property_flags.contains(need))
            .map(|(i, _)| i as u32)
            .ok_or_else(|| {
                Error::Backend(format!("vulkan: no memory type for {need:?} in {bits:#x}"))
            })
    }

    /// A buffer on the host-visible type (mapped) or the device-local one.
    fn buffer(&self, bytes: usize, host: bool) -> Result<VkBuf> {
        // Kernels read whole 32-bit words: a 210-byte Q6_K block or an
        // odd f16 row must not end mid-word.
        let size = bytes
            .max(16)
            .checked_next_multiple_of(4)
            .ok_or(Error::SizeOverflow)? as u64;
        let usage = vk::BufferUsageFlags::STORAGE_BUFFER
            | vk::BufferUsageFlags::TRANSFER_SRC
            | vk::BufferUsageFlags::TRANSFER_DST
            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS;
        let device = self.dev();
        // SAFETY: the create info outlives the call; `size` is non-zero (>= 16, a multiple of
        // 4, overflow-checked above). SHADER_DEVICE_ADDRESS usage relies on
        // bufferDeviceAddress, which device creation required.
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(vk_err("create buffer"))?;
        // SAFETY: `buffer` was just created from this device and not yet destroyed.
        let reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
        let mut flags =
            vk::MemoryAllocateFlagsInfo::default().flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
        let memory_type = match self.memory_type_for(reqs.memory_type_bits, host) {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: `buffer` was created above and never bound or recorded; destroyed once
                // before returning the error.
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(memory_type)
            .push_next(&mut flags);
        // Host-visible BAR/unified memory can still live on a device-local heap.
        let heap = self.inner.mem.memory_types[memory_type as usize].heap_index as usize;
        let domain = if self.inner.mem.memory_heaps[heap]
            .flags
            .contains(vk::MemoryHeapFlags::DEVICE_LOCAL)
        {
            crate::memory::MemoryDomain::Device
        } else {
            crate::memory::MemoryDomain::Host
        };
        let allocation = usize::try_from(reqs.size)
            .map_err(|_| Error::SizeOverflow)
            .and_then(|bytes| {
                self.buffers.allocate(domain, bytes, || {
                    // SAFETY: `ai` and its chained `flags` live in the enclosing frame; memory_type_for
                    // returned an index below memory_type_count whose bit is set in
                    // reqs.memory_type_bits, and the DEVICE_ADDRESS flag relies on bufferDeviceAddress
                    // (enabled). The live allocation count is not tracked against
                    // maxMemoryAllocationCount (one allocation per buffer).
                    unsafe { device.allocate_memory(&ai, None) }.map_err(vk_err("allocate memory"))
                })
            });
        let (memory, mut charge) = match allocation {
            Ok(allocation) => allocation,
            Err(e) => {
                // SAFETY: allocation failed, so `buffer` was never bound or recorded;
                // destroyed once.
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        // SAFETY: `buffer` is unbound and `memory` was allocated for reqs.size from a type in
        // reqs.memory_type_bits, so offset 0 meets the size and alignment requirements.
        let bound = unsafe { device.bind_buffer_memory(buffer, memory, 0) }.and_then(|()| {
            if host {
                // SAFETY: with `host`, memory_type_for picked a HOST_VISIBLE | HOST_COHERENT type;
                // `memory` is freshly allocated and not yet mapped.
                unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
            } else {
                Ok(std::ptr::null_mut())
            }
        });
        let ptr = match bound {
            Ok(p) => p.cast::<u8>(),
            Err(e) => {
                // SAFETY: bind or map failed before the buffer escaped or was recorded, and
                // `memory` is not mapped; the buffer is destroyed before its memory is freed.
                charge.release(|| unsafe {
                    device.destroy_buffer(buffer, None);
                    device.free_memory(memory, None);
                    true
                });
                return Err(Error::Backend(format!("vulkan bind/map: {e}")));
            }
        };
        // SAFETY: `buffer` has SHADER_DEVICE_ADDRESS usage and is bound in full to `memory`,
        // allocated with the DEVICE_ADDRESS flag; bufferDeviceAddress is enabled.
        let addr = unsafe {
            device.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        Ok(VkBuf {
            inner: self.inner.clone(),
            buffer,
            memory,
            ptr,
            addr,
            len: bytes,
            charge: Some(charge),
        })
    }

    /// Open the command buffer if it is not, so a command can be recorded.
    fn begin(&self, pending: &mut Pending) -> Result<()> {
        pending.submission.begin(|| {
            // SAFETY: `pending` borrows the locked state, so `cmd` is externally synchronized.
            // Submission::begin runs this only when nothing is open and nothing failed, so any
            // earlier submission of `cmd` completed (flush waits on the fence before clearing
            // `open`). The pool was created with RESET_COMMAND_BUFFER.
            unsafe {
                self.dev()
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
                    .map_err(vk_err("reset command buffer"))?;
                self.dev()
                    .begin_command_buffer(
                        self.cmd,
                        &vk::CommandBufferBeginInfo::default()
                            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )
                    .map_err(vk_err("begin command buffer"))?;
            }
            Ok(())
        })
    }

    /// Every recorded command's writes visible to every later command's
    /// reads and writes, compute or transfer.
    fn barrier(&self) {
        let access = vk::AccessFlags::SHADER_READ
            | vk::AccessFlags::SHADER_WRITE
            | vk::AccessFlags::TRANSFER_READ
            | vk::AccessFlags::TRANSFER_WRITE;
        let stages = vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER;
        let mb = [vk::MemoryBarrier::default()
            .src_access_mask(access)
            .dst_access_mask(access)];
        // SAFETY: only record_copy, launch_labeled and alloc call this, each after begin() and
        // while holding the pending lock, so `cmd` is recording and externally synchronized;
        // `mb` outlives the call, and compute queues support the TRANSFER stage.
        unsafe {
            self.dev().cmd_pipeline_barrier(
                self.cmd,
                stages,
                stages,
                vk::DependencyFlags::empty(),
                &mb,
                &[],
                &[],
            )
        };
    }

    /// Submit the open command buffer and wait for it. Host reads and
    /// writes of device memory go through here first.
    fn flush(&self, pending: &mut Pending) -> Result<()> {
        pending.submission.finish(|| {
            let d = self.dev();
            // A fence alone does not make device writes visible to the host;
            // this dependency does (the host reads logits and staging next).
            let mb = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)];
            // SAFETY: `pending` borrows the locked state, so `cmd`, `queue` and `fence` are
            // externally synchronized. Submission::finish runs this only while `cmd` is open
            // (recording). The fence is unsignaled: created that way and reset after every
            // successful wait; any failure leaves the submission failed, so `cmd` and `fence` are
            // never reused. `mb`, `cmds` and `submit` outlive their calls, and the unbounded wait
            // completes the work before `cmd` is reset or the host reads results.
            unsafe {
                d.cmd_pipeline_barrier(
                    self.cmd,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &mb,
                    &[],
                    &[],
                );
                d.end_command_buffer(self.cmd)
                    .map_err(vk_err("end command buffer"))?;
                let cmds = [self.cmd];
                let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
                d.queue_submit(self.queue, &submit, self.fence)
                    .map_err(vk_err("queue submit"))?;
                d.wait_for_fences(&[self.fence], true, u64::MAX)
                    .map_err(vk_err("wait fence"))?;
                d.reset_fences(&[self.fence])
                    .map_err(vk_err("reset fence"))?;
            }
            Ok(())
        })?;
        self.inner.bury();
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let mut pending = self
            .inner
            .pending
            .lock()
            .map_err(|_| Error::Backend("Vulkan submission lock poisoned".into()))?;
        self.flush(&mut pending)
    }

    /// A host-visible buffer of at least `bytes`: the kept one, grown if
    /// needed, or a temporary for large transfers. The caller puts a
    /// kept buffer back with `keep_staging`.
    fn take_staging(&self, pending: &mut Pending, bytes: usize) -> Result<VkBuf> {
        if bytes > STAGING_KEEP {
            return self.buffer(bytes, true);
        }
        match pending.staging.take() {
            Some(buf) if buf.len >= bytes => Ok(buf),
            _ => self.buffer(STAGING_KEEP, true),
        }
    }

    fn keep_staging(&self, pending: &mut Pending, buf: VkBuf) {
        if buf.len <= STAGING_KEEP {
            pending.staging = Some(buf);
        } else {
            drop(buf);
        }
        if pending.submission.is_idle() {
            // The temporary dropped under this lock went to the graveyard,
            // but the transfer already completed. Do not retain it until a
            // later allocation which may itself need these budget bytes.
            self.inner.bury();
        }
    }

    /// Record a buffer-to-buffer copy, in order with the dispatches
    /// around it, followed by a barrier. Not submitted.
    fn record_copy(
        &self,
        pending: &mut Pending,
        src: &VkBuf,
        src_off: usize,
        dst: &VkBuf,
        dst_off: usize,
        len: usize,
    ) -> Result<()> {
        self.begin(pending)?;
        let region = [vk::BufferCopy::default()
            .src_offset(src_off as u64)
            .dst_offset(dst_off as u64)
            .size(len as u64)];
        // SAFETY: begin() opened `cmd` under the pending lock. Callers range-check both regions
        // (shape::range in upload/download/copy; take_staging gives len >= the transfer) and
        // skip len 0. `src` and `dst` are distinct buffers (a & and a &mut borrow, or a private
        // staging buffer) with TRANSFER usage, and dropping either while `cmd` is open defers
        // its destruction to the graveyard.
        unsafe {
            self.dev()
                .cmd_copy_buffer(self.cmd, src.buffer, dst.buffer, &region)
        };
        self.barrier();
        Ok(())
    }

    /// Record one dispatch with `push` as the push-constant block, followed
    /// by a barrier. Not submitted until the host needs the result.
    fn launch(&self, name: &'static str, push: &[u8], grid: [u32; 3]) -> Result<()> {
        self.launch_labeled(name, push, grid, name)
    }

    fn launch_binary_f32(
        &self,
        op: &'static str,
        a: &VkBuf,
        b: &VkBuf,
        y: &mut VkBuf,
    ) -> Result<()> {
        let n = shape::elementwise(
            op,
            &[shape::elems(op, a.len)?, shape::elems(op, b.len)?],
            &[shape::elems(op, y.len)?],
        )?;
        let pc = PcElem {
            a: a.addr,
            b: b.addr,
            y: y.addr,
            n: n as u32,
        };
        self.launch(op, &push(&pc), [(n as u32).div_ceil(BINARY_THREADS), 1, 1])
    }

    fn profiling(&self) -> bool {
        self.inner.pending.lock().unwrap().profile.is_some()
    }

    /// `label` is the profile key; it defaults to the kernel name.
    fn launch_labeled(&self, name: &str, push: &[u8], grid: [u32; 3], label: &str) -> Result<()> {
        let mut pending = self.inner.pending.lock().unwrap();
        let kernel = self
            .kernels
            .get(name)
            .ok_or_else(|| Error::Backend(format!("vulkan: no kernel {name}")))?;
        self.begin(&mut pending)?;
        let started = pending.profile.is_some().then(std::time::Instant::now);
        let d = self.dev();
        // SAFETY: the pending lock is held and begin() opened `cmd`. `kernel.pipeline` is a
        // compute pipeline built with `self.layout`. `push` is a Pc* block of at most 64 bytes,
        // a multiple of 4, inside the 128-byte range (not checked here). Its addresses belong to
        // buffers the caller borrowed and shape-checked, whose destruction is deferred until
        // this command completes. The grid is clamped to >= 1 but not checked against
        // maxComputeWorkGroupCount.
        unsafe {
            d.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::COMPUTE, kernel.pipeline);
            d.cmd_push_constants(
                self.cmd,
                self.layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                push,
            );
            d.cmd_dispatch(self.cmd, grid[0].max(1), grid[1].max(1), grid[2].max(1));
        }
        self.barrier();
        self.profile_op(&mut pending, label, started)
    }

    fn profile_op(
        &self,
        pending: &mut Pending,
        name: &str,
        started: Option<std::time::Instant>,
    ) -> Result<()> {
        if let Some(started) = started {
            self.flush(pending)?;
            let e = pending
                .profile
                .as_mut()
                .unwrap()
                .entry(name.to_string())
                .or_insert((0, std::time::Duration::ZERO));
            e.0 += 1;
            e.1 += started.elapsed();
        }
        Ok(())
    }
}

/// Push-constant blocks mirror the GLSL `PC` layouts (std430: u64 first,
/// then 4-byte scalars).
fn push<T: Copy>(v: &T) -> Vec<u8> {
    let n = size_of::<T>();
    let mut out = vec![0u8; n];
    // SAFETY: T is a plain #[repr(C)] struct of integers/floats; its bytes are read once.
    // Caveat: T is not bounded to padding-free types, and PcElem, PcRope and PcGemv carry
    // 4 trailing padding bytes that are copied into `out` uninitialized.
    unsafe { std::ptr::copy_nonoverlapping((v as *const T).cast::<u8>(), out.as_mut_ptr(), n) };
    out
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcElem {
    a: u64,
    b: u64,
    y: u64,
    n: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcNorm {
    w: u64,
    x: u64,
    y: u64,
    dim: u32,
    eps: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcRope {
    q: u64,
    k: u64,
    pos: i32,
    head_dim: i32,
    n_q: i32,
    n_kv: i32,
    theta: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcGemv {
    w: u64,
    x: u64,
    y: u64,
    n_out: u32,
    n_in: u32,
    n_tokens: u32,
    dtype: u32,
    block_bytes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcEmbed {
    table: u64,
    ids: u64,
    o: u64,
    n_embd: u32,
    n_tokens: u32,
    dtype: u32,
    block_bytes: u32,
}

impl Backend for VulkanBackend {
    type Buf = VkBuf;

    fn execution_identity(&self) -> Option<Arc<()>> {
        Some(self.identity.clone())
    }

    fn join_execution(&self) -> Result<()> {
        let result = self.sync();
        if result.is_err() {
            // Failed submission is never reset/reused. Join any submitted work
            // before request buffers are released; retain graveyard charges
            // when the driver cannot establish completion.
            // SAFETY: vkDeviceWaitIdle needs every queue externally synchronized. A failed sync()
            // means the submission is marked failed (sticky until Drop) or the lock is poisoned,
            // so every other path errors or panics before it can reach queue_submit.
            let _ = unsafe { self.dev().device_wait_idle() };
        }
        self.health.check(result)
    }

    fn device_label(&self) -> Option<String> {
        // SAFETY: `inner.pdev` was enumerated from `inner.instance`, both live while `self` is.
        let properties = unsafe {
            self.inner
                .instance
                .get_physical_device_properties(self.inner.pdev)
        };
        // SAFETY: deviceName is NUL-terminated within its fixed array per the spec, and
        // `properties` outlives the copy made by into_owned().
        Some(
            unsafe { std::ffi::CStr::from_ptr(properties.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn unified_memory(&self) -> bool {
        // SAFETY: `inner.pdev` was enumerated from `inner.instance`, both live while `self` is.
        matches!(
            unsafe {
                self.inner
                    .instance
                    .get_physical_device_properties(self.inner.pdev)
            }
            .device_type,
            vk::PhysicalDeviceType::INTEGRATED_GPU | vk::PhysicalDeviceType::CPU
        )
    }

    fn total_device_bytes(&self) -> Option<usize> {
        let heap = self.inner.mem.memory_types[self.device_type as usize].heap_index as usize;
        usize::try_from(self.inner.mem.memory_heaps[heap].size).ok()
    }

    fn available_device_bytes(&self) -> Result<Option<usize>> {
        if !self.inner.memory_budget {
            return Ok(None);
        }
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut properties = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        // SAFETY: `inner.pdev` belongs to the live instance; `budget` is chained only when
        // `memory_budget` is set (VK_EXT_memory_budget supported by `pdev` and enabled), and
        // outlives the call.
        unsafe {
            self.inner
                .instance
                .get_physical_device_memory_properties2(self.inner.pdev, &mut properties)
        };
        let heap = self.inner.mem.memory_types[self.device_type as usize].heap_index as usize;
        let free = budget.heap_budget[heap].saturating_sub(budget.heap_usage[heap]);
        Ok(Some(
            usize::try_from(free).map_err(|_| Error::SizeOverflow)?,
        ))
    }

    fn supports(&self, op: Operation) -> bool {
        matches!(
            op,
            Operation::Scale
                | Operation::GeluTanhMul
                | Operation::Softcap
                | Operation::RopeSplitHalf
                | Operation::WindowedAttention
        )
    }

    fn supports_weight_dtype(&self, dtype: crate::gguf::GgmlDType) -> bool {
        dtype_code::decoder_code(dtype).is_ok()
    }

    fn scale(&self, x: &mut VkBuf, factor: f32) -> Result<()> {
        if !factor.is_finite() {
            return Err(Error::ShapeMismatch("scale: factor must be finite".into()));
        }
        self.unary(x, factor, UNARY_SCALE)
    }

    fn softcap(&self, x: &mut VkBuf, cap: f32) -> Result<()> {
        shape::softcap(cap)?;
        self.unary(x, cap, UNARY_SOFTCAP)
    }

    fn gelu_tanh_mul(&self, a: &VkBuf, b: &VkBuf, y: &mut VkBuf) -> Result<()> {
        self.launch_binary_f32("gelu_tanh_mul", a, b, y)
    }

    fn rope_split_half(
        &self,
        x: &mut VkBuf,
        pos: u32,
        spec: &RopeSpec,
        factors: &VkBuf,
    ) -> Result<()> {
        self.split_rope(x, pos, spec, factors)
    }

    fn attn_windowed(
        &self,
        q: &VkBuf,
        kv: &KvView<VkBuf>,
        spec: &AttnSpec,
        scale: f32,
        window: usize,
        o: &mut VkBuf,
    ) -> Result<()> {
        self.attention(q, kv, spec, scale, window, o)
    }

    fn set_buffer_budget(&mut self, budget: crate::memory::BufferBudget) -> Result<()> {
        self.buffers.replace(budget)
    }

    fn buffer_budget(&self) -> Option<&crate::memory::BufferBudget> {
        Some(&self.buffers)
    }

    fn max_head_dim(&self) -> Option<usize> {
        Some(MAX_HEAD_DIM)
    }

    fn alloc(&self, bytes: usize, _pool: Pool) -> Result<Self::Buf> {
        let buf = self.buffer(bytes, false)?;
        if buf.host_visible() {
            // SAFETY: `buf` was just created and no command references it; if mapped, the mapping
            // spans the whole allocation, at least `bytes` long. (buffer(_, false) never maps, so
            // this branch is currently unreachable.)
            unsafe { std::ptr::write_bytes(buf.ptr, 0, bytes) };
        } else {
            let mut pending = self.inner.pending.lock().unwrap();
            self.begin(&mut pending)?;
            // SAFETY: the pending lock is held and begin() opened `cmd`; `buf` is bound, has
            // TRANSFER_DST usage, and offset 0 with WHOLE_SIZE meets the 4-byte alignment rules.
            unsafe {
                self.dev()
                    .cmd_fill_buffer(self.cmd, buf.buffer, 0, vk::WHOLE_SIZE, 0)
            };
            self.barrier();
        }
        Ok(buf)
    }

    fn upload(&self, dst: &mut Self::Buf, offset: usize, src: &[u8]) -> Result<()> {
        shape::range("upload", offset, src.len(), dst.len)?;
        if src.is_empty() {
            return Ok(());
        }
        if dst.host_visible() {
            self.sync()?;
            // SAFETY: shape::range above bounds offset + src.len() by dst.len, which the mapping
            // covers. sync() completed every recorded command and `&mut dst` keeps new ones from
            // naming it; `src` cannot alias the mapping through safe code.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.ptr.add(offset), src.len()) };
            return Ok(());
        }
        // Through staging; flushed so the staging buffer is reusable.
        let mut pending = self.inner.pending.lock().unwrap();
        let staging = self.take_staging(&mut pending, src.len())?;
        // SAFETY: take_staging returned a mapped buffer with len >= src.len(); it is either
        // fresh or the kept one, which returns to `pending` only after a flush, so no command
        // references it.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), staging.ptr, src.len()) };
        self.record_copy(&mut pending, &staging, 0, dst, offset, src.len())?;
        self.flush(&mut pending)?;
        self.keep_staging(&mut pending, staging);
        Ok(())
    }

    fn download(&self, src: &Self::Buf, offset: usize, dst: &mut [u8]) -> Result<()> {
        shape::range("download", offset, dst.len(), src.len)?;
        if dst.is_empty() {
            return Ok(());
        }
        if src.host_visible() {
            self.sync()?;
            // SAFETY: shape::range above bounds offset + dst.len() by src.len, which the mapping
            // covers. sync() waited for recorded work and its HOST_READ barrier made device writes
            // visible; `&src` rules out a concurrent kernel writing it (writes need `&mut`).
            unsafe {
                std::ptr::copy_nonoverlapping(src.ptr.add(offset), dst.as_mut_ptr(), dst.len())
            };
            return Ok(());
        }
        let mut pending = self.inner.pending.lock().unwrap();
        let staging = self.take_staging(&mut pending, dst.len())?;
        self.record_copy(&mut pending, src, offset, &staging, 0, dst.len())?;
        self.flush(&mut pending)?;
        // SAFETY: flush waited on the fence and its MEMORY_WRITE -> HOST_READ barrier made the
        // copy visible; the coherent staging mapping holds at least dst.len() bytes.
        unsafe { std::ptr::copy_nonoverlapping(staging.ptr, dst.as_mut_ptr(), dst.len()) };
        self.keep_staging(&mut pending, staging);
        Ok(())
    }

    fn copy(
        &self,
        src: &Self::Buf,
        src_off: usize,
        dst: &mut Self::Buf,
        dst_off: usize,
        len: usize,
    ) -> Result<()> {
        shape::range("copy", src_off, len, src.len)?;
        shape::range("copy", dst_off, len, dst.len)?;
        if len == 0 {
            return Ok(());
        }
        // On the queue, in order with the dispatches around it (the KV
        // write sits between rope and attention).
        let mut pending = self.inner.pending.lock().unwrap();
        let started = pending.profile.is_some().then(std::time::Instant::now);
        self.record_copy(&mut pending, src, src_off, dst, dst_off, len)?;
        self.profile_op(&mut pending, "copy", started)
    }

    fn qmatmul(&self, w: &QWeight<Self::Buf>, x: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        let shape::Matmul {
            n_out,
            n_in,
            n_tokens,
        } = shape::matmul(&w.shape, x.len / 4, y.len / 4)?;
        shape::kquant_aligned("qmatmul", w.dtype, n_in)?;
        let code = dtype_code::decoder_code(w.dtype)?;
        let int8 = self.int8.load(Ordering::Relaxed);
        if int8 != INT8_OFF && n_tokens > 1 && matches!(code, dtype_code::Q4K | dtype_code::Q6K) {
            return self.qmatmul_int8(
                w,
                x,
                y,
                shape::Matmul {
                    n_out,
                    n_in,
                    n_tokens,
                },
                code,
                int8 == INT8_DOT,
            );
        }
        let pc = PcGemv {
            w: w.buf.addr,
            x: x.addr,
            y: y.addr,
            n_out: n_out as u32,
            n_in: n_in as u32,
            n_tokens: n_tokens as u32,
            dtype: code as u32,
            block_bytes: dtype_code::block_bytes(w.dtype) as u32,
        };
        // Q4_K/Q6_K: one 64-lane workgroup per GEMV_NR rows for one token
        // (decode), per GEMM_BM x GEMM_BN output tile for several
        // (prefill). Float/Q5_K weights: one workgroup per (row, token).
        let (name, grid) = match (code, n_tokens) {
            (dtype_code::Q4K | dtype_code::Q6K, 1) => {
                ("gemv_q", [(n_out as u32).div_ceil(GEMV_NR), 1, 1])
            }
            (dtype_code::Q4K | dtype_code::Q6K, _) => {
                let name = if self.f16_weights() {
                    "gemm_q_f16"
                } else {
                    "gemm_q"
                };
                (
                    name,
                    [
                        (n_out as u32).div_ceil(GEMM_BM),
                        (n_tokens as u32).div_ceil(GEMM_BN),
                        1,
                    ],
                )
            }
            _ => ("gemv_f", [n_out as u32, n_tokens as u32, 1]),
        };
        if self.profiling() {
            let label = format!("{name} {:?} {n_out}x{n_in} T={n_tokens}", w.dtype);
            return self.launch_labeled(name, &push(&pc), grid, &label);
        }
        self.launch(name, &push(&pc), grid)
    }

    fn embed(
        &self,
        table: &QWeight<Self::Buf>,
        token_ids: &[u32],
        out: &mut Self::Buf,
    ) -> Result<()> {
        let shape::Embed { n_embd, .. } = shape::embed(&table.shape, token_ids, out.len / 4)?;
        shape::kquant_aligned("embed", table.dtype, n_embd)?;
        let code = dtype_code::decoder_code(table.dtype)?;
        let ids: Vec<i32> = token_ids.iter().map(|id| *id as i32).collect();
        // A fresh host-visible buffer nothing pending can reference:
        // written directly, no flush. Dropped at the end of this call, it
        // waits in the graveyard until the dispatch has run.
        let id_buf = self.buffer(ids.len() * 4, true)?;
        // SAFETY: `id_buf` was just created mapped with len ids.len() * 4, exactly the bytes
        // of `ids`; it is a separate allocation and nothing has recorded it yet.
        unsafe {
            std::ptr::copy_nonoverlapping(ids.as_ptr().cast::<u8>(), id_buf.ptr, ids.len() * 4)
        };
        let pc = PcEmbed {
            table: table.buf.addr,
            ids: id_buf.addr,
            o: out.addr,
            n_embd: n_embd as u32,
            n_tokens: token_ids.len() as u32,
            dtype: code as u32,
            block_bytes: dtype_code::block_bytes(table.dtype) as u32,
        };
        self.launch(
            "embed",
            &push(&pc),
            [
                token_ids.len() as u32,
                (n_embd as u32).div_ceil(EMBED_DIM_THREADS),
                1,
            ],
        )
    }

    fn rmsnorm(&self, w: &Self::Buf, x: &Self::Buf, y: &mut Self::Buf, eps: f32) -> Result<()> {
        let dim = w.len / 4;
        let n_tokens = shape::rows("rmsnorm", dim, x.len / 4, y.len / 4)?;
        let pc = PcNorm {
            w: w.addr,
            x: x.addr,
            y: y.addr,
            dim: dim as u32,
            eps,
        };
        self.launch("rmsnorm", &push(&pc), [n_tokens as u32, 1, 1])
    }

    fn add_rmsnorm(
        &self,
        w: &Self::Buf,
        a: &Self::Buf,
        b: &Self::Buf,
        sum: &mut Self::Buf,
        y: &mut Self::Buf,
        eps: f32,
    ) -> Result<()> {
        self.residual_norm(w, a, b, sum, y, eps)
    }

    fn rope(
        &self,
        q: &mut Self::Buf,
        k: &mut Self::Buf,
        pos: u32,
        n_tokens: usize,
        spec: &RopeSpec,
    ) -> Result<()> {
        let shape::Rope {
            n_q_heads,
            n_kv_heads,
        } = shape::rope(spec, q.len / 4, k.len / 4, n_tokens)?;
        let (n_q, n_kv) = (n_q_heads as i32, n_kv_heads as i32);
        let pc = PcRope {
            q: q.addr,
            k: k.addr,
            pos: pos as i32,
            head_dim: spec.head_dim as i32,
            n_q,
            n_kv,
            theta: spec.theta,
        };
        let pairs = (spec.head_dim / 2) as u32;
        self.launch(
            "rope",
            &push(&pc),
            [
                pairs.div_ceil(ROPE_PAIR_THREADS),
                (n_q + n_kv) as u32,
                n_tokens as u32,
            ],
        )
    }

    fn add(&self, a: &Self::Buf, b: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        self.launch_binary_f32("add", a, b, y)
    }

    fn attn(
        &self,
        q: &Self::Buf,
        kv: &KvView<Self::Buf>,
        spec: &AttnSpec,
        o: &mut Self::Buf,
    ) -> Result<()> {
        self.attention(q, kv, spec, 1.0 / (spec.head_dim as f32).sqrt(), 0, o)
    }

    fn silu_mul(&self, gate: &Self::Buf, up: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        self.launch_binary_f32("silu_mul", gate, up, y)
    }
}

impl VkBuf {
    /// Raw mapped pointer; null unless the GPU has no memory type the
    /// host cannot see (then everything is mapped, and a caller on such
    /// an APU can read it after the backend has synced).
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr.cast()
    }
}

#[cfg(test)]
mod tests;
