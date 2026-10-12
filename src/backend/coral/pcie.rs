//! The `apex` character device on FreeBSD: register pages, page-table mappings
//! through the `gasket` ioctls, the coherent block the instruction queue lives in,
//! and interrupt events. The ioctl layouts are Google's gasket ABI, which the
//! FreeBSD driver keeps on purpose.

#[allow(unsafe_code)]
mod os;
pub mod host_queue;

use super::regs;
use super::{Error, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

/// Interrupt indices the driver reports in events (`apex_interrupt` in `apexreg.h`).
pub mod interrupt {
    pub const INSTR_QUEUE: u32 = 0;
    pub const INPUT_ACTV_QUEUE: u32 = 1;
    pub const PARAM_QUEUE: u32 = 2;
    pub const OUTPUT_ACTV_QUEUE: u32 = 3;
    pub const SC_HOST_0: u32 = 4;
    pub const SC_HOST_1: u32 = 5;
    pub const SC_HOST_2: u32 = 6;
    pub const SC_HOST_3: u32 = 7;
    pub const TOP_LEVEL_0: u32 = 8;
    pub const TOP_LEVEL_1: u32 = 9;
    pub const TOP_LEVEL_2: u32 = 10;
    pub const TOP_LEVEL_3: u32 = 11;
    pub const FATAL_ERR: u32 = 12;
    pub const COUNT: u32 = 13;
    pub const NAMES: [&str; 13] = [
        "instruction_queue", "input_actv_queue", "param_queue", "output_actv_queue", "sc_host_0", "sc_host_1", "sc_host_2",
        "sc_host_3", "top_level_0", "top_level_1", "top_level_2", "top_level_3", "fatal_err",
    ];
}

/// One interrupt the kernel driver delivered (`struct gasket_interrupt_event`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub interrupt: u32,
    /// Events lost before this one because the kernel ring was full.
    pub dropped: u32,
    /// Cumulative count of this interrupt.
    pub count: u64,
}

impl Event {
    pub const BYTES: usize = 16;

    pub fn name(&self) -> &'static str { interrupt::NAMES.get(self.interrupt as usize).copied().unwrap_or("unknown") }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} #{}{}", self.name(), self.count, if self.dropped > 0 { format!(" ({} dropped before it)", self.dropped) } else { String::new() })
    }
}

/// Direction of a page-table mapping: what the chip may do with the pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    ToDevice,
    FromDevice,
    Bidirectional,
}

/// The driver's clock divider request (`APEX_PERFORMANCE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Performance {
    Low = 0,
    Medium = 1,
    High = 2,
    Max = 3,
}

/// Coherent DMA memory the kernel allocated for this open: host-visible through
/// its own mapping, device-visible once mapped into the page table.
pub struct Coherent {
    mapping: os::Mapping,
    /// What the driver returned as the block's DMA address; on FreeBSD it names
    /// the mmap region rather than a bus address.
    dma_address: u64,
}

impl Coherent {
    pub fn len(&self) -> usize { self.mapping.len() }

    pub fn is_empty(&self) -> bool { self.mapping.len() == 0 }

    pub fn dma_address(&self) -> u64 { self.dma_address }

    /// The host address of byte `offset`, for page-table mapping.
    pub fn host_address(&self, offset: usize) -> u64 { self.mapping.address() + offset as u64 }

    pub fn read64(&self, offset: usize) -> u64 { os::read64(&self.mapping, offset) }

    pub fn read32(&self, offset: usize) -> u32 { os::read32(&self.mapping, offset) }

    pub fn write64(&self, offset: usize, value: u64) { os::write64(&self.mapping, offset, value); }
}

/// An open `/dev/apexN`.
pub struct Device {
    path: String,
    file: File,
    regions: Vec<os::Mapping>,
    coherent: Option<Coherent>,
}

impl Device {
    pub fn open(path: &str) -> Result<Self> {
        let at = |op: &'static str| move |source: std::io::Error| Error::Device { path: path.into(), op, source };
        // Non-blocking at open: event reads must not block, and the driver answers
        // FIONBIO (what fcntl would send) with ENOTTY.
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).map_err(at("open"))?;
        let mut regions = Vec::with_capacity(regs::REGIONS.len());
        for base in regs::REGIONS {
            regions.push(os::Mapping::device(file.as_fd(), base, regs::REGION_SIZE as usize).map_err(at("map registers"))?);
        }
        Ok(Self { path: path.into(), file, regions, coherent: None })
    }

    pub fn path(&self) -> &str { &self.path }

    fn io(&self, op: &'static str, source: std::io::Error) -> Error { Error::Device { path: self.path.clone(), op, source } }

    fn region(&self, offset: u64, width: u64) -> Result<(&os::Mapping, usize)> {
        if !offset.is_multiple_of(width) {
            return Err(Error::Protocol(format!("register 0x{offset:x} is not {width}-byte aligned")));
        }
        regs::REGIONS.iter().zip(&self.regions).find(|(base, _)| offset >= **base && offset + width <= **base + regs::REGION_SIZE)
            .map(|(base, mapping)| (mapping, (offset - base) as usize))
            .ok_or_else(|| Error::Protocol(format!("register 0x{offset:x} is not in a user-mappable page")))
    }

    pub fn read(&self, offset: u64) -> Result<u64> {
        let (mapping, at) = self.region(offset, 8)?;
        Ok(os::read64(mapping, at))
    }

    pub fn write(&self, offset: u64, value: u64) -> Result<()> {
        let (mapping, at) = self.region(offset, 8)?;
        os::write64(mapping, at, value);
        Ok(())
    }

    /// Spins until `register & mask == expected & mask`, returning the last value read.
    pub fn poll(&self, name: &'static str, offset: u64, expected: u64, mask: u64, timeout: Duration) -> Result<u64> {
        let start = Instant::now();
        loop {
            let value = self.read(offset)?;
            if value & mask == expected & mask {
                return Ok(value);
            }
            if start.elapsed() > timeout {
                return Err(Error::Poll { name, offset, expected, last: value, timeout });
            }
            std::thread::sleep(Duration::from_micros(20));
        }
    }

    /// Reserves the first `simple_entries` page-table entries for one-page mappings;
    /// the rest become extended entries.
    pub fn partition_page_table(&self, simple_entries: u64) -> Result<()> {
        let mut request = os::PageTableIoctl { page_table_index: 0, size: simple_entries, host_address: 0, device_address: 0 };
        os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_PARTITION_PAGE_TABLE, &mut request).map_err(|e| self.io("partition page table", e))
    }

    /// Asks the kernel for `size` bytes of coherent DMA memory and maps them.
    pub fn allocate_coherent(&mut self, size: usize) -> Result<&Coherent> {
        if self.coherent.is_some() {
            return Err(Error::Protocol("coherent memory is already allocated".into()));
        }
        let mut request = os::CoherentAllocConfigIoctl { page_table_index: 0, enable: 1, size: size as u64, dma_address: 0 };
        os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR, &mut request).map_err(|e| self.io("allocate coherent memory", e))?;
        let mapping = match os::Mapping::device(self.file.as_fd(), request.dma_address, size) {
            Ok(mapping) => mapping,
            Err(source) => {
                let mut release = os::CoherentAllocConfigIoctl { page_table_index: 0, enable: 0, size: size as u64, dma_address: request.dma_address };
                // The allocation is released whether or not this ioctl succeeds: the mmap failure is the error reported.
                let _ = os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR, &mut release);
                return Err(self.io("map coherent memory", source));
            }
        };
        Ok(self.coherent.insert(Coherent { mapping, dma_address: request.dma_address }))
    }

    pub fn coherent(&self) -> Result<&Coherent> {
        self.coherent.as_ref().ok_or_else(|| Error::Protocol("coherent memory is not allocated".into()))
    }

    pub fn release_coherent(&mut self) -> Result<()> {
        let Some(coherent) = self.coherent.take() else { return Ok(()) };
        let mut request =
            os::CoherentAllocConfigIoctl { page_table_index: 0, enable: 0, size: coherent.len() as u64, dma_address: coherent.dma_address };
        drop(coherent);
        os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_CONFIG_COHERENT_ALLOCATOR, &mut request).map_err(|e| self.io("release coherent memory", e))
    }

    /// Maps `len` bytes of host memory at `host_address` (both page aligned) at
    /// `device_address`. The memory must stay allocated until `unmap`.
    pub fn map(&self, host_address: u64, len: usize, device_address: u64, direction: DmaDirection) -> Result<()> {
        let page = super::address_space::PAGE;
        if !host_address.is_multiple_of(page) || len == 0 || !(len as u64).is_multiple_of(page) || !device_address.is_multiple_of(page) {
            return Err(Error::Protocol(format!(
                "mapping of {len} bytes at host 0x{host_address:x} to device 0x{device_address:x} is not page aligned"
            )));
        }
        let flags = match direction {
            DmaDirection::Bidirectional => os::GASKET_DMA_BIDIRECTIONAL,
            DmaDirection::ToDevice => os::GASKET_DMA_TO_DEVICE,
            DmaDirection::FromDevice => os::GASKET_DMA_FROM_DEVICE,
        };
        let mut request = os::PageTableIoctlFlags {
            base: os::PageTableIoctl { page_table_index: 0, size: len as u64, host_address, device_address },
            flags,
            pad: 0,
        };
        os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_MAP_BUFFER_FLAGS, &mut request).map_err(|e| self.io("map buffer", e))
    }

    pub fn unmap(&self, device_address: u64, len: usize) -> Result<()> {
        let mut request = os::PageTableIoctl { page_table_index: 0, size: len as u64, host_address: 0, device_address };
        os::ioctl(self.file.as_fd(), os::GASKET_IOCTL_UNMAP_BUFFER, &mut request).map_err(|e| self.io("unmap buffer", e))
    }

    pub fn set_performance(&self, performance: Performance) -> Result<()> {
        let mut request = os::PerformanceExpectationIoctl { performance: performance as u32 };
        os::ioctl(self.file.as_fd(), os::APEX_IOCTL_PERFORMANCE_EXPECTATION, &mut request).map_err(|e| self.io("set performance", e))
    }

    /// Every interrupt event the kernel has queued, without blocking.
    pub fn events(&self) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut buf = [0u8; Event::BYTES * 32];
        loop {
            let n = os::read_nonblocking(self.file.as_fd(), &mut buf).map_err(|e| self.io("read events", e))?;
            if n == 0 {
                return Ok(events);
            }
            if n % Event::BYTES != 0 {
                return Err(Error::Protocol(format!("event read returned {n} bytes, not a multiple of {}", Event::BYTES)));
            }
            for raw in buf[..n].as_chunks::<{ Event::BYTES }>().0 {
                events.push(Event {
                    interrupt: u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                    dropped: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                    count: u64::from_le_bytes([raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]]),
                });
            }
        }
    }
}
