//! Runs programs on one Edge TPU over PCIe. `open` follows libedgetpu's MMIO driver:
//! the kernel has already taken the chip out of reset, so user space checks the
//! host interface, partitions the page table, sets up the instruction queue, moves
//! every unit to run and enables interrupts. `run` maps the program's buffers,
//! links their device addresses into the instruction chunks, queues the chunks and
//! waits for scalar-core interrupt 0, which every program raises when it ends.

use super::address_space::{
    AddressSpace, PAGE, PAGE_TABLE_ENTRIES, extended_entries, round_up_to_page, simple_entries,
};
use super::executable::{Description, Executable, ExecutableType, Hint};
use super::link::{Addresses, link};
use super::pcie::host_queue::{self, HostQueue};
use super::pcie::{Device, DmaDirection, Event, Performance, interrupt};
use super::regs::{self, RunControl};
use super::{Error, Result};
use std::time::{Duration, Instant};

/// The first card's node, as each driver names it.
pub const DEFAULT_DEVICE: &str = cfg_select! {
    target_os = "linux" => { "/dev/apex_0" }
    _ => { "/dev/apex0" }
};

#[derive(Clone, Debug)]
pub struct Options {
    pub device: String,
    pub performance: Performance,
    /// How long one program may run.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.into(),
            performance: Performance::High,
            timeout: Duration::from_secs(5),
        }
    }
}

/// Host memory the chip can read or write: page aligned, a whole number of pages.
pub struct PageBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl PageBuf {
    pub fn zeroed(bytes: usize) -> Self {
        let len = round_up_to_page(bytes as u64) as usize;
        let storage = vec![0u8; len + PAGE as usize];
        let start = (PAGE as usize - storage.as_ptr() as usize % PAGE as usize) % PAGE as usize;
        Self {
            storage,
            start,
            len,
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut buf = Self::zeroed(bytes.len());
        buf.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
        buf
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.len]
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }

    pub fn host_address(&self) -> u64 {
        self.as_slice().as_ptr() as u64
    }
}

/// A buffer the chip can reach until it is unmapped.
struct Mapped {
    buf: PageBuf,
    device: u64,
}

/// A parameter blob, mapped for a caching program to upload.
pub struct Parameters {
    mapped: Mapped,
    len: usize,
    pub token: u64,
}

impl Parameters {
    pub fn device_address(&self) -> u64 {
        self.mapped.device
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct RunOutput {
    pub outputs: Vec<(String, Vec<u8>)>,
    pub elapsed: Duration,
    /// Interrupt events the kernel delivered during the run.
    pub events: Vec<Event>,
}

/// Device state the run needs to report in a timeout.
fn describe(device: &Device, queue: &HostQueue) -> String {
    let read = |offset| {
        device
            .read(offset)
            .map_or_else(|e| e.to_string(), |v| format!("0x{v:x}"))
    };
    let status = queue
        .status(device)
        .map_or_else(|e| e.to_string(), |s| format!("{s:?}"));
    format!(
        "queue tail {} status {status}, sc_host_int_count {}, sc_host_int_status {}, hib_error_status {}, scalar_core_run_status {}, \
         error_scalar_core {}, error_tile {}",
        queue.tail(),
        read(regs::SC_HOST_INT_COUNT),
        read(regs::SC_HOST_INT_STATUS),
        read(regs::HIB_ERROR_STATUS),
        read(regs::SCALAR_CORE_RUN_STATUS),
        read(regs::ERROR_SCALAR_CORE),
        read(regs::ERROR_TILE),
    )
}

pub struct Tpu {
    options: Options,
    device: Device,
    simple: AddressSpace,
    extended: AddressSpace,
    queue: HostQueue,
    /// Last seen 16-bit counters of the four scalar-core interrupts.
    int_counts: [u16; regs::SC_HOST_INTERRUPTS as usize],
    live_parameters: usize,
    closed: bool,
}

impl Tpu {
    pub fn open(options: Options) -> Result<Self> {
        let mut device = Device::open(&options.device)?;
        device.set_performance(options.performance)?;
        let mut simple = AddressSpace::simple(simple_entries(PAGE_TABLE_ENTRIES));
        let extended = AddressSpace::extended(extended_entries(PAGE_TABLE_ENTRIES));
        Self::check_hib(&device)?;
        device.write(regs::DMA_BURST_LIMITER, 0)?;
        device.partition_page_table(simple_entries(PAGE_TABLE_ENTRIES))?;
        device.allocate_coherent(host_queue::COHERENT_BYTES)?;
        let queue = match HostQueue::open(&device, &mut simple) {
            Ok(queue) => queue,
            Err(error) => {
                device.release_coherent()?;
                return Err(error);
            }
        };
        let mut tpu = Self {
            options,
            device,
            simple,
            extended,
            queue,
            int_counts: [0; 4],
            live_parameters: 0,
            closed: false,
        };
        if let Err(error) = tpu.start() {
            tpu.shutdown(true)?;
            return Err(error);
        }
        Ok(tpu)
    }

    fn check_hib(device: &Device) -> Result<()> {
        let status = device.read(regs::HIB_ERROR_STATUS)?;
        if status == 0 {
            return Ok(());
        }
        Err(Error::Hib {
            status,
            first: device.read(regs::HIB_FIRST_ERROR_STATUS)?,
            decoded: regs::hib_error_names(status),
        })
    }

    fn start(&mut self) -> Result<()> {
        let raw = self.device.read(regs::SC_HOST_INT_COUNT)?;
        for (id, count) in self.int_counts.iter_mut().enumerate() {
            *count = regs::sc_host_int_count(raw, id as u64);
        }
        self.run_control(RunControl::Run)?;
        self.device.write(regs::STATUS_BLOCK_UPDATE, 0)?;
        self.device.write(
            regs::SC_HOST_INT_CONTROL,
            (1 << regs::SC_HOST_INTERRUPTS) - 1,
        )?;
        self.device.write(regs::INSTRUCTION_QUEUE_INT_CONTROL, 1)?;
        self.device.write(regs::FATAL_ERR_INT_CONTROL, 1)?;
        // Events from before this open are not ours.
        self.device.events()?;
        Ok(())
    }

    fn run_control(&self, state: RunControl) -> Result<()> {
        for offset in regs::SCALAR_CORE_RUN_CONTROLS {
            self.device.write(offset, state as u64)?;
        }
        self.device
            .write(regs::TILECONFIG0, regs::TILECONFIG_BROADCAST)?;
        self.device.poll(
            "tileconfig0",
            regs::TILECONFIG0,
            regs::TILECONFIG_BROADCAST,
            u64::MAX,
            Duration::from_millis(500),
        )?;
        for offset in regs::TILE_RUN_CONTROLS {
            self.device.write(offset, state as u64)?;
        }
        Ok(())
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    fn map(&mut self, buf: PageBuf, direction: DmaDirection) -> Result<Mapped> {
        let device = self.extended.allocate(buf.len() as u64)?;
        if let Err(error) = self
            .device
            .map(buf.host_address(), buf.len(), device, direction)
        {
            self.extended.free(device, buf.len() as u64)?;
            return Err(error);
        }
        Ok(Mapped { buf, device })
    }

    fn unmap(&mut self, mapped: Mapped) -> Result<PageBuf> {
        let unmapped = self.device.unmap(mapped.device, mapped.buf.len());
        self.extended.free(mapped.device, mapped.buf.len() as u64)?;
        unmapped.map(|()| mapped.buf)
    }

    fn unmap_all(&mut self, mapped: Vec<Mapped>) -> Result<Vec<PageBuf>> {
        let mut bufs = Vec::with_capacity(mapped.len());
        let mut first_error = None;
        for m in mapped {
            match self.unmap(m) {
                Ok(buf) => bufs.push(buf),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(bufs), Err)
    }

    /// Maps a parameter blob so a caching program can upload it; `token` is the
    /// caching token of the programs it belongs to.
    pub fn load_parameters(&mut self, bytes: &[u8], token: u64) -> Result<Parameters> {
        if bytes.is_empty() {
            return Err(Error::Protocol(
                "cannot load an empty parameter blob".into(),
            ));
        }
        let mapped = self.map(PageBuf::from_bytes(bytes), DmaDirection::ToDevice)?;
        self.live_parameters += 1;
        Ok(Parameters {
            mapped,
            len: bytes.len(),
            token,
        })
    }

    pub fn unload_parameters(&mut self, parameters: Parameters) -> Result<()> {
        self.live_parameters -= 1;
        self.unmap(parameters.mapped).map(drop)
    }

    /// Runs `exe` once with `inputs` (name, bytes of the layer's padded size).
    pub fn run(
        &mut self,
        exe: &Executable,
        parameters: Option<&Parameters>,
        inputs: &[(&str, &[u8])],
    ) -> Result<RunOutput> {
        exe.validate()?;
        let needs_parameters = exe
            .bitstreams
            .iter()
            .flat_map(|b| &b.relocs)
            .any(|r| r.desc == Description::Parameter)
            || exe.hints.iter().any(|h| {
                matches!(
                    h,
                    Hint::Dma {
                        desc: Description::Parameter,
                        ..
                    }
                )
            });
        if needs_parameters && parameters.is_none() {
            return Err(Error::Executable(format!(
                "{}: needs its parameters loaded",
                exe.name
            )));
        }
        if let Some(parameters) = parameters {
            for hint in &exe.hints {
                if let Hint::Dma {
                    desc: Description::Parameter,
                    offset,
                    size,
                    ..
                } = hint
                    && offset + size > parameters.len()
                {
                    return Err(Error::Executable(format!(
                        "{}: parameter DMA of {size} bytes at {offset} is outside the {}-byte blob loaded",
                        exe.name,
                        parameters.len()
                    )));
                }
            }
        }
        if exe.kind == ExecutableType::ParameterCaching
            && exe.inputs.is_empty()
            && !inputs.is_empty()
        {
            return Err(Error::Executable(format!(
                "{}: a caching program takes no inputs",
                exe.name
            )));
        }
        if exe.batch_size != 1 {
            return Err(Error::Executable(format!(
                "{}: batch size {} is not supported yet",
                exe.name, exe.batch_size
            )));
        }
        let mut addresses = Addresses {
            parameter: parameters.map(Parameters::device_address),
            ..Addresses::default()
        };
        let mut mapped = Vec::new();
        let outcome = self
            .prepare(exe, inputs, &mut addresses, &mut mapped)
            .and_then(|chunks| self.execute(exe, &chunks));
        let bufs = self.unmap_all(mapped);
        match (outcome, bufs) {
            (Ok((elapsed, events)), Ok(bufs)) => {
                let outputs = exe
                    .outputs
                    .iter()
                    .zip(bufs)
                    .map(|(layer, buf)| {
                        (
                            layer.name.clone(),
                            buf.as_slice()[..layer.size_bytes].to_vec(),
                        )
                    })
                    .collect();
                Ok(RunOutput {
                    outputs,
                    elapsed,
                    events,
                })
            }
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(Error::Protocol(format!(
                "{error}; unmapping afterwards also failed: {cleanup}"
            ))),
        }
    }

    /// Maps outputs first (so `unmap_all` returns them first), then inputs and
    /// scratch, then the linked instruction chunks; returns the chunks' device
    /// addresses and sizes.
    fn prepare(
        &mut self,
        exe: &Executable,
        inputs: &[(&str, &[u8])],
        addresses: &mut Addresses,
        mapped: &mut Vec<Mapped>,
    ) -> Result<Vec<(u64, u32)>> {
        for layer in &exe.outputs {
            let m = self.map(PageBuf::zeroed(layer.size_bytes), DmaDirection::FromDevice)?;
            addresses.outputs.insert(layer.name.clone(), vec![m.device]);
            mapped.push(m);
        }
        for layer in &exe.inputs {
            let (_, bytes) = inputs
                .iter()
                .find(|(name, _)| *name == layer.name)
                .ok_or_else(|| {
                    Error::Executable(format!(
                        "{}: input {:?} was not provided",
                        exe.name, layer.name
                    ))
                })?;
            if bytes.len() != layer.size_bytes {
                return Err(Error::Executable(format!(
                    "{}: input {:?} has {} bytes, the program expects {}",
                    exe.name,
                    layer.name,
                    bytes.len(),
                    layer.size_bytes
                )));
            }
            let m = self.map(PageBuf::from_bytes(bytes), DmaDirection::ToDevice)?;
            addresses.inputs.insert(layer.name.clone(), vec![m.device]);
            mapped.push(m);
        }
        if let Some((name, _)) = inputs.iter().find(|(name, _)| exe.input(name).is_none()) {
            return Err(Error::Executable(format!(
                "{}: has no input {name:?}",
                exe.name
            )));
        }
        if exe.scratch_size > 0 {
            let m = self.map(
                PageBuf::zeroed(exe.scratch_size),
                DmaDirection::Bidirectional,
            )?;
            addresses.scratch = Some(m.device);
            mapped.push(m);
        }
        let mut chunks = Vec::with_capacity(exe.bitstreams.len());
        for bitstream in &exe.bitstreams {
            let linked = link(bitstream, addresses)?;
            let size = u32::try_from(linked.len()).map_err(|_| {
                Error::Executable(format!("{}: instruction chunk too large", exe.name))
            })?;
            let m = self.map(PageBuf::from_bytes(&linked), DmaDirection::ToDevice)?;
            chunks.push((m.device, size));
            mapped.push(m);
        }
        Ok(chunks)
    }

    /// Queues the chunks and waits for scalar-core interrupt 0. Completion is the
    /// chip's own interrupt counter moving by one; the kernel's events are only
    /// collected, since they can arrive before or after the counter and a run
    /// reported done early would unmap buffers the chip is still writing.
    fn execute(
        &mut self,
        exe: &Executable,
        chunks: &[(u64, u32)],
    ) -> Result<(Duration, Vec<Event>)> {
        let timeout = self.options.timeout;
        if self.queue.available() < chunks.len() as u32 {
            self.queue.reconcile(&self.device)?;
        }
        if self.queue.available() < chunks.len() as u32 {
            return Err(Error::HostQueue(format!(
                "{} slots free, {} chunks to queue",
                self.queue.available(),
                chunks.len()
            )));
        }
        let before = self.int_counts[0];
        let stale = regs::sc_host_int_count(self.device.read(regs::SC_HOST_INT_COUNT)?, 0);
        if stale != before {
            return Err(Error::Protocol(format!(
                "{}: completion counter moved from {before} to {stale} before the program was queued",
                exe.name
            )));
        }
        let start = Instant::now();
        for &(address, size) in chunks {
            self.queue.enqueue(&self.device, address, size)?;
        }
        let mut events = Vec::new();
        let mut spins = 0u32;
        loop {
            let now = regs::sc_host_int_count(self.device.read(regs::SC_HOST_INT_COUNT)?, 0);
            events.extend(self.device.events()?);
            if now.wrapping_sub(before) >= 1 {
                let elapsed = start.elapsed();
                self.int_counts[0] = before.wrapping_add(1);
                self.finish(exe, &mut events)?;
                return Ok((elapsed, events));
            }
            if let Some(fatal) = events.iter().find(|e| e.interrupt == interrupt::FATAL_ERR) {
                Self::check_hib(&self.device)?;
                return Err(Error::Protocol(format!(
                    "{}: {fatal} without a HIB error status",
                    exe.name
                )));
            }
            Self::check_hib(&self.device)?;
            if self.queue.status(&self.device)?.fatal_error != 0 {
                return Err(Error::HostQueue(format!(
                    "{}: the instruction queue reported a fatal error",
                    exe.name
                )));
            }
            if start.elapsed() > timeout {
                let state = describe(&self.device, &self.queue);
                return Err(Error::Timeout {
                    name: exe.name.clone(),
                    timeout,
                    state,
                });
            }
            spins += 1;
            if spins > 2000 {
                std::thread::sleep(Duration::from_micros(20));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// Acknowledges the completion interrupt and the instruction fetches.
    fn finish(&mut self, exe: &Executable, events: &mut Vec<Event>) -> Result<()> {
        let all = (1u64 << regs::SC_HOST_INTERRUPTS) - 1;
        // Status bits are write-zero-to-clear: keep every bit but interrupt 0.
        self.device.write(regs::SC_HOST_INT_STATUS, all & !1)?;
        let start = Instant::now();
        loop {
            if self.queue.reconcile(&self.device)? {
                break;
            }
            if start.elapsed() > Duration::from_millis(100) {
                let state = describe(&self.device, &self.queue);
                return Err(Error::HostQueue(format!(
                    "{}: the program completed but the instruction queue did not: {state}",
                    exe.name
                )));
            }
            std::thread::sleep(Duration::from_micros(20));
        }
        self.device.write(regs::INSTRUCTION_QUEUE_INT_STATUS, 0)?;
        events.extend(self.device.events()?);
        Ok(())
    }

    /// Pauses the DMAs, halts every unit, disables interrupts and releases the
    /// queue and the coherent block. The kernel resets the chip when the file closes.
    fn shutdown(&mut self, in_error: bool) -> Result<()> {
        let mut status = Ok(());
        let mut note = |result: Result<()>| {
            if let Err(error) = result
                && status.is_ok()
            {
                status = Err(error);
            }
        };
        note(self.device.write(regs::DMA_PAUSE, 1));
        note(
            self.device
                .poll(
                    "dma_paused",
                    regs::DMA_PAUSED,
                    1,
                    1,
                    Duration::from_millis(500),
                )
                .map(drop),
        );
        note(self.run_control(RunControl::Halt));
        note(self.device.write(regs::FATAL_ERR_INT_CONTROL, 0));
        note(self.device.write(regs::INSTRUCTION_QUEUE_INT_CONTROL, 0));
        note(self.device.write(regs::SC_HOST_INT_CONTROL, 0));
        note(self.queue.close(&self.device, &mut self.simple, in_error));
        note(self.device.release_coherent());
        self.closed = true;
        status
    }

    pub fn close(mut self) -> Result<()> {
        if self.live_parameters > 0 {
            return Err(Error::Protocol(format!(
                "{} parameter sets are still loaded",
                self.live_parameters
            )));
        }
        self.shutdown(false)
    }
}

impl Drop for Tpu {
    fn drop(&mut self) {
        if !self.closed {
            // Best effort: the kernel driver resets the chip on the last close anyway.
            let _ = self.shutdown(true);
        }
    }
}
