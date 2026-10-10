//! Private request-owned encoding. Public Backend operations remain synchronous.
use super::*;
use crate::{kv_cache::KvCache, model::{Model, Workspace, decoder::Decoder}};
use objc2_metal::MTLBarrierScope;
use std::sync::Mutex;

pub(super) type Pending = Mutex<Option<Batch>>;

pub(super) struct Batch {
    command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    encoder: Option<Retained<ProtocolObject<dyn MTLComputeCommandEncoder>>>,
    // Retain temporary embedding IDs/constants, and identify allocations that
    // a subsequent host upload must not modify until the batch completes.
    buffers: HashMap<usize, Retained<ProtocolObject<dyn MTLBuffer>>>,
}

// SAFETY: command buffers/encoders may move between threads but must not be
// encoded concurrently. This private value has exclusive access (or is under
// Pending's mutex); retained resources are accessed only by the owning request.
#[expect(clippy::non_send_fields_in_send_ty, reason = "justified by the SAFETY comment above")]
unsafe impl Send for Batch {}

fn key(buffer: &MetalBuf) -> usize {
    Retained::as_ptr(&buffer.buffer) as usize
}

impl Batch {
    pub(super) fn new(queue: &ProtocolObject<dyn MTLCommandQueue>) -> Result<Self> {
        let command = queue.commandBuffer()
            .ok_or_else(|| Error::Backend("Metal command buffer allocation failed".into()))?;
        let encoder = command.computeCommandEncoder()
            .ok_or_else(|| Error::Backend("Metal compute encoder allocation failed".into()))?;
        Ok(Self { command, encoder: Some(encoder), buffers: HashMap::new() })
    }

    pub(super) fn references(&self, buffer: &MetalBuf) -> bool {
        self.buffers.contains_key(&key(buffer))
    }

    /// # Safety
    ///
    /// The caller validates every shader buffer/scalar/dispatch contract.
    pub(super) unsafe fn encode<P: bytemuck::Pod>(
        &mut self,
        pipeline: &ProtocolObject<dyn MTLComputePipelineState>,
        buffers: &[&MetalBuf],
        params: &P,
        groups: MTLSize,
        threads: MTLSize,
    ) {
        let encoder = self.encoder.as_ref().expect("open Metal encoder");
        // A serial compute encoder orders dispatches. This also expresses the
        // dependency explicitly if the encoder's dispatch policy ever changes.
        encoder.memoryBarrierWithScope(MTLBarrierScope::Buffers);
        encoder.setComputePipelineState(pipeline);
        for (index, buf) in buffers.iter().enumerate() {
            self.buffers.entry(key(buf)).or_insert_with(|| buf.buffer.clone());
            // SAFETY: caller checked accesses; retained allocations survive completion.
            unsafe { encoder.setBuffer_offset_atIndex(Some(&buf.buffer), 0, index) };
        }
        // SAFETY: setBytes copies the caller's POD scalar ABI during encoding.
        unsafe {
            encoder.setBytes_length_atIndex(NonNull::from(params).cast(), std::mem::size_of::<P>(), buffers.len());
        }
        encoder.dispatchThreadgroups_threadsPerThreadgroup(groups, threads);
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.encoder.take().expect("open Metal encoder").endEncoding();
        self.command.commit();
        self.command.waitUntilCompleted();
        if self.command.status() != MTLCommandBufferStatus::Completed {
            let detail = self.command.error().map_or_else(|| format!("status {:?}", self.command.status()), |e| e.to_string());
            return Err(Error::Backend(format!("Metal command batch: {detail}")));
        }
        Ok(())
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // On panic, cancel unsubmitted work rather than launch it after the
        // exclusive request borrows are released. Submitted work is joined in finish.
        if let Some(encoder) = self.encoder.take() {
            encoder.endEncoding();
        }
    }
}

pub(super) struct BatchedDecoder<'a> {
    backend: MetalBackend,
    model: &'a Model<MetalBackend>,
    kv: &'a mut KvCache<MetalBackend>,
    ws: &'a mut Workspace<MetalBackend>,
}

impl<'a> BatchedDecoder<'a> {
    pub(super) fn new(
        backend: &MetalBackend,
        model: &'a Model<MetalBackend>,
        kv: &'a mut KvCache<MetalBackend>,
        ws: &'a mut Workspace<MetalBackend>,
    ) -> Result<Self> {
        Ok(Self { backend: backend.request_backend()?, model, kv, ws })
    }
}

impl Decoder for BatchedDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.model.validate_decode(self.kv, self.ws, token, pos)?;
        let result = self.model.forward(&self.backend, self.kv, self.ws, token, pos);
        // Join partial work on errors too, before returning request buffers.
        self.backend.finish()?;
        result
    }
}

impl MetalBackend {
    // Only BatchedDecoder may expose this backend to model operations. It
    // exclusively borrows KV/workspace, and shares only immutable model weights.
    pub(super) fn request_backend(&self) -> Result<Self> {
        let queue = self.device.newCommandQueue()
            .ok_or_else(|| Error::Backend("Metal request queue allocation failed".into()))?;
        Ok(Self {
            device: self.device.clone(),
            queue,
            pipelines: self.pipelines.clone(),
            rope_freqs: self.rope_freqs.clone(),
            pending: Some(Mutex::new(None)),
            tile_prefill: self.tile_prefill,
            batch_decode: self.batch_decode,
            shader_id: self.shader_id.clone(),
            health: self.health.clone(),
            identity: self.identity.clone(),
        })
    }

    pub(super) fn finish(&self) -> Result<()> {
        self.health.check((|| {
        if let Some(pending) = &self.pending {
            let batch = pending.lock()
                .map_err(|_| Error::Backend("Metal batch lock poisoned".into()))?.take();
            if let Some(batch) = batch {
                return self.health.check(autoreleasepool(|_| batch.finish()));
            }
        }
        Ok(())
        })())
    }

    pub(super) fn before_upload(&self, buffer: &MetalBuf) -> Result<()> {
        if let Some(pending) = &self.pending {
            let referenced = pending.lock()
                .map_err(|_| Error::Backend("Metal batch lock poisoned".into()))?
                .as_ref().is_some_and(|batch| batch.references(buffer));
            if referenced { self.finish()?; }
        }
        Ok(())
    }
}
