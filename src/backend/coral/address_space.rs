//! Device virtual addresses for the chip's page table. The simple range maps one
//! 4 KiB host page per entry; the extended range (bit 63 set) maps 2 MiB per entry,
//! each pointing at a host page of 512 page addresses the kernel driver maintains.
//! Both hand out whole pages, first fit, and coalesce on free. The hardware
//! constants are Beagle's (libedgetpu `hardware_structures.h`).

use super::{Error, Result};
use std::collections::BTreeMap;

pub const PAGE: u64 = 4096;
pub const EXTENDED_BIT: u64 = 1 << 63;
/// Bytes one extended page-table entry addresses.
pub const EXTENDED_ENTRY_BYTES: u64 = 512 * PAGE;
/// Page-table entries the Beagle HIB has.
pub const PAGE_TABLE_ENTRIES: u64 = 8192;
/// The extended range may not exceed 4 GiB.
pub const MAX_EXTENDED_ENTRIES: u64 = (4096 * 1024 * 1024) / EXTENDED_ENTRY_BYTES;
pub const MIN_SIMPLE_ENTRIES: u64 = 256;

/// How libedgetpu partitions the table: everything the extended range cannot use
/// stays simple, with a floor.
pub const fn simple_entries(total: u64) -> u64 {
    let simple = total.saturating_sub(MAX_EXTENDED_ENTRIES);
    if simple < MIN_SIMPLE_ENTRIES { MIN_SIMPLE_ENTRIES } else { simple }
}

pub const fn extended_entries(total: u64) -> u64 { total - simple_entries(total) }

pub const fn round_up_to_page(bytes: u64) -> u64 { bytes.div_ceil(PAGE) * PAGE }

/// Page-granular allocator over one contiguous device address range.
#[derive(Debug)]
pub struct AddressSpace {
    start: u64,
    size: u64,
    /// Free runs, keyed by start address.
    free: BTreeMap<u64, u64>,
    /// Live allocations, keyed by start address.
    allocated: BTreeMap<u64, u64>,
}

impl AddressSpace {
    pub fn new(start: u64, size: u64) -> Self {
        let mut free = BTreeMap::new();
        if size > 0 {
            free.insert(start, size);
        }
        Self { start, size, free, allocated: BTreeMap::new() }
    }

    pub fn simple(entries: u64) -> Self { Self::new(0, entries * PAGE) }

    pub fn extended(entries: u64) -> Self { Self::new(EXTENDED_BIT, entries * EXTENDED_ENTRY_BYTES) }

    pub fn contains(&self, address: u64) -> bool { address >= self.start && address - self.start < self.size }

    pub fn free_bytes(&self) -> u64 { self.free.values().sum() }

    pub fn allocated_bytes(&self) -> u64 { self.allocated.values().sum() }

    /// A page-aligned run of at least `bytes` (rounded up to whole pages).
    pub fn allocate(&mut self, bytes: u64) -> Result<u64> {
        let exhausted = |free: u64| Error::AddressSpace { requested: bytes, free };
        if bytes == 0 {
            return Err(Error::Protocol("cannot allocate zero device bytes".into()));
        }
        let size = round_up_to_page(bytes);
        let (&start, &run) = self.free.iter().find(|(_, run)| **run >= size).ok_or_else(|| exhausted(self.free_bytes()))?;
        self.free.remove(&start);
        if run > size {
            self.free.insert(start + size, run - size);
        }
        self.allocated.insert(start, size);
        Ok(start)
    }

    /// Returns an allocation; `bytes` must be what `allocate` was asked for.
    pub fn free(&mut self, address: u64, bytes: u64) -> Result<()> {
        let size = round_up_to_page(bytes);
        match self.allocated.remove(&address) {
            Some(allocated) if allocated == size => {}
            Some(allocated) => {
                self.allocated.insert(address, allocated);
                return Err(Error::Protocol(format!("device address 0x{address:x} holds {allocated} bytes, not {size}")));
            }
            None => return Err(Error::Protocol(format!("device address 0x{address:x} is not allocated"))),
        }
        let mut start = address;
        let mut run = size;
        if let Some((&before, &before_run)) = self.free.range(..address).next_back() && before + before_run == address {
            self.free.remove(&before);
            start = before;
            run += before_run;
        }
        if let Some(&after_run) = self.free.get(&(address + size)) {
            self.free.remove(&(address + size));
            run += after_run;
        }
        self.free.insert(start, run);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
