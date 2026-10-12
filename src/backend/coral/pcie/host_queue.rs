//! The instruction queue: a ring of 16-byte descriptors in coherent memory that the
//! chip fetches instruction chunks from, and a status block it writes back with the
//! completed head. Only this queue is a host queue on PCIe; the program's own
//! descriptors drive the activation and parameter DMAs.

use super::{Device, DmaDirection};
use crate::backend::coral::address_space::{AddressSpace, PAGE};
use crate::backend::coral::regs;
use crate::backend::coral::{Error, Result};
use std::time::Duration;

/// Entries in the ring; a power of two, and what the chip is told.
pub const SIZE: u32 = 256;
pub const DESCRIPTOR_BYTES: usize = 16;
/// Coherent bytes the queue needs: one page of descriptors, one for the status block.
pub const COHERENT_BYTES: usize = 2 * PAGE as usize;
const RING_OFFSET: usize = 0;
const STATUS_BLOCK_OFFSET: usize = PAGE as usize;
const SETUP_TIMEOUT: Duration = Duration::from_millis(500);

const _: () = assert!(SIZE.is_power_of_two() && SIZE as usize * DESCRIPTOR_BYTES <= PAGE as usize);

/// What the chip wrote to the status block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusBlock {
    pub completed_head: u32,
    pub fatal_error: u32,
}

pub struct HostQueue {
    ring_device: u64,
    status_device: u64,
    tail: u32,
    completed_head: u32,
    enabled: bool,
}

impl HostQueue {
    /// Allocates the ring and the status block in the device's coherent memory,
    /// maps them with simple page-table entries and enables the queue.
    pub fn open(device: &Device, simple: &mut AddressSpace) -> Result<Self> {
        let descriptor_size = device.read(regs::INSTRUCTION_QUEUE_DESCRIPTOR_SIZE)?;
        if descriptor_size != DESCRIPTOR_BYTES as u64 {
            return Err(Error::HostQueue(format!("the chip reports {descriptor_size}-byte descriptors, expected {DESCRIPTOR_BYTES}")));
        }
        let coherent = device.coherent()?;
        if coherent.len() < COHERENT_BYTES {
            return Err(Error::HostQueue(format!("{} coherent bytes, need {COHERENT_BYTES}", coherent.len())));
        }
        for offset in (0..COHERENT_BYTES).step_by(8) {
            coherent.write64(offset, 0);
        }
        let ring_device = simple.allocate(PAGE)?;
        let status_device = match simple.allocate(PAGE) {
            Ok(address) => address,
            Err(error) => {
                simple.free(ring_device, PAGE)?;
                return Err(error);
            }
        };
        let mapped = device.map(coherent.host_address(RING_OFFSET), PAGE as usize, ring_device, DmaDirection::Bidirectional)
            .and_then(|()| device.map(coherent.host_address(STATUS_BLOCK_OFFSET), PAGE as usize, status_device, DmaDirection::Bidirectional));
        let mut queue = Self { ring_device, status_device, tail: 0, completed_head: 0, enabled: false };
        if let Err(error) = mapped.and_then(|()| queue.enable(device)) {
            queue.release(device, simple)?;
            return Err(error);
        }
        queue.enabled = true;
        Ok(queue)
    }

    fn enable(&self, device: &Device) -> Result<()> {
        device.write(regs::INSTRUCTION_QUEUE_BASE, self.ring_device)?;
        device.write(regs::INSTRUCTION_QUEUE_STATUS_BLOCK_BASE, self.status_device)?;
        device.write(regs::INSTRUCTION_QUEUE_SIZE, u64::from(SIZE))?;
        device.write(regs::INSTRUCTION_QUEUE_CONTROL, regs::QUEUE_CONTROL_ENABLE | regs::QUEUE_CONTROL_STATUS_BLOCK_WRITE)?;
        device.poll("instruction_queue_status", regs::INSTRUCTION_QUEUE_STATUS, 1, 1, SETUP_TIMEOUT)?;
        Ok(())
    }

    fn release(&self, device: &Device, simple: &mut AddressSpace) -> Result<()> {
        let unmapped = device.unmap(self.ring_device, PAGE as usize).and_then(|()| device.unmap(self.status_device, PAGE as usize));
        simple.free(self.ring_device, PAGE)?;
        simple.free(self.status_device, PAGE)?;
        unmapped
    }

    pub fn tail(&self) -> u32 { self.tail }

    /// Slots free for new descriptors; one slot always stays empty.
    pub fn available(&self) -> u32 { self.completed_head.wrapping_sub(self.tail).wrapping_sub(1) & (SIZE - 1) }

    /// Queues one instruction chunk at device address `address`.
    pub fn enqueue(&mut self, device: &Device, address: u64, size: u32) -> Result<()> {
        if !self.enabled {
            return Err(Error::HostQueue("the queue is closed".into()));
        }
        if self.available() == 0 {
            return Err(Error::HostQueue(format!("no space: completed head {}, tail {}, size {SIZE}", self.completed_head, self.tail)));
        }
        let coherent = device.coherent()?;
        let at = RING_OFFSET + self.tail as usize * DESCRIPTOR_BYTES;
        coherent.write64(at, address);
        coherent.write64(at + 8, u64::from(size));
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.tail = (self.tail + 1) & (SIZE - 1);
        device.write(regs::INSTRUCTION_QUEUE_TAIL, u64::from(self.tail))
    }

    pub fn status(&self, device: &Device) -> Result<StatusBlock> {
        let coherent = device.coherent()?;
        Ok(StatusBlock { completed_head: coherent.read32(STATUS_BLOCK_OFFSET), fatal_error: coherent.read32(STATUS_BLOCK_OFFSET + 4) })
    }

    /// Takes the chip's completed head into account; true when every queued
    /// descriptor has been consumed.
    pub fn reconcile(&mut self, device: &Device) -> Result<bool> {
        let status = self.status(device)?;
        if status.fatal_error != 0 {
            return Err(Error::HostQueue(format!("fatal error 0x{:x} reported in the status block", status.fatal_error)));
        }
        self.completed_head = status.completed_head & (SIZE - 1);
        Ok(self.completed_head == self.tail)
    }

    /// Disables the queue and returns its memory; a second call does nothing.
    pub fn close(&mut self, device: &Device, simple: &mut AddressSpace, in_error: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.enabled = false;
        device.write(regs::INSTRUCTION_QUEUE_CONTROL, 0)?;
        if !in_error {
            device.poll("instruction_queue_status", regs::INSTRUCTION_QUEUE_STATUS, 0, 1, SETUP_TIMEOUT)?;
        }
        device.write(regs::INSTRUCTION_QUEUE_TAIL, 0)?;
        device.write(regs::INSTRUCTION_QUEUE_BASE, 0)?;
        device.write(regs::INSTRUCTION_QUEUE_STATUS_BLOCK_BASE, 0)?;
        self.release(device, simple)
    }
}
