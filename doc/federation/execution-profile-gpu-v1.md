# Native GPU execution profiles

These loader-created vocabularies extend `mdma.execution-profile.v1` for frozen
GGUF decoders on the concrete Metal, ROCm and Vulkan backends. They do not
authorize export/execution, attest a remote machine, or guarantee bit-identical
results across GPUs. CPU Laya retains its CPU profile.

The canonical manifest includes the actual whole running executable SHA-256,
backend, activation/KV dtype and string-valued modes. Its canonical SHA-256 is
the exact `profile_id` used by catalogs, imports, routes and execution. Uploaded
resources and profiles are bound to the same backend instance. Labels, build
environment claims and arbitrary implementations of `Backend` cannot create
native GPU provenance. Frozen loads consume original GGUF representations, not
prepared caches.

## Backend vocabularies

- **`mdma.metal.v1`**: actual Metal registry ID/name and unified-memory property;
  OS/kernel runtime versions; SHA-256 of the exact compiled shader source,
  including selected matrix definitions; created pipeline names and actual
  thread geometry. Metal 2.4 disables fast math. Activations/KV/norms are f32;
  quantized matrices retain GGUF dtype. Captured execution mode is eager joined
  operations or request-owned command batches, with the resolved tiled-prefill
  switch and geometry-dependent eligibility. Attention uses online causal f32;
  RoPE frequencies use host f32 and GPU operations.
- **`mdma.rocm.v1`**: actual device architecture, PCI/UUID/name, HIP runtime and
  driver versions; built kernel target and SHA-256 of the loaded embedded HSACO.
  Stored activations are f32. The concrete instance selects f32 reference
  computation/KV or RDNA4 f16/Q8_1/WMMA computation with f16 KV. The captured graph policy is eager or
  eligible request-owned graph capture with eager fallback; model/context
  geometry selects eligibility. Attention selects flash or generic kernels by
  geometry. A graph is an execution optimization, not another peer or retry.
- **`mdma.vulkan.v1`**: actual physical-device/vendor/UUID, driver ID/version/name/
  information/UUID and API version; SHA-256 for each created SPIR-V pipeline.
  Activations/KV/norms are f32. Captured resolved modes include f16 staging,
  int8 strategy, fused norm, tiled attention and actual device/staging memory
  types. Commands are recorded and fence-joined.

All include compiled OS/architecture. Kernel implementation changes also change
the executable digest. Runtime identifiers are queried locally; driver/shared
library bytes, root/kernel state and other processes are **not attested**.
Metal/HIP batch/graph switches are fixed at instance construction, rather than
re-read from the environment during delegated requests.

## Currentness and completion

Capture occurs after load, calibration and warm-up. Vulkan calibration setters
invalidate previously captured profiles, including when restoring the old mode;
recapture is explicit. Device/submission completion failures withdraw GPU
profiles. Existing executable and model-availability checks remain mandatory.
CUDA and Level Zero do not yet have native federation vocabularies.

Cancellation does not preempt a kernel. Native generation joins device work on
success, error, cancellation and unwind before finishing its job and releasing
the request lease. Request-owned Metal/HIP executors join their private work;
Vulkan retains deferred allocations after ambiguous submission failures.
An unprovable join is a failure, never successful terminal completion or an
automatic redispatch. Already released output cannot be recalled.

This is single-hop GPU **request delegation**, not pooled GPU memory,
distributed tensors, KV transfer, training or model transfer. Each executor
already owns the exact model. Independent trust, caller disclosure, quotas,
deadlines and direct-mTLS/no-retry rules are unchanged.
