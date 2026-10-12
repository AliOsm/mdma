use super::{Result, hip_err};
use crate::memory::{BufferBudget, BufferCharge, MemoryDomain, product};
use rocm_rs::hip::{DeviceMemory as HipMemory, ffi};
use std::ffi::c_void;
use std::mem::ManuallyDrop;

/// rocm-rs ignores hipFree's status in Drop. Own that step so a failed
/// free cannot refund memory which may still be allocated.
pub(super) struct DeviceMemory<T> {
    mem: ManuallyDrop<HipMemory<T>>,
    charge: BufferCharge,
}

impl<T> DeviceMemory<T> {
    pub(super) fn new(count: usize, budget: &BufferBudget) -> Result<Self> {
        let bytes = product(&[count, size_of::<T>()])?;
        let (mem, charge) = budget.allocate(MemoryDomain::Device, bytes, || {
            HipMemory::new(count).map_err(hip_err)
        })?;
        Ok(Self {
            mem: ManuallyDrop::new(mem),
            charge,
        })
    }

    pub(super) fn as_ptr(&self) -> *mut c_void {
        self.mem.as_ptr()
    }

    pub(super) fn memset(&mut self, value: i32) -> Result<()> {
        self.mem.memset(value).map_err(hip_err)
    }
}

impl<T> Drop for DeviceMemory<T> {
    fn drop(&mut self) {
        let ptr = self.mem.as_ptr();
        self.charge.release(|| {
            if ptr.is_null() {
                return true;
            }
            // SAFETY: `ptr` is the non-null hipMalloc pointer of the inner HipMemory, which
            // sits in ManuallyDrop and is never dropped, so this is the only hipFree of the
            // allocation; Drop runs once and `release` invokes this closure at most once.
            // hipFree implicitly synchronizes the device, so no in-flight kernel or async
            // copy can still be using the buffer.
            let status = unsafe { ffi::hipFree(ptr) };
            if status != ffi::hipError_t_hipSuccess {
                eprintln!("mdma: hipFree failed ({status}); buffer remains charged");
                return false;
            }
            true
        });
    }
}
