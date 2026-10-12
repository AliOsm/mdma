//! The system calls behind the transport: mmap of device pages, gasket and apex
//! ioctls, volatile register access and the non-blocking event read. Every unsafe
//! block states the invariant it relies on; nothing else in the backend is unsafe.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

// FreeBSD ioctl encoding (<sys/ioccom.h>): the parameter length is part of the number.
const IOCPARM_MASK: u64 = 0x1fff;
const IOC_VOID: u64 = 0x2000_0000;
const IOC_OUT: u64 = 0x4000_0000;
const IOC_IN: u64 = 0x8000_0000;

const fn ioc(inout: u64, group: u8, num: u8, len: usize) -> libc::c_ulong {
    (inout | ((len as u64 & IOCPARM_MASK) << 16) | ((group as u64) << 8) | num as u64)
        as libc::c_ulong
}

const GASKET_IOC_GROUP: u8 = 0xDC;
const APEX_IOC_GROUP: u8 = 0x7F;

/// `struct gasket_page_table_ioctl`.
#[repr(C)]
pub(super) struct PageTableIoctl {
    pub page_table_index: u64,
    pub size: u64,
    pub host_address: u64,
    pub device_address: u64,
}

/// `struct gasket_page_table_ioctl_flags`; the padding keeps it 40 bytes on every ABI.
#[repr(C)]
pub(super) struct PageTableIoctlFlags {
    pub base: PageTableIoctl,
    pub flags: u32,
    pub pad: u32,
}

/// `struct gasket_coherent_alloc_config_ioctl`.
#[repr(C)]
pub(super) struct CoherentAllocConfigIoctl {
    pub page_table_index: u64,
    pub enable: u64,
    pub size: u64,
    pub dma_address: u64,
}

/// `struct apex_performance_expectation_ioctl`.
#[repr(C)]
pub(super) struct PerformanceExpectationIoctl {
    pub performance: u32,
}

const _: () = assert!(size_of::<PageTableIoctl>() == 32);
const _: () = assert!(size_of::<PageTableIoctlFlags>() == 40);
const _: () = assert!(size_of::<CoherentAllocConfigIoctl>() == 32);

pub(super) const GASKET_IOCTL_PARTITION_PAGE_TABLE: libc::c_ulong =
    ioc(IOC_IN, GASKET_IOC_GROUP, 7, size_of::<PageTableIoctl>());
pub(super) const GASKET_IOCTL_UNMAP_BUFFER: libc::c_ulong =
    ioc(IOC_IN, GASKET_IOC_GROUP, 9, size_of::<PageTableIoctl>());
pub(super) const GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR: libc::c_ulong = ioc(
    IOC_IN | IOC_OUT,
    GASKET_IOC_GROUP,
    11,
    size_of::<CoherentAllocConfigIoctl>(),
);
pub(super) const GASKET_IOCTL_MAP_BUFFER_FLAGS: libc::c_ulong = ioc(
    IOC_IN,
    GASKET_IOC_GROUP,
    12,
    size_of::<PageTableIoctlFlags>(),
);
pub(super) const APEX_IOCTL_PERFORMANCE_EXPECTATION: libc::c_ulong = ioc(
    IOC_IN,
    APEX_IOC_GROUP,
    1,
    size_of::<PerformanceExpectationIoctl>(),
);
#[allow(
    dead_code,
    reason = "the void-argument encoding is kept next to the ones in use"
)]
pub(super) const GASKET_IOCTL_RESET: libc::c_ulong = ioc(IOC_VOID, GASKET_IOC_GROUP, 0, 0);

/// DMA direction flags of `GASKET_IOCTL_MAP_BUFFER_FLAGS`, already shifted into place.
pub(super) const GASKET_DMA_BIDIRECTIONAL: u32 = 0 << 1;
pub(super) const GASKET_DMA_TO_DEVICE: u32 = 1 << 1;
pub(super) const GASKET_DMA_FROM_DEVICE: u32 = 2 << 1;

/// A shared mapping of a device file region, unmapped on drop.
pub(super) struct Mapping {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is ordinary process memory with no thread affinity; access
// goes through the volatile helpers below, which take `&self`.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn device(fd: BorrowedFd<'_>, offset: u64, len: usize) -> io::Result<Self> {
        // SAFETY: mmap of a file descriptor with a null hint creates a fresh
        // mapping that aliases nothing in this process; the result is checked.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                libc::off_t::try_from(offset)
                    .map_err(|_| io::Error::other("mmap offset does not fit off_t"))?,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn address(&self) -> u64 {
        self.ptr as u64
    }

    /// A pointer `width` bytes wide at `offset`: the mapping is page aligned, so an
    /// offset that is a multiple of `width` is aligned for it.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "checked above: page-aligned base plus a width-aligned offset"
    )]
    fn at<T>(&self, offset: usize) -> *mut T {
        let width = size_of::<T>();
        assert!(
            offset.is_multiple_of(width) && offset + width <= self.len,
            "register access at {offset} ({width} bytes) outside {} bytes",
            self.len
        );
        self.ptr.wrapping_add(offset).cast::<T>()
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful mmap and are unmapped once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

pub(super) fn read64(mapping: &Mapping, offset: usize) -> u64 {
    // SAFETY: `at` checked alignment and bounds inside the live mapping.
    unsafe { ptr::read_volatile(mapping.at::<u64>(offset)) }
}

pub(super) fn read32(mapping: &Mapping, offset: usize) -> u32 {
    // SAFETY: as for `read64`.
    unsafe { ptr::read_volatile(mapping.at::<u32>(offset)) }
}

pub(super) fn write64(mapping: &Mapping, offset: usize, value: u64) {
    // SAFETY: as for `read64`; device registers are written with one 64-bit store.
    unsafe { ptr::write_volatile(mapping.at::<u64>(offset), value) }
}

pub(super) fn ioctl<T>(fd: BorrowedFd<'_>, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `request` encodes `size_of::<T>()`, so the kernel copies exactly the
    // struct `arg` points at, which lives for the call.
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            request,
            ptr::from_mut(arg).cast::<libc::c_void>(),
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Reads what is available; zero when nothing is queued.
pub(super) fn read_nonblocking(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the kernel writes at most `buf.len()` bytes into the live buffer.
    let n = unsafe {
        libc::read(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
        )
    };
    if let Ok(n) = usize::try_from(n) {
        return Ok(n);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(0)
    } else {
        Err(error)
    }
}
