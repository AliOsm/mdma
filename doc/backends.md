# Backend build details and execution modes

## Build details

The HIP kernels (`src/backend/rocm/kernels/kernels.hip`) are compiled by
`build.rs` with `hipcc` and embedded into the binary.

Vulkan's elementwise, embedding and RoPE launch sizes and unary-operation tags
live in `src/backend/vulkan/abi.rs`; dtype tags live in
`src/backend/dtype_code/tags.rs`. `build.rs` passes them to GLSL as compiler
defines, so host dispatch and shader compilation use the same values.

Metal uses `objc2-metal` and compiles embedded `src/backend/metal/kernels.metal`
source at backend startup, caching the compute pipelines. It does not need
MLX, Python, or the standalone `xcrun metal` compiler. Building Rust's native
dependencies still requires the macOS Command Line Tools and CMake.
Metal's host/shader constants live in `src/backend/metal/abi.rs`; shader
dtype defines come from the shared `dtype_code/tags.rs`.
Public backend operations and `Model::forward` are synchronous.
`Backend::decoder` creates a private queue/batch for each request, retaining
temporary buffers and joining GPU work before logits downloads or errors.
It is command batching, not graph replay. Set `MDMA_METAL_BATCH=0` to use
the eager decoder instead. Serving and `mdma-run` use the decoder interface.

On Apple-family GPUs supporting the matrix path, batches of at least eight
tokens use 32-row × 8-token tiles with f32 staging and accumulation.
Decode and small matrices retain the SIMD-group matvec path.
Set `MDMA_METAL_TILE=0` before backend initialization to disable tiling.
Llama prefill is batched; Gemma prefill remains sequential.
Unsupported weight types have no CPU fallback.
RoPE frequencies are cached, host-rounded model constants; rotations and
all activation computation stay on Metal.
Binary elementwise kernels accept extra output capacity without modifying
the unused tail, matching the CPU buffer contract.
The feature has no Apple dependencies on non-macOS targets, where selecting
`MDMA_BACKEND=metal` reports an explicit platform error.

## Execution modes

`Session` uses graph replay on supported HIP decode configurations.
Set `MDMA_HIP_GRAPHS=0` to use the eager fallback. Library callers can use
`Backend::decoder`; direct `Model::forward` remains eager.
`HipBackend::new_f32()` uses f32 activations and KV with eager GPU kernels.
It trades throughput and twice the KV storage for reference accuracy;
`HipBackend::new()` keeps the existing fast path.

On CUDA, `MDMA_CUDA_INT8` picks the K-quant int8 paths: `prefill`
(default, the int8 GEMM from 2 tokens up on sm_80+), `all` (plus the
dp4a decode matvec) or `off` (f32 activations only). CUDA decode replays
a captured graph per token; `MDMA_CUDA_GRAPHS=0` uses the eager path.

For Vulkan, `MDMA_VK_F16_WEIGHTS=1` stages prefill weights as f16; anything
else keeps f32. `VulkanBackend::calibrate_f16` measures speed and logit drift
for a model when you want to decide. FP16 arithmetic and its staging
pipeline are optional: unsupported devices retain f32 staging, even with `1`.
Packed f16 model weights remain supported without FP16 arithmetic.
`MDMA_VK_PROFILE=1` flushes each operation
separately for wall-time profiling.

Vulkan fuses residual addition with RMSNorm by default. Eligible prefill
attention uses f32 K/V tiles shared across eight queries, without matrix
instructions. `MDMA_VK_FUSED_NORM=0` and `MDMA_VK_TILED_ATTN=0` restore the
separate/original kernels. Decode attention and heads wider than 128 keep
the original path.
See [selection, correctness gates and benchmarking](vulkan-fusion.md).

`MDMA_VK_INT8=1` opts into experimental Q4_K/Q6_K int8 prefill: cooperative
matrices on RADV RDNA3/4, otherwise accelerated packed dot products
(RDNA2-compatible). `MDMA_VK_INT8=dot` forces the packed-dot path.
It requantizes activations to Q8_1; decode and other weight types keep
their existing paths. It overrides f16 staging and skips its load-time
calibration; unsupported devices fall back. RDNA2 hardware validation
is pending.
Int8 remains off by default.
See [validation, limits and attribution](vulkan-int8.md).

Library users can opt into `Session::with_prefix_reuse()`. It is off by
default and not enabled by the gRPC server. Reuse a session only within a
single trust boundary: cache-hit timing can reveal prompt reuse, and
cached prompt KV remains resident. Each session retains at most one idle
cache, bounded by `n_ctx`; concurrent requests use separate caches.
`Session::clear_prefix_cache()` evicts the idle cache and prevents
in-flight requests from restoring it. This is session-local reuse, not
shared KV pages or cross-request batching.

K-quant decode and prefill use Q8_1 activations: f16-defined scales and
exact integer block sums, with FP32 accumulation. Activation rounding
differs from the FP32 reference; requantized-reference tests and the stored
MiniCheck greedy-continuation gates pass, without claiming parity on every prompt.

## Strict ROCm reference mode

For TinyLlama and Gemma, `mdma-run` also accepts `MDMA_BACKEND=rocm` or `vulkan`
when built with the matching feature. For strict reference accuracy on ROCm:

```sh
MDMA_BACKEND=rocm MDMA_HIP_F32=1 cargo run --release --features rocm --bin mdma-run -- \
  /path/to/gemma-4-E2B-it-Q4_K_M.gguf "The capital of France is" 16
```

`mdma-serve` accepts the same `MDMA_HIP_F32=0|1` switch and freezes the selection at
startup; see the [federation runtime](federation/routing-runtime.md#deployment-notes).
