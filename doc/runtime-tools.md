# Runtime tools and layer offloading

Build the GPU backend you intend to use; CPU is always available:

```sh
cargo build --release --features vulkan --bin mdma
mdma info --json
mdma doctor --backend vulkan --json
mdma plan model.gguf --backend vulkan --ctx 512 --prompt-tokens 32 --gpu-layers auto --json
mdma run model.gguf "Hello" --backend vulkan --gpu-layers auto --max-tokens 32 --json
mdma bench --backend vulkan --size-mib 16 --iterations 8 --json
```

Commands use the same native Linux, FreeBSD and macOS inventory. Hardware
inventory requires no GPU SDK. Runtime GPU commands require their compiled
feature and a working driver; they do not silently fall back to CPU.
Backend names are `cpu`, `vulkan`, `rocm`, `cuda`, `metal` and `level0`.

ROCm embeds kernels for one architecture, defaulting to `gfx1201` (RDNA4).
For an RX 6800 XT (`gfx1030`, RDNA2), build with:

```sh
MDMA_HIP_OFFLOAD_ARCH=gfx1030 cargo build --release --features rocm,vulkan --bin mdma
mdma doctor --backend rocm --json
```

RDNA2 automatically uses portable GPU GEMV with F32 activations and KV,
without RDNA4 WMMA, activation quantization or graph replay. Quantized weights
remain quantized. This is a correctness-first path, not an RDNA2-tuned prefill
kernel; Vulkan remains available. HIP compilation requires the ROCm SDK;
execution requires a compatible ROCm runtime and access to `/dev/kfd`.

## Doctor

`doctor` checks all compiled backends by default. Each receives a tiny
allocation/upload/copy/F32 identity-matrix/download test. Reports include the
selected device, unified-memory status, KV element size, attention head-width
limit, prepared-cache profile and optional decoder operations.

`ready` means that smoke test passed, **not** that every model is supported.
`not_built` includes rebuild advice. A failed compiled backend or an explicitly
requested unbuilt backend makes the command fail. Missing inventory utilities
are reported as warnings, not backend failures.

## Plan

`plan MODEL --ctx N --prompt-tokens N` reads metadata and tensor descriptors,
validates the architecture/layout and file extents, and estimates native
weight, KV and prefill/decode workspace buffers. It does not upload model
weights or run inference. GPU initialization can create driver resources.
Context defaults to 512; prefill tokens default to min(32, context).

Without `--gpu-layers`, estimates describe ordinary execution wholly on the
selected backend, including the existing sequential-prefill architectures.
With `--gpu-layers N|auto`, the report separates CPU/GPU weights, KV and
workspace plus the activation bridge and loading staging.

Available RAM comes from the OS inventory. Live GPU free-memory snapshots use
HIP, CUDA or Vulkan's optional `VK_EXT_memory_budget`. Metal and other unified
backends can use the RAM snapshot. If a discrete driver exposes no live budget,
supply `--vram-bytes N`; advertised total VRAM is not a free-memory measurement.
`--ram-bytes N` also accepts an explicit RAM budget. `--reserve-mib` defaults
to 256 MiB device headroom. Failed fits return a nonzero status; ordinary
nonfits still print their report, including `"fits": false` in JSON.

These are **logical tensor estimates**, not total RSS or reservations. They
exclude allocator padding, tokenizer storage, CPU dequantization temporaries,
driver/backend scratch and graph-owned resources. Leave additional RAM/device
headroom for these and for other applications. Unified GPU memory shares one
RAM pool: RAM and a GPU allowance are never added as independent capacity.
Native budget enforcement in serving is unchanged.

## Mixed execution

Initial layer placement supports **Llama and InternLM2 only**. Other native
architectures remain usable without `--gpu-layers`; hybrid requests fail
explicitly rather than using a partial fallback.

- CPU holds embeddings, output normalization/projection and a contiguous prefix
  of transformer layers. The GPU holds the suffix.
- Each domain retains its own weights and KV cache. There is no per-token weight
  streaming and no claim that RAM and VRAM become one flat allocation.
- Only hidden activations cross host→device and device→host once per prefill
  batch/decode step. Prefill scratch is dropped before decode scratch is allocated.
- Zero offloaded layers allocate no device model/KV/workspace buffers; backend
  initialization still happens. Offloading all transformer layers still leaves
  embeddings and output projection on CPU.
- `auto` selects the **largest executable suffix that fits**, not necessarily
  the fastest split. Host projection and transfers can dominate latency.
- Hybrid execution is eager. Ordinary execution keeps the existing backend
  decoder/graph selection.

`mdma run` validates the request and checks estimated budgets before loading.
Its context capacity is prompt tokens + requested output tokens. It uses the
raw GGUF source so its layout matches `mdma plan`; environment-selected prepared
caches are not attached. Serving and ordinary `mdma-run` retain their cache
policies.

The compatibility runner still accepts `mdma-run MODEL PROMPT [MAX_TOKENS]`.
`MDMA_JSON=1` prints prompt IDs, generated IDs and decoded text.
`MDMA_GPU_LAYERS=N` opts into an explicit hybrid suffix there. Automatic placement
and RAM/device budget preflight are provided by `mdma run`, not that smoke runner.
Library APIs live in `model::planning`, `model::hybrid` and `inference`.

## Bounded memory benchmarks

`bench` measures a RAM copy. Selecting a GPU additionally measures pageable
host→device transfer, device copy and device→host transfer. Defaults are 16 MiB
per buffer and eight iterations; hard limits are 256 MiB and 32 iterations.
Buffers are checked against available/explicit budgets before allocation.
GPU checks conservatively allow one staging buffer in each memory domain,
in addition to device headroom; these remain estimates, not reservations.

Allocation, initialization, warmup and full byte verification are outside
timing. Completion is inside timing: queued device copies are fenced by a
blocking download. Values are **decimal payload GB/s**, not doubled read/write
bus traffic. Pageable transfers include backend staging overhead.

Small RAM buffers can fit in CPU caches; these results are not automatically
DRAM bandwidth. Unified GPU copies are not separate VRAM capacity or PCIe
bandwidth measurements. This is a short copy benchmark, not a stress test or
an inference-throughput predictor.
Level Zero's current shared-buffer copy APIs use CPU memcpy; their copy numbers
are not GPU copy-engine throughput.
