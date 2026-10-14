//! Runs programs on one Edge TPU over PCIe. `open` follows libedgetpu's MMIO driver:
//! the kernel has already taken the chip out of reset, so user space checks the
//! host interface, partitions the page table, sets up the instruction queue, moves
//! every unit to run and enables interrupts. `run` maps the program's buffers,
//! links their device addresses into the instruction chunks, queues the chunks and
//! waits for scalar-core interrupt 0, which every program raises when it ends.

use super::address_space::{
    AddressSpace, PAGE, PAGE_TABLE_ENTRIES, extended_entries, round_up_to_page, simple_entries,
};
use super::executable::{Description, Executable, Hint, Layer};
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
    /// Every page is written once here: Linux gasket pins the pages of a host-to-device
    /// buffer read-only, and a page the process never touched is pinned as the shared
    /// zero page, so writes made after mapping would never reach the chip.
    pub fn zeroed(bytes: usize) -> Self {
        let len = round_up_to_page(bytes as u64) as usize;
        let mut storage = vec![0u8; len + PAGE as usize];
        let start = (PAGE as usize - storage.as_ptr() as usize % PAGE as usize) % PAGE as usize;
        for page in storage[start..start + len].chunks_mut(PAGE as usize) {
            // An opaque value: the store must happen even though the memory is zero.
            page[0] = std::hint::black_box(0);
        }
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

/// One input or output buffer of a loaded program.
struct Io {
    name: String,
    len: usize,
    mapped: Mapped,
}

impl Io {
    fn new(layer: &Layer, mapped: Mapped) -> Self {
        Self {
            name: layer.name.clone(),
            len: layer.size_bytes,
            mapped,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.mapped.buf.as_slice()[..self.len]
    }
}

/// A program with its buffers mapped and its chunks linked: the host writes the
/// inputs in place, may patch instruction fields, invokes it and reads the outputs.
pub struct Loaded {
    name: String,
    inputs: Vec<Io>,
    outputs: Vec<Io>,
    scratch: Option<Mapped>,
    chunks: Vec<(Mapped, u32)>,
}

impl Loaded {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The input's bytes, the layer's padded size; the chip reads them at `invoke`.
    pub fn input_mut(&mut self, name: &str) -> Result<&mut [u8]> {
        let io = self
            .inputs
            .iter_mut()
            .find(|io| io.name == name)
            .ok_or_else(|| Error::Executable(format!("{}: has no input {name:?}", self.name)))?;
        Ok(&mut io.mapped.buf.as_mut_slice()[..io.len])
    }

    /// The output's bytes as the last `invoke` left them.
    pub fn output(&self, name: &str) -> Result<&[u8]> {
        self.outputs
            .iter()
            .find(|io| io.name == name)
            .map(Io::bytes)
            .ok_or_else(|| Error::Executable(format!("{}: has no output {name:?}", self.name)))
    }

    /// Linked instruction chunk `index`, to patch immediates between invokes.
    pub fn chunk_mut(&mut self, index: usize) -> Result<&mut [u8]> {
        let name = &self.name;
        self.chunks
            .get_mut(index)
            .map(|(mapped, size)| &mut mapped.buf.as_mut_slice()[..*size as usize])
            .ok_or_else(|| Error::Executable(format!("{name}: has no instruction chunk {index}")))
    }
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
    live_mappings: usize,
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
            live_mappings: 0,
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
        self.live_mappings += 1;
        Ok(Parameters {
            mapped,
            len: bytes.len(),
            token,
        })
    }

    /// Maps `blob` and runs `caching`, which copies it into the tiles' wide memory;
    /// the parameters stay mapped for the execution programs that read them.
    pub fn cache_parameters(&mut self, caching: &Executable, blob: &[u8]) -> Result<Parameters> {
        let parameters = self.load_parameters(blob, caching.caching_token)?;
        match self.run(caching, Some(&parameters), &[]) {
            Ok(_) => Ok(parameters),
            Err(error) => match self.unload_parameters(parameters) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Protocol(format!(
                    "{error}; unmapping afterwards also failed: {cleanup}"
                ))),
            },
        }
    }

    pub fn unload_parameters(&mut self, parameters: Parameters) -> Result<()> {
        self.live_mappings -= 1;
        self.unmap(parameters.mapped).map(drop)
    }

    /// Runs `exe` once with `inputs` (name, bytes of the layer's padded size).
    pub fn run(
        &mut self,
        exe: &Executable,
        parameters: Option<&Parameters>,
        inputs: &[(&str, &[u8])],
    ) -> Result<RunOutput> {
        if let Some((name, _)) = inputs.iter().find(|(name, _)| exe.input(name).is_none()) {
            return Err(Error::Executable(format!(
                "{}: has no input {name:?}",
                exe.name
            )));
        }
        let mut program = self.load(exe, parameters)?;
        let outcome = Self::write_inputs(exe, &mut program, inputs).and_then(|()| {
            let (elapsed, events) = self.execute(&program)?;
            let outputs = program
                .outputs
                .iter()
                .map(|io| (io.name.clone(), io.bytes().to_vec()))
                .collect();
            Ok(RunOutput {
                outputs,
                elapsed,
                events,
            })
        });
        match (outcome, self.unload(program)) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(Error::Protocol(format!(
                "{error}; unmapping afterwards also failed: {cleanup}"
            ))),
        }
    }

    fn write_inputs(
        exe: &Executable,
        program: &mut Loaded,
        inputs: &[(&str, &[u8])],
    ) -> Result<()> {
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
            program.input_mut(&layer.name)?.copy_from_slice(bytes);
        }
        Ok(())
    }

    /// Maps `exe`'s buffers and links its instruction chunks once, for any number
    /// of `invoke`s; `unload` releases it. `parameters` must stay loaded meanwhile.
    pub fn load(&mut self, exe: &Executable, parameters: Option<&Parameters>) -> Result<Loaded> {
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
        if exe.batch_size != 1 {
            return Err(Error::Executable(format!(
                "{}: batch size {} is not supported yet",
                exe.name, exe.batch_size
            )));
        }
        let mut program = Loaded {
            name: exe.name.clone(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            scratch: None,
            chunks: Vec::new(),
        };
        let mut addresses = Addresses {
            parameter: parameters.map(Parameters::device_address),
            ..Addresses::default()
        };
        match self.map_program(exe, &mut addresses, &mut program) {
            Ok(()) => {
                self.live_mappings += 1;
                Ok(program)
            }
            Err(error) => match self.release(program) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Protocol(format!(
                    "{error}; unmapping afterwards also failed: {cleanup}"
                ))),
            },
        }
    }

    /// Queues `program`'s chunks and waits for it to finish; its outputs are then
    /// in `program.output`.
    pub fn invoke(&mut self, program: &Loaded) -> Result<Duration> {
        self.execute(program).map(|(elapsed, _)| elapsed)
    }

    pub fn unload(&mut self, program: Loaded) -> Result<()> {
        self.live_mappings -= 1;
        self.release(program)
    }

    fn release(&mut self, program: Loaded) -> Result<()> {
        let Loaded {
            inputs,
            outputs,
            scratch,
            chunks,
            ..
        } = program;
        let mapped = outputs
            .into_iter()
            .chain(inputs)
            .map(|io| io.mapped)
            .chain(scratch)
            .chain(chunks.into_iter().map(|(m, _)| m))
            .collect();
        self.unmap_all(mapped).map(drop)
    }

    /// Maps outputs, inputs and scratch, then the linked instruction chunks.
    fn map_program(
        &mut self,
        exe: &Executable,
        addresses: &mut Addresses,
        program: &mut Loaded,
    ) -> Result<()> {
        for (layers, direction, at, ios) in [
            (
                &exe.outputs,
                DmaDirection::FromDevice,
                &mut addresses.outputs,
                &mut program.outputs,
            ),
            (
                &exe.inputs,
                DmaDirection::ToDevice,
                &mut addresses.inputs,
                &mut program.inputs,
            ),
        ] {
            for layer in layers {
                let m = self.map(PageBuf::zeroed(layer.size_bytes), direction)?;
                at.insert(layer.name.clone(), vec![m.device]);
                ios.push(Io::new(layer, m));
            }
        }
        if exe.scratch_size > 0 {
            let m = self.map(
                PageBuf::zeroed(exe.scratch_size),
                DmaDirection::Bidirectional,
            )?;
            addresses.scratch = Some(m.device);
            program.scratch = Some(m);
        }
        for bitstream in &exe.bitstreams {
            let linked = link(bitstream, addresses)?;
            let size = u32::try_from(linked.len()).map_err(|_| {
                Error::Executable(format!("{}: instruction chunk too large", exe.name))
            })?;
            let m = self.map(PageBuf::from_bytes(&linked), DmaDirection::ToDevice)?;
            program.chunks.push((m, size));
        }
        Ok(())
    }

    /// Queues the chunks and waits for scalar-core interrupt 0. Completion is the
    /// chip's own interrupt counter moving by one; the kernel's events are only
    /// collected, since they can arrive before or after the counter and a run
    /// reported done early would unmap buffers the chip is still writing.
    fn execute(&mut self, program: &Loaded) -> Result<(Duration, Vec<Event>)> {
        let timeout = self.options.timeout;
        let chunks = &program.chunks;
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
                program.name
            )));
        }
        let start = Instant::now();
        for (mapped, size) in chunks {
            self.queue.enqueue(&self.device, mapped.device, *size)?;
        }
        let mut events = Vec::new();
        let mut spins = 0u32;
        loop {
            let now = regs::sc_host_int_count(self.device.read(regs::SC_HOST_INT_COUNT)?, 0);
            events.extend(self.device.events()?);
            if now.wrapping_sub(before) >= 1 {
                let elapsed = start.elapsed();
                self.int_counts[0] = before.wrapping_add(1);
                self.finish(&program.name, &mut events)?;
                return Ok((elapsed, events));
            }
            if let Some(fatal) = events.iter().find(|e| e.interrupt == interrupt::FATAL_ERR) {
                Self::check_hib(&self.device)?;
                return Err(Error::Protocol(format!(
                    "{}: {fatal} without a HIB error status",
                    program.name
                )));
            }
            Self::check_hib(&self.device)?;
            if self.queue.status(&self.device)?.fatal_error != 0 {
                return Err(Error::HostQueue(format!(
                    "{}: the instruction queue reported a fatal error",
                    program.name
                )));
            }
            if start.elapsed() > timeout {
                let state = describe(&self.device, &self.queue);
                return Err(Error::Timeout {
                    name: program.name.clone(),
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
    fn finish(&mut self, name: &str, events: &mut Vec<Event>) -> Result<()> {
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
                    "{name}: the program completed but the instruction queue did not: {state}"
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
        if self.live_mappings > 0 {
            return Err(Error::Protocol(format!(
                "{} parameter sets or programs are still loaded",
                self.live_mappings
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
