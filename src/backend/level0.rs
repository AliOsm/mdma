// Intel Xe Level Zero backend. OpenCL C in kernels/kernels.cl is compiled
// by ocloc to a native GPU binary and launched through libze_loader.
// The iGPU shares DDR with the host, so buffers are zeMemAllocShared and
// upload/download are memcpy.

use super::{AttnSpec, Backend, Error, KvView, Pool, QWeight, Result, RopeSpec};
use super::dtype_code::{F32 as DT_F32, F16 as DT_F16, block_bytes, code as dtype_code};
use super::shape;
use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;

const ZE_RESULT_SUCCESS: u32 = 0;
const ZE_DEVICE_TYPE_GPU: i32 = 1;
const ZE_DEVICE_PROPERTY_FLAG_INTEGRATED: u32 = 1;
const ZE_COMMAND_QUEUE_MODE_SYNCHRONOUS: i32 = 1;
const ZE_MODULE_FORMAT_NATIVE: i32 = 1;
const ZE_STRUCTURE_TYPE_DEVICE_PROPERTIES: i32 = 0x3;
const ZE_STRUCTURE_TYPE_CONTEXT_DESC: i32 = 0xd;
const ZE_STRUCTURE_TYPE_COMMAND_QUEUE_DESC: i32 = 0xe;
const ZE_STRUCTURE_TYPE_COMMAND_LIST_DESC: i32 = 0xf;
const ZE_STRUCTURE_TYPE_DEVICE_MEM_ALLOC_DESC: i32 = 0x15;
const ZE_STRUCTURE_TYPE_HOST_MEM_ALLOC_DESC: i32 = 0x16;
const ZE_STRUCTURE_TYPE_MODULE_DESC: i32 = 0x1b;
const ZE_STRUCTURE_TYPE_KERNEL_DESC: i32 = 0x1d;

type ZeResult = u32;
type ZeDriver = *mut c_void;
type ZeDevice = *mut c_void;
type ZeContext = *mut c_void;
type ZeQueue = *mut c_void;
type ZeList = *mut c_void;
type ZeModule = *mut c_void;
type ZeKernel = *mut c_void;

#[repr(C)]
struct ZeDeviceProps {
    stype: i32,
    p_next: *mut c_void,
    type_: i32,
    vendor_id: u32,
    device_id: u32,
    _flags: u32,
    _subdevice_id: u32,
    core_clock: u32,
    _max_mem_alloc: u64,
    _max_hw_contexts: u32,
    _max_cmd_queue_priority: u32,
    _num_threads_per_eu: u32,
    _physical_eu_simd_width: u32,
    _num_eus_per_subslice: u32,
    _num_subslices_per_slice: u32,
    _num_slices: u32,
    _timer_resolution: u64,
    _timestamp_valid_bits: u32,
    _kernel_timestamp_valid_bits: u32,
    _uuid: [u8; 16],
    name: [u8; 256],
}

#[repr(C)]
struct ZeCtxDesc {
    stype: i32,
    p_next: *const c_void,
    flags: u32,
}
#[repr(C)]
struct ZeQueueDesc {
    stype: i32,
    p_next: *const c_void,
    ordinal: u32,
    index: u32,
    flags: u32,
    mode: i32,
    priority: i32,
}
#[repr(C)]
struct ZeListDesc {
    stype: i32,
    p_next: *const c_void,
    command_queue_group_ordinal: u32,
    flags: u32,
}
#[repr(C)]
struct ZeModuleDesc {
    stype: i32,
    p_next: *const c_void,
    format: i32,
    input_size: usize,
    p_input: *const u8,
    p_build_flags: *const c_char,
    p_constants: *const c_void,
}
#[repr(C)]
struct ZeKernelDesc {
    stype: i32,
    p_next: *const c_void,
    flags: u32,
    p_kernel_name: *const c_char,
}
#[repr(C)]
struct ZeGroupCount {
    x: u32,
    y: u32,
    z: u32,
}
#[repr(C)]
struct ZeDevAllocDesc {
    stype: i32,
    p_next: *const c_void,
    flags: u32,
    ordinal: u32,
}
#[repr(C)]
struct ZeHostAllocDesc {
    stype: i32,
    p_next: *const c_void,
    flags: u32,
}

// SAFETY: these declarations transcribe ze_api.h (Level Zero 1.x) as exported by
// libze_loader: handles are opaque pointers, ze_result_t and the flag typedefs are 32-bit,
// and the `*const` list/wait parameters match C pointers the driver only reads. The
// #[repr(C)] structs above mirror ze_device_properties_t and the ze_*_desc_t layouts.
// Each call site must still uphold that function's handle and pointer contract.
unsafe extern "C" {
    fn zeInit(flags: u32) -> ZeResult;
    fn zeDriverGet(count: *mut u32, drivers: *mut ZeDriver) -> ZeResult;
    fn zeDeviceGet(driver: ZeDriver, count: *mut u32, devices: *mut ZeDevice) -> ZeResult;
    fn zeDeviceGetProperties(device: ZeDevice, props: *mut ZeDeviceProps) -> ZeResult;
    fn zeContextCreate(driver: ZeDriver, desc: *const ZeCtxDesc, ctx: *mut ZeContext) -> ZeResult;
    fn zeContextDestroy(ctx: ZeContext) -> ZeResult;
    fn zeCommandQueueCreate(ctx: ZeContext, dev: ZeDevice, desc: *const ZeQueueDesc, out: *mut ZeQueue) -> ZeResult;
    fn zeCommandQueueDestroy(queue: ZeQueue) -> ZeResult;
    fn zeCommandQueueExecuteCommandLists(queue: ZeQueue, n: u32, lists: *const ZeList, fence: *mut c_void) -> ZeResult;
    fn zeCommandQueueSynchronize(queue: ZeQueue, timeout: u64) -> ZeResult;
    fn zeCommandListCreate(ctx: ZeContext, dev: ZeDevice, desc: *const ZeListDesc, out: *mut ZeList) -> ZeResult;
    fn zeCommandListDestroy(list: ZeList) -> ZeResult;
    fn zeCommandListClose(list: ZeList) -> ZeResult;
    fn zeCommandListReset(list: ZeList) -> ZeResult;
    fn zeCommandListAppendLaunchKernel(list: ZeList, kernel: ZeKernel, args: *const ZeGroupCount, signal: *mut c_void, n_wait: u32, waits: *const c_void) -> ZeResult;
    fn zeModuleCreate(ctx: ZeContext, dev: ZeDevice, desc: *const ZeModuleDesc, module: *mut ZeModule, log: *mut *mut c_void) -> ZeResult;
    fn zeModuleDestroy(module: ZeModule) -> ZeResult;
    fn zeKernelCreate(module: ZeModule, desc: *const ZeKernelDesc, kernel: *mut ZeKernel) -> ZeResult;
    fn zeKernelDestroy(kernel: ZeKernel) -> ZeResult;
    fn zeKernelSetGroupSize(kernel: ZeKernel, x: u32, y: u32, z: u32) -> ZeResult;
    fn zeKernelSetArgumentValue(kernel: ZeKernel, index: u32, size: usize, value: *const c_void) -> ZeResult;
    fn zeMemAllocShared(ctx: ZeContext, dev_desc: *const ZeDevAllocDesc, host_desc: *const ZeHostAllocDesc, size: usize, align: usize, dev: ZeDevice, out: *mut *mut c_void) -> ZeResult;
    fn zeMemFree(ctx: ZeContext, ptr: *mut c_void) -> ZeResult;
}

fn ze(what: &str, r: ZeResult) -> Result<()> {
    if r == ZE_RESULT_SUCCESS { Ok(()) } else { Err(Error::Backend(format!("{what} failed: {r:#x}"))) }
}

pub struct L0Buf {
    ctx: ZeContext,
    ptr: *mut c_void,
    len: usize,
}

// SAFETY: an L0Buf solely owns one shared-USM allocation (freed once in Drop) plus a copy of its context handle.
// Level Zero handles may be used from any thread and zeMemFree is thread-safe for distinct pointers. Host writes
// need `&mut L0Buf` while `&L0Buf` only allows reads (download, kernel inputs), so sharing cannot race, and kernel
// accesses finish before launch() returns. Not enforced: the allocating Level0Backend (and its context) must
// outlive the buffer.
unsafe impl Send for L0Buf {}
unsafe impl Sync for L0Buf {}

impl Drop for L0Buf {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr is the non-null allocation zeMemAllocShared returned for self.ctx
            // in shared(), owned solely by this L0Buf, so it is freed exactly once; launch()
            // synchronizes the queue before returning, so no kernel still references it.
            // Requires self.ctx to still be alive, i.e. the allocating Level0Backend must not
            // have been dropped yet; nothing in the types enforces that order.
            unsafe { zeMemFree(self.ctx, self.ptr) };
        }
    }
}

pub struct Level0Backend {
    device_name: String,
    unified_memory: bool,
    ctx: ZeContext,
    device: ZeDevice,
    queue: ZeQueue,
    list: ZeList,
    module: ZeModule,
    lock: Mutex<()>,
}

// SAFETY: the context, device, queue, list and module handles are driver objects the Level
// Zero spec lets any thread use. The calls it requires to be externally serialized
// (command-list append/close/reset, queue execute/synchronize, per-kernel argument setup)
// all happen in launch() under `lock`; zeMemAllocShared is documented safe to call
// concurrently, and the host memcpys touch only USM guarded by Rust borrows of the L0Buf.
unsafe impl Send for Level0Backend {}
unsafe impl Sync for Level0Backend {}

impl Drop for Level0Backend {
    fn drop(&mut self) {
        // SAFETY: Self is only built after every create in new() succeeded, so all four
        // handles are valid and owned solely by this backend; Drop runs once. No work is
        // pending (launch() synchronizes before returning) and per-launch kernels are already
        // destroyed, so children go first (list, queue, module) and the context last, as the
        // spec requires. Results are ignored because Drop cannot report them. Not enforced:
        // L0Bufs allocated from `ctx` may still be alive and will free into it later.
        unsafe {
            zeCommandListDestroy(self.list);
            zeCommandQueueDestroy(self.queue);
            zeModuleDestroy(self.module);
            zeContextDestroy(self.ctx);
        }
    }
}

macro_rules! args {
    ($($v:ident),* $(,)?) => {{
        let ptrs: Vec<*const c_void> = vec![$(std::ptr::from_ref(&$v) as *const c_void),*];
        let sizes: Vec<usize> = vec![$(std::mem::size_of_val(&$v)),*];
        (ptrs, sizes)
    }};
}

impl Level0Backend {
    pub fn new() -> Result<Self> {
        // SAFETY: zeInit precedes every other call. Enumeration is count-then-fill, so the second call writes at most
        // the vec's length (relies on counts not shrinking between calls; the vecs are not truncated). `props` is a
        // zeroed ZeDeviceProps (integers, byte arrays, one raw pointer: zero is valid) with its stype and null pNext.
        // Each descriptor is a live, correctly tagged local; `bin` and the build-flags C string are 'static; the log
        // out-param is optional (null). Handles are used only after ze()? confirms their create; `device` belongs to
        // `driver`, the context's driver. `name` is NUL-terminated per spec; zeroed init covers shorter writes.
        unsafe {
            ze("zeInit", zeInit(0))?;
            let mut n_drivers = 0u32;
            ze("zeDriverGet", zeDriverGet(&mut n_drivers, std::ptr::null_mut()))?;
            let mut drivers = vec![std::ptr::null_mut(); n_drivers as usize];
            ze("zeDriverGet", zeDriverGet(&mut n_drivers, drivers.as_mut_ptr()))?;
            let mut found: Option<(ZeDriver, ZeDevice, ZeDeviceProps)> = None;
            for driver in drivers {
                let mut n = 0u32;
                if zeDeviceGet(driver, &mut n, std::ptr::null_mut()) != ZE_RESULT_SUCCESS {
                    continue;
                }
                let mut devs = vec![std::ptr::null_mut(); n as usize];
                if zeDeviceGet(driver, &mut n, devs.as_mut_ptr()) != ZE_RESULT_SUCCESS {
                    continue;
                }
                for dev in devs {
                    let mut props: ZeDeviceProps = std::mem::zeroed();
                    props.stype = ZE_STRUCTURE_TYPE_DEVICE_PROPERTIES;
                    if zeDeviceGetProperties(dev, &mut props) == ZE_RESULT_SUCCESS && props.type_ == ZE_DEVICE_TYPE_GPU {
                        found = Some((driver, dev, props));
                    }
                }
            }
            let (driver, device, properties) = found.ok_or_else(|| Error::Backend("no Level Zero GPU".into()))?;
            let mut ctx = std::ptr::null_mut();
            let cd = ZeCtxDesc { stype: ZE_STRUCTURE_TYPE_CONTEXT_DESC, p_next: std::ptr::null(), flags: 0 };
            ze("zeContextCreate", zeContextCreate(driver, &cd, &mut ctx))?;
            let mut queue = std::ptr::null_mut();
            let qd = ZeQueueDesc {
                stype: ZE_STRUCTURE_TYPE_COMMAND_QUEUE_DESC,
                p_next: std::ptr::null(),
                ordinal: 0,
                index: 0,
                flags: 0,
                mode: ZE_COMMAND_QUEUE_MODE_SYNCHRONOUS,
                priority: 0,
            };
            ze("zeCommandQueueCreate", zeCommandQueueCreate(ctx, device, &qd, &mut queue))?;
            let mut list = std::ptr::null_mut();
            let ld = ZeListDesc { stype: ZE_STRUCTURE_TYPE_COMMAND_LIST_DESC, p_next: std::ptr::null(), command_queue_group_ordinal: 0, flags: 0 };
            ze("zeCommandListCreate", zeCommandListCreate(ctx, device, &ld, &mut list))?;
            let bin: &[u8] = include_bytes!(env!("MDMA_L0_ZEBIN"));
            let mut module = std::ptr::null_mut();
            let md = ZeModuleDesc {
                stype: ZE_STRUCTURE_TYPE_MODULE_DESC,
                p_next: std::ptr::null(),
                format: ZE_MODULE_FORMAT_NATIVE,
                input_size: bin.len(),
                p_input: bin.as_ptr(),
                p_build_flags: c"".as_ptr(),
                p_constants: std::ptr::null(),
            };
            ze("zeModuleCreate", zeModuleCreate(ctx, device, &md, &mut module, std::ptr::null_mut()))?;
            Ok(Self {
                device_name: std::ffi::CStr::from_ptr(properties.name.as_ptr().cast()).to_string_lossy().into_owned(),
                unified_memory: properties._flags & ZE_DEVICE_PROPERTY_FLAG_INTEGRATED != 0,
                ctx, device, queue, list, module, lock: Mutex::new(()),
            })
        }
    }

    fn shared(&self, bytes: usize) -> Result<L0Buf> {
        let alloc = bytes.max(64);
        let mut ptr = std::ptr::null_mut();
        let dd = ZeDevAllocDesc { stype: ZE_STRUCTURE_TYPE_DEVICE_MEM_ALLOC_DESC, p_next: std::ptr::null(), flags: 0, ordinal: 0 };
        let hd = ZeHostAllocDesc { stype: ZE_STRUCTURE_TYPE_HOST_MEM_ALLOC_DESC, p_next: std::ptr::null(), flags: 0 };
        // SAFETY: dd/hd are live, correctly tagged descriptors with null pNext; the size is at
        // least 64 and the 64-byte alignment is a power of two; ctx/device are this backend's
        // live handles, and zeMemAllocShared may be called concurrently. `ptr` is wrapped in
        // an L0Buf only after ze()? confirms the allocation.
        unsafe { ze("zeMemAllocShared", zeMemAllocShared(self.ctx, &dd, &hd, alloc, 64, self.device, &mut ptr))? };
        Ok(L0Buf { ctx: self.ctx, ptr, len: bytes })
    }

    fn launch_binary_f32(&self, op: &str, kernel: &str, a: &L0Buf, b: &L0Buf, y: &mut L0Buf) -> Result<()> {
        let n = a.len / 4;
        if b.len != a.len || y.len != a.len {
            return Err(Error::ShapeMismatch(format!("{op}: lengths {} {} {}", a.len, b.len, y.len)));
        }
        let a_ptr = a.ptr;
        let b_ptr = b.ptr;
        let y_ptr = y.ptr;
        let n_i = n as i32;
        let (ptrs, sizes) = args!(a_ptr, b_ptr, y_ptr, n_i);
        let group = 64u32.min(n.max(1) as u32);
        self.launch(kernel, [group, 1, 1], [(n as u32).div_ceil(group).max(1), 1, 1], &ptrs, &sizes)
    }

    fn launch(&self, name: &str, group: [u32; 3], grid: [u32; 3], ptrs: &[*const c_void], sizes: &[usize]) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let c_name = CString::new(name).map_err(|e| Error::Backend(e.to_string()))?;
        let desc = ZeKernelDesc { stype: ZE_STRUCTURE_TYPE_KERNEL_DESC, p_next: std::ptr::null(), flags: 0, p_kernel_name: c_name.as_ptr() };
        let mut kernel = std::ptr::null_mut();
        // SAFETY: self.module is live for &self; desc points at c_name, which outlives the
        // call; `kernel` is an out-param used only after ze()? confirms creation, and it stays
        // private to this call (the launch lock is held).
        unsafe { ze("zeKernelCreate", zeKernelCreate(self.module, &desc, &mut kernel))? };
        let fail = |r: ZeResult, what: &str| -> Result<()> {
            // SAFETY: `fail` only runs after zeKernelCreate succeeded, and each caller returns
            // right after it, so the kernel is destroyed once and never also on the success
            // path. NOTE: the list is not reset here, so a failed zeCommandListClose can leave
            // an append of this destroyed kernel in the list for the next launch() to execute.
            unsafe { zeKernelDestroy(kernel) };
            ze(what, r)
        };
        for (i, (p, s)) in ptrs.iter().zip(sizes).enumerate() {
            // SAFETY: args! pairs a pointer to each named local with that local's
            // size_of_val, and those locals live in the caller's frame for all of launch()
            // (past execute and synchronize), so the value is readable whenever the driver
            // captures it. Index order and widths match the OpenCL signatures in
            // level0/kernels/kernels.cl (8-byte __global pointers, i32 int, f32 float); a size
            // mismatch is rejected with an error. `kernel` is private to this thread.
            let r = unsafe { zeKernelSetArgumentValue(kernel, i as u32, *s, *p) };
            if r != ZE_RESULT_SUCCESS {
                return fail(r, "zeKernelSetArgumentValue");
            }
        }
        // SAFETY: `kernel` is live and private to this call; the arguments are plain values,
        // and the driver rejects unsupported group sizes with an error code.
        let r = unsafe { zeKernelSetGroupSize(kernel, group[0], group[1], group[2]) };
        if r != ZE_RESULT_SUCCESS {
            return fail(r, "zeKernelSetGroupSize");
        }
        let count = ZeGroupCount { x: grid[0].max(1), y: grid[1].max(1), z: grid[2].max(1) };
        // SAFETY: self.list is only touched under `lock` (command lists are not
        // free-threaded); `kernel` is live with every argument set; `count` is read during
        // the call; there are no signal or wait events (null with a count of 0).
        let r = unsafe { zeCommandListAppendLaunchKernel(self.list, kernel, &count, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r != ZE_RESULT_SUCCESS {
            return fail(r, "zeCommandListAppendLaunchKernel");
        }
        // SAFETY: self.list is live and only touched under `lock`; this closes the single
        // append recorded above.
        let r = unsafe { zeCommandListClose(self.list) };
        if r != ZE_RESULT_SUCCESS {
            return fail(r, "zeCommandListClose");
        }
        // SAFETY: under `lock`, executes exactly the one closed list (&self.list is a valid one-element array) on this
        // queue with no fence, then blocks with an infinite timeout, so the kernel is done with every buffer argument
        // before launch() returns and the callers' borrows end. Kernel indexing is only as safe as each caller's size
        // checks: attn() does not check kv.k/kv.v lengths or GQA divisibility, and qmatmul()/embed() do not check
        // w.buf/table.buf against shape.
        let r = unsafe { zeCommandQueueExecuteCommandLists(self.queue, 1, &self.list, std::ptr::null_mut()) };
        let r2 = unsafe { zeCommandQueueSynchronize(self.queue, u64::MAX) };
        // SAFETY: runs after the synchronize above, so the device no longer references the
        // list or the kernel: the list returns to recording state and the kernel created in
        // this call is destroyed once (`fail` is not reached on this path). If execute
        // succeeded but the synchronize failed (device lost), work may still be in flight.
        unsafe {
            zeCommandListReset(self.list);
            zeKernelDestroy(kernel);
        }
        ze("zeCommandQueueExecuteCommandLists", r)?;
        ze("zeCommandQueueSynchronize", r2)?;
        Ok(())
    }
}

impl Backend for Level0Backend {
    type Buf = L0Buf;

    fn device_label(&self) -> Option<String> { Some(self.device_name.clone()) }
    fn unified_memory(&self) -> bool { self.unified_memory }

    fn alloc(&self, bytes: usize, _pool: Pool) -> Result<Self::Buf> {
        let buf = self.shared(bytes)?;
        // SAFETY: buf.ptr is a fresh host-accessible shared-USM allocation of
        // bytes.max(64) >= bytes bytes, not yet visible to any kernel; write_bytes over c_void
        // writes exactly `bytes` bytes (size_of::<c_void>() == 1).
        unsafe { std::ptr::write_bytes(buf.ptr, 0, bytes) };
        Ok(buf)
    }

    fn upload(&self, dst: &mut Self::Buf, offset: usize, src: &[u8]) -> Result<()> {
        shape::range("level0 upload", offset, src.len(), dst.len)?;
        // SAFETY: shape::range proved offset + src.len() <= dst.len, so the destination range
        // lies inside dst's host-accessible shared-USM allocation. `&mut dst` rules out
        // concurrent kernel or host access, and no kernel is in flight (launch() synchronizes
        // before returning). `src` is Rust memory, disjoint from the USM allocation.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst.ptr.cast::<u8>().add(offset), src.len()) };
        Ok(())
    }

    fn download(&self, src: &Self::Buf, offset: usize, dst: &mut [u8]) -> Result<()> {
        shape::range("level0 download", offset, dst.len(), src.len)?;
        // SAFETY: shape::range proved offset + dst.len() <= src.len, so the source range lies
        // inside src's shared-USM allocation; `dst` is an exclusive host slice of dst.len()
        // bytes, disjoint from it. Kernel writes to src need `&mut src` and complete before
        // launch() returns, so no device write races this read.
        unsafe { std::ptr::copy_nonoverlapping(src.ptr.cast::<u8>().add(offset), dst.as_mut_ptr(), dst.len()) };
        Ok(())
    }

    fn copy(&self, src: &Self::Buf, src_off: usize, dst: &mut Self::Buf, dst_off: usize, len: usize) -> Result<()> {
        shape::range("level0 copy source", src_off, len, src.len)?;
        shape::range("level0 copy destination", dst_off, len, dst.len)?;
        // SAFETY: both shape::range checks keep each range inside its buffer's shared-USM
        // allocation; `src` and `&mut dst` are distinct L0Bufs, hence distinct allocations, so
        // the ranges cannot overlap, and no kernel is in flight.
        unsafe { std::ptr::copy_nonoverlapping(src.ptr.cast::<u8>().add(src_off), dst.ptr.cast::<u8>().add(dst_off), len) };
        Ok(())
    }

    fn qmatmul(&self, w: &QWeight<Self::Buf>, x: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        let &[n_out, n_in] = w.shape.as_slice() else {
            return Err(Error::ShapeMismatch(format!("qmatmul expects rank-2, got {:?}", w.shape)));
        };
        let code = dtype_code(w.dtype)?;
        let x_elems = x.len / 4;
        if n_in == 0 || x_elems % n_in != 0 {
            return Err(Error::ShapeMismatch(format!("qmatmul: x has {x_elems} elems, n_in {n_in}")));
        }
        let n_tokens = x_elems / n_in;
        if y.len / 4 != n_tokens * n_out {
            return Err(Error::ShapeMismatch(format!("qmatmul: y has {} elems, expected {}", y.len / 4, n_tokens * n_out)));
        }
        let w_ptr = w.buf.ptr;
        let x_ptr = x.ptr;
        let y_ptr = y.ptr;
        let n_out_i = n_out as i32;
        let n_in_i = n_in as i32;
        let n_tokens_i = n_tokens as i32;
        let code_i = code;
        if code == DT_F32 || code == DT_F16 {
            let (ptrs, sizes) = args!(w_ptr, x_ptr, y_ptr, n_out_i, n_in_i, n_tokens_i, code_i);
            let gx = (n_out as u32).div_ceil(8).max(1);
            self.launch("gemv_f", [8, 1, 1], [gx, n_tokens as u32, 1], &ptrs, &sizes)
        } else {
            let bb = block_bytes(w.dtype);
            if n_in % 256 != 0 {
                return Err(Error::ShapeMismatch(format!("qmatmul: K-quant n_in {n_in} is not a multiple of 256")));
            }
            let (ptrs, sizes) = args!(w_ptr, x_ptr, y_ptr, n_out_i, n_in_i, n_tokens_i, code_i, bb);
            let gx = (n_out as u32).div_ceil(4).max(1);
            self.launch("gemv_q", [4, 1, 1], [gx, n_tokens as u32, 1], &ptrs, &sizes)
        }
    }

    fn embed(&self, table: &QWeight<Self::Buf>, token_ids: &[u32], out: &mut Self::Buf) -> Result<()> {
        let &[vocab, n_embd] = table.shape.as_slice() else {
            return Err(Error::ShapeMismatch(format!("embed expects rank-2, got {:?}", table.shape)));
        };
        let code = dtype_code(table.dtype)?;
        if out.len / 4 != token_ids.len() * n_embd {
            return Err(Error::ShapeMismatch(format!("embed: out has {} elems, expected {}", out.len / 4, token_ids.len() * n_embd)));
        }
        for &id in token_ids {
            if id as usize >= vocab {
                return Err(Error::ShapeMismatch(format!("embed: token {id} out of range (vocab {vocab})")));
            }
        }
        let ids: Vec<i32> = token_ids.iter().map(|id| *id as i32).collect();
        let mut id_buf = self.shared(ids.len() * 4)?;
        // SAFETY: id_buf is a fresh shared allocation of max(ids.len() * 4, 64) bytes, 64-byte
        // aligned (>= align_of::<i32>()), so ids.len() i32s fit; `ids` is a separate Vec, so
        // the regions are disjoint, and no kernel has seen id_buf yet.
        unsafe { std::ptr::copy_nonoverlapping(ids.as_ptr(), id_buf.ptr.cast(), ids.len()) };
        let table_ptr = table.buf.ptr;
        let ids_ptr = id_buf.ptr;
        let out_ptr = out.ptr;
        let n_embd_i = n_embd as i32;
        let n_tokens_i = token_ids.len() as i32;
        let code_i = code;
        let bb = block_bytes(table.dtype);
        let (ptrs, sizes) = args!(table_ptr, ids_ptr, out_ptr, n_embd_i, n_tokens_i, code_i, bb);
        self.launch("embed_rows", [1, 64, 1], [token_ids.len().max(1) as u32, (n_embd as u32).div_ceil(64).max(1), 1], &ptrs, &sizes)?;
        drop(id_buf);
        Ok(())
    }

    fn rmsnorm(&self, w: &Self::Buf, x: &Self::Buf, y: &mut Self::Buf, eps: f32) -> Result<()> {
        let dim = w.len / 4;
        let x_elems = x.len / 4;
        if dim == 0 || x_elems % dim != 0 {
            return Err(Error::ShapeMismatch(format!("rmsnorm: x has {x_elems} elems, dim {dim}")));
        }
        if y.len != x.len {
            return Err(Error::ShapeMismatch(format!("rmsnorm: y has {} bytes, expected {}", y.len, x.len)));
        }
        let n_tokens = x_elems / dim;
        let w_ptr = w.ptr;
        let x_ptr = x.ptr;
        let y_ptr = y.ptr;
        let dim_i = dim as i32;
        let eps = eps;
        let (ptrs, sizes) = args!(w_ptr, x_ptr, y_ptr, dim_i, eps);
        self.launch("rmsnorm_f32", [1, 1, 1], [n_tokens as u32, 1, 1], &ptrs, &sizes)
    }

    fn rope(&self, q: &mut Self::Buf, k: &mut Self::Buf, pos: u32, n_tokens: usize, spec: &RopeSpec) -> Result<()> {
        let q_elems = q.len / 4;
        let k_elems = k.len / 4;
        if n_tokens == 0 || q_elems % n_tokens != 0 || k_elems % n_tokens != 0 {
            return Err(Error::ShapeMismatch(format!("rope: q {q_elems} k {k_elems} n_tokens {n_tokens}")));
        }
        let q_stride = q_elems / n_tokens;
        let k_stride = k_elems / n_tokens;
        if spec.head_dim == 0 || q_stride % spec.head_dim != 0 || k_stride % spec.head_dim != 0 {
            return Err(Error::ShapeMismatch("rope: stride is not a multiple of head_dim".into()));
        }
        let n_q = (q_stride / spec.head_dim) as i32;
        let n_kv = (k_stride / spec.head_dim) as i32;
        let q_ptr = q.ptr;
        let k_ptr = k.ptr;
        let pos_i = pos as i32;
        let head_dim = spec.head_dim as i32;
        let theta = spec.theta;
        let (ptrs, sizes) = args!(q_ptr, k_ptr, pos_i, head_dim, n_q, n_kv, theta);
        let pairs = (spec.head_dim / 2) as u32;
        self.launch("rope_f32", [pairs.max(1), 1, 1], [1, (n_q + n_kv) as u32, n_tokens as u32], &ptrs, &sizes)
    }


    fn add_rmsnorm(&self, w: &Self::Buf, a: &Self::Buf, b: &Self::Buf, sum: &mut Self::Buf, y: &mut Self::Buf, eps: f32) -> Result<()> {
        let dim = w.len / 4;
        if !eps.is_finite() || eps <= 0.0 || dim == 0 || a.len != b.len || a.len % (dim * 4) != 0 || sum.len < a.len || y.len < a.len {
            return Err(Error::ShapeMismatch(format!("add_rmsnorm: dim {dim}, lengths {} {} {} {}", a.len, b.len, sum.len, y.len)));
        }
        let n_tokens = a.len / 4 / dim;
        let w_ptr = w.ptr;
        let a_ptr = a.ptr;
        let b_ptr = b.ptr;
        let sum_ptr = sum.ptr;
        let y_ptr = y.ptr;
        let dim_i = dim as i32;
        let eps = eps;
        let (ptrs, sizes) = args!(w_ptr, a_ptr, b_ptr, sum_ptr, y_ptr, dim_i, eps);
        self.launch("add_rmsnorm_f32", [1, 1, 1], [n_tokens as u32, 1, 1], &ptrs, &sizes)
    }

    fn add(&self, a: &Self::Buf, b: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        self.launch_binary_f32("add", "add_f32", a, b, y)
    }

    fn attn(&self, q: &Self::Buf, kv: &KvView<Self::Buf>, spec: &AttnSpec, o: &mut Self::Buf) -> Result<()> {
        let per = spec.n_q_heads * spec.head_dim;
        let q_elems = q.len / 4;
        if per == 0 || q_elems % per != 0 {
            return Err(Error::ShapeMismatch(format!("attn: q has {q_elems} elems, per-token {per}")));
        }
        let n_q = q_elems / per;
        if n_q > kv.len {
            return Err(Error::ShapeMismatch(format!("attn: n_q {n_q} exceeds kv.len {}", kv.len)));
        }
        if o.len != q.len {
            return Err(Error::ShapeMismatch(format!("attn: o has {} bytes, expected {}", o.len, q.len)));
        }
        let prefix = (kv.len - n_q) as i32;
        let q_ptr = q.ptr;
        let k_ptr = kv.k.ptr;
        let v_ptr = kv.v.ptr;
        let o_ptr = o.ptr;
        let head_dim = spec.head_dim as i32;
        let n_q_heads = spec.n_q_heads as i32;
        let n_kv_heads = spec.n_kv_heads as i32;
        let n_q_i = n_q as i32;
        let (ptrs, sizes) = args!(q_ptr, k_ptr, v_ptr, o_ptr, head_dim, n_q_heads, n_kv_heads, n_q_i, prefix);
        self.launch("attn_f32", [1, 1, 1], [spec.n_q_heads as u32, n_q as u32, 1], &ptrs, &sizes)
    }

    fn silu_mul(&self, gate: &Self::Buf, up: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        self.launch_binary_f32("silu_mul", "silu_mul_f32", gate, up, y)
    }
}

#[cfg(test)]
mod tests;
