// NVML probe for the GPU's enforced power limit. The enforced limit is the
// minimum of the user, driver and platform caps, so it shows the laptop
// firmware's thermal ceiling (80 W -> 15 W on the RTX 3070 Ti Laptop) that
// no power supply reading reflects. libnvidia-ml ships with the driver.

use super::{GpuCap, GpuProbe};
use crate::backend::{Error, Result};
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io;

const NVML_SUCCESS: c_int = 0;

type NvmlReturn = c_int;
type NvmlDevice = *mut c_void;

// SAFETY: prototypes match nvml.h for libnvidia-ml (linked by build.rs with
// the cuda feature): nvmlReturn_t is a C enum (int), nvmlDevice_t an opaque
// struct pointer, the bus id `const char *`, the limits `unsigned int *`. The
// _v2 names are the entry points nvml.h maps nvmlInit and
// nvmlDeviceGetHandleByPciBusId to.
unsafe extern "C" {
    fn nvmlInit_v2() -> NvmlReturn;
    fn nvmlShutdown() -> NvmlReturn;
    fn nvmlErrorString(r: NvmlReturn) -> *const c_char;
    fn nvmlDeviceGetHandleByPciBusId_v2(bus: *const c_char, device: *mut NvmlDevice) -> NvmlReturn;
    fn nvmlDeviceGetEnforcedPowerLimit(device: NvmlDevice, limit_mw: *mut c_uint) -> NvmlReturn;
    fn nvmlDeviceGetPowerManagementDefaultLimit(
        device: NvmlDevice,
        limit_mw: *mut c_uint,
    ) -> NvmlReturn;
}

fn describe(what: &str, r: NvmlReturn) -> String {
    // SAFETY: nvmlErrorString takes any return code by value and no handle;
    // it maps the code to a static string (null is handled below) without
    // needing a successful nvmlInit, so the init-failure path may call it.
    let name = unsafe { nvmlErrorString(r) };
    let name = if name.is_null() {
        "unknown".into()
    } else {
        // SAFETY: non-null (checked above) and NUL-terminated, pointing into
        // libnvidia-ml's static error strings, which live as long as the
        // process (the library is linked, never unloaded).
        unsafe { CStr::from_ptr(name) }.to_string_lossy()
    };
    format!("{what} failed: {name} ({r})")
}

/// One device, found by the PCI address the compute backend runs on.
pub struct NvmlProbe {
    device: NvmlDevice,
}

// NVML is thread-safe; the handle is an opaque id valid until nvmlShutdown.
// SAFETY: the handle stays valid while this probe's own nvmlInit reference is
// held (released only in its Drop), and NVML calls may come from any thread,
// so the probe can move. No Sync impl: polling goes through `&mut self`.
unsafe impl Send for NvmlProbe {}

impl NvmlProbe {
    pub fn new(pci_bus_id: &str) -> Result<Self> {
        let bus = CString::new(pci_bus_id)
            .map_err(|_| Error::Backend(format!("bad PCI bus id {pci_bus_id:?}")))?;
        // SAFETY: nvmlInit_v2 takes no arguments. `bus` is a NUL-terminated
        // CString alive across the lookup and `device` a live out-pointer.
        // After a successful init, the lookup failure calls nvmlShutdown
        // once, and Self is built only once both succeed, so its Drop
        // balances the init exactly once, also when read_cap fails below.
        unsafe {
            let r = nvmlInit_v2();
            if r != NVML_SUCCESS {
                return Err(Error::Backend(describe("nvmlInit_v2", r)));
            }
            let mut device = std::ptr::null_mut();
            let r = nvmlDeviceGetHandleByPciBusId_v2(bus.as_ptr(), &raw mut device);
            if r != NVML_SUCCESS {
                nvmlShutdown();
                return Err(Error::Backend(describe(
                    &format!("nvmlDeviceGetHandleByPciBusId_v2({pci_bus_id})"),
                    r,
                )));
            }
            let probe = Self { device };
            // Fail at startup, not on the first poll, if limits are unsupported.
            probe
                .read_cap()
                .map_err(|e| Error::Backend(e.to_string()))?;
            Ok(probe)
        }
    }

    fn read_cap(&self) -> io::Result<GpuCap> {
        let (mut limit_mw, mut default_mw) = (0, 0);
        // SAFETY: `self.device` came from nvmlDeviceGetHandleByPciBusId_v2 and
        // stays valid while this probe holds its nvmlInit reference;
        // `limit_mw` is a live u32 (c_uint) out-pointer.
        let r = unsafe { nvmlDeviceGetEnforcedPowerLimit(self.device, &raw mut limit_mw) };
        if r != NVML_SUCCESS {
            return Err(io::Error::other(describe(
                "nvmlDeviceGetEnforcedPowerLimit",
                r,
            )));
        }
        // SAFETY: the same live handle as above; `default_mw` is a live u32
        // (c_uint) out-pointer.
        let r =
            unsafe { nvmlDeviceGetPowerManagementDefaultLimit(self.device, &raw mut default_mw) };
        if r != NVML_SUCCESS {
            return Err(io::Error::other(describe(
                "nvmlDeviceGetPowerManagementDefaultLimit",
                r,
            )));
        }
        Ok(GpuCap {
            limit_mw,
            default_mw,
        })
    }
}

impl GpuProbe for NvmlProbe {
    fn read(&mut self) -> io::Result<Option<GpuCap>> {
        let cap = self.read_cap()?;
        // A zero default means the board does not report one; no ratio.
        Ok((cap.default_mw > 0).then_some(cap))
    }
}

impl Drop for NvmlProbe {
    fn drop(&mut self) {
        // SAFETY: balances the one successful nvmlInit_v2 in `new` (Self only
        // exists after it); `self.device` is never used after this.
        unsafe { nvmlShutdown() };
    }
}
