use super::{ATT_DECODE_HEAD_DIM, ATT_DECODE_MAX_KV, DeviceMemory, HipBackend, Q8Scratch, Q8_1_BLOCK, Result, Stream, hip_err};
use crate::kv_cache::KvCache;
use crate::model::decoder::{Decoder, EagerDecoder};
use crate::model::{Model, Workspace};
use crate::memory::{product, sum};
use rocm_rs::hip::bindings as hip;
use std::ptr;

pub(super) struct ReplayParams {
    pub mem: DeviceMemory<u32>,
    pub capacity: usize,
}

fn check(status: hip::hipError_t) -> Result<()> {
    if status == hip::hipError_t_hipSuccess {
        Ok(())
    } else {
        Err(hip_err(rocm_rs::hip::error::Error::new(status)))
    }
}

struct Graph(hip::hipGraph_t);

impl Drop for Graph {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 is non-null only when hipStreamEndCapture handed this Graph a
            // graph it alone owns (Graph is neither Clone nor Copy), so it is destroyed once.
            // An instantiated hipGraphExec_t keeps no reference to its source graph, so this
            // may run after hipGraphInstantiate.
            unsafe { hip::hipGraphDestroy(self.0) };
        }
    }
}

struct Capture<'a> {
    stream: &'a Stream,
    active: bool,
}

impl<'a> Capture<'a> {
    fn begin(stream: &'a Stream) -> Result<Self> {
        // SAFETY: `stream` is a live non-blocking (not legacy null) stream that this Capture
        // borrows for 'a; its backend was just created in GraphState::capture, so no other
        // thread can use it. ThreadLocal mode confines capture restrictions to this thread.
        // A successful begin is paired with exactly one end, in finish() or Drop (`active`).
        check(unsafe { hip::hipStreamBeginCapture(stream.as_raw(), hip::hipStreamCaptureMode_hipStreamCaptureModeThreadLocal) })?;
        Ok(Self { stream, active: true })
    }

    fn finish(mut self) -> Result<Graph> {
        let mut graph = Graph(ptr::null_mut());
        // SAFETY: ends the capture begun on this same stream (`active` is still true, and
        // finish consumes self so it cannot run twice). &mut graph.0 is a valid out-param;
        // on success the graph is owned by `graph` and destroyed in its Drop, on failure
        // HIP leaves it null, which Graph::drop skips.
        let status = unsafe { hip::hipStreamEndCapture(self.stream.as_raw(), &mut graph.0) };
        self.active = false;
        check(status)?;
        Ok(graph)
    }
}

impl Drop for Capture<'_> {
    fn drop(&mut self) {
        if self.active {
            let mut graph = Graph(ptr::null_mut());
            // SAFETY: a capture that was never finished (e.g. unwinding out of
            // forward_device) is still active on self.stream, which the 'a borrow keeps
            // alive; ending it takes the stream out of capture mode, and any partial graph
            // lands in `graph` and is destroyed when it drops. The status is ignored because
            // the capture is being abandoned.
            unsafe { hip::hipStreamEndCapture(self.stream.as_raw(), &mut graph.0) };
        }
    }
}

struct Executable(hip::hipGraphExec_t);

impl Drop for Executable {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 is non-null only after hipGraphInstantiate succeeded into this
            // Executable, its sole owner, so it is destroyed once. GraphState::drop
            // synchronizes the replay stream before its fields drop, and `executable` is
            // declared before `backend`, so no launch is in flight and the module holding the
            // graph's kernels is still loaded.
            unsafe { hip::hipGraphExecDestroy(self.0) };
        }
    }
}

// Private: a captured state must immediately be wrapped in HipDecoder's borrows.
struct GraphState {
    executable: Executable,
    backend: HipBackend,
}

impl GraphState {
    fn capture(eager: &HipBackend, model: &Model<HipBackend>, kv: &mut KvCache<HipBackend>, ws: &mut Workspace<HipBackend>) -> Result<Self> {
        model.validate_decode(kv, ws, 0, 0)?;
        let mut backend = HipBackend::new()?;
        backend.buffers = eager.buffers.clone();
        backend.profile_owner = false;
        backend.health = eager.health.clone();
        backend.replay = Some(ReplayParams { mem: DeviceMemory::new(2, &backend.buffers)?, capacity: kv.capacity() });
        let width = model.config().n_embd.max(model.config().n_ff);
        let cap = sum([width, product(&[width.div_ceil(Q8_1_BLOCK as usize), size_of::<[f32; 2]>()])?])?;
        *backend.q8_scratch.get_mut().unwrap() = Q8Scratch {
            mem: Some(DeviceMemory::new(cap, &backend.buffers)?), cap, cached: None,
        };
        backend.reserve_attn_scratch(HipBackend::attn_partials_bytes(model.config().n_heads, kv.capacity())?, model.config().n_kv_heads)?;
        // Finish any previous prefill/default-stream writes before capture/replay.
        rocm_rs::hip::device_synchronize().map_err(hip_err)?;
        let saved_len = kv.len();
        let capture = Capture::begin(backend.stream.as_ref().unwrap())?;
        let recorded = model.forward_device(&backend, kv, ws, 0, 0);
        let captured = capture.finish();
        kv.set_len(saved_len);
        recorded?;
        let graph = captured?;
        let mut executable = Executable(ptr::null_mut());
        // SAFETY: graph.0 is the graph a successful hipStreamEndCapture just returned
        // (`captured?` above); &mut executable.0 is a valid out-param, owned by Executable
        // from here on; the error-node and log outputs are optional (null, size 0). The
        // captured kernels embed raw pointers to model/kv/ws buffers and to this backend's
        // module, params and pre-reserved scratch, which forward() must keep alive.
        check(unsafe { hip::hipGraphInstantiate(&mut executable.0, graph.0, ptr::null_mut(), ptr::null_mut(), 0) })?;
        Ok(Self { executable, backend })
    }
}

impl Drop for GraphState {
    fn drop(&mut self) {
        // Also covers an error after enqueue but before the normal logits join.
        let _ = self.backend.stream.as_ref().unwrap().synchronize();
    }
}

struct HipDecoder<'a> {
    state: GraphState,
    model: &'a Model<HipBackend>,
    kv: &'a mut KvCache<HipBackend>,
    ws: &'a mut Workspace<HipBackend>,
}

impl Decoder for HipDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.model.validate_decode(self.kv, self.ws, token, pos)?;
        let params = [token, pos];
        let backend = &self.state.backend;
        let stream = backend.stream.as_ref().unwrap();
        // SAFETY: copies size_of_val(&params) == 8 bytes (two u32) into replay.mem, the DeviceMemory<u32> of
        // count 2 allocated in capture() (replay is always Some on a graph backend). `params` stays on this frame,
        // and once the copy is enqueued every path syncs the stream before returning (download_logits -> download,
        // or the error branch below), so the host source outlives the async copy. Kernels read params[0] as the
        // token and params[1] as the position, both bounded by validate_decode above; `&mut self` makes this the
        // only user of the stream.
        check(unsafe {
            hip::hipMemcpyAsync(
                backend.replay.as_ref().unwrap().mem.as_ptr(),
                params.as_ptr().cast(), size_of_val(&params), hip::hipMemcpyKind_hipMemcpyHostToDevice,
                stream.as_raw(),
            )
        })?;
        // SAFETY: the executable came from a capture on this same stream, and all it references stays pinned while
        // this decoder exists: weights via &'a Model, KV/workspace buffers via HipDecoder's exclusive &'a mut borrows
        // (forward only calls kv.set_len), and the module, params and q8/attention scratch owned by state.backend,
        // reserved before capture and never regrown (growth starts with hipDeviceSynchronize, which HIP rejects
        // during capture). Device-side indexing by params is bounded by validate_decode and the capacity checks
        // rope_kv_write/kv_write ran at capture time.
        let result = check(unsafe { hip::hipGraphLaunch(self.state.executable.0, stream.as_raw()) })
            .and_then(|()| self.model.download_logits(backend, self.ws));
        if result.is_err() {
            let _ = backend.health.check(stream.synchronize().map_err(hip_err));
        }
        let logits = result?;
        self.kv.set_len(pos as usize + 1);
        Ok(logits)
    }

    fn is_graph(&self) -> bool {
        true
    }
}

pub(super) fn decoder<'a>(
    backend: &'a HipBackend, model: &'a Model<HipBackend>,
    kv: &'a mut KvCache<HipBackend>, ws: &'a mut Workspace<HipBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    // Fixed head dimension and bounded LDS are the initially validated path.
    let enabled = backend.graphs_enabled;
    if enabled && model.supports_graph_decode() && !backend.f32_compute && model.config().head_dim == ATT_DECODE_HEAD_DIM
        && (1..=ATT_DECODE_MAX_KV).contains(&kv.capacity()) {
        match GraphState::capture(backend, model, kv, ws) {
            Ok(state) => return Ok(Box::new(HipDecoder { state, model, kv, ws })),
            Err(error) => eprintln!("mdma: HIP graph capture unavailable, using eager decode: {error}"),
        }
    }
    Ok(Box::new(EagerDecoder { backend, model, kv, ws }))
}

#[cfg(test)]
mod tests;
