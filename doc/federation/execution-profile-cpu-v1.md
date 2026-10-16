# Native CPU execution profiles

This defines the `mdma.cpu.v1` mode vocabulary for the
[`mdma.execution-profile.v1` manifest](identity-and-catalog-v1.md).
It describes local load provenance, not export permission, remote attestation
or a guarantee of bit-identical results on different machines.

Only frozen native entries can receive a profile. Decoders require the concrete
`CpuBackend`, not a backend label or operator-supplied mode strings. Native Laya
always runs on its loaded CPU predictor, independently of the daemon's decoder
backend. Concrete Metal, ROCm and Vulkan decoders use the separate
[GPU vocabularies](execution-profile-gpu-v1.md); other backends remain unprofiled.

## Executable identity

`build_id` is `sha256:` plus the SHA-256 of the **complete running executable**.
It is not the informational `MDMA_BUILD_ID` VCS/operator token.

- Linux opens the kernel's `/proc/self/exe`, including an unlinked running inode.
- macOS uses dyld's main-image header address and the kernel's
  `PROC_PIDREGIONPATHINFO` mapped-vnode identity. The opened descriptor must match
  its device/inode, size and modification/change timestamps. A pathname or
  Mach-O UUID alone is insufficient.
- Other platforms, unavailable kernel information, mismatched descriptors and
  changes while hashing fail closed. There is no `current_exe()`-only fallback.

The descriptor stays pinned; current metadata and running-image identity are
rechecked before the registry returns a profile. Deployment/metadata changes
conservatively withdraw old snapshots. Recapture clears old profiles before
fallible work. Native availability and warm-up withdrawal are checked too.
These checks are not protection against the operator/root modifying the running
process or kernel. Shared system math libraries are not attested by the binary
digest; the `math` mode describes the API/implementation family, not library
byte identity or cross-platform numerical equivalence.

## Manifest fields

`format` is `mdma.execution-profile.v1`; `backend` is `cpu`.
`activation_dtype` is `f32`. Decoder `kv_dtype` is `f32`; Laya uses `none`.
SHA-256 of the canonical manifest bytes is `profile_id`.

All `modes` values are strings:

| Key | Vocabulary and meaning |
| --- | --- |
| `vocabulary` | `mdma.cpu.v1` |
| `workload` | `decoder` or `laya` |
| `execution` | `eager`; no GPU graphs or compiled graph replay |
| `weights` | `gguf-raw-matrices-f32-norms` or `safetensors-raw-matrices-f32-norms`; matrices retain source dtype until row-to-f32 dequantization; norms/biases use f32. Frozen GGUF does not consume prepared cache payloads. |
| `matmul` | `rayon-rows-16-lane-f32-dot`; independent output rows parallelized, each row dequantized to f32 and its products summed in 16 fixed lanes, then the lanes and the tail in order, never across threads. Scheduling/thread count does not select a different reduction algorithm. With `MDMA_CPU_MATMUL=packed`, a decoder's quantized weights instead take `rayon-rows-packed-q8-activation-blocks-else-16-lane-f32-dot`: activations are quantized once per matmul to the weight type's dot type (Q8_K, Q8_0 or Q8_1) and Candle's block kernels take each row; f32/f16/bf16 weights and Laya keep the 16-lane path. |
| `attention` | `scalar-sequential-f32-softmax`; f32 dot products and ordered value accumulation, max-subtracted f32 softmax; causal/windowed decoder or bidirectional encoder geometry comes from the artifact/operation. |
| `normalization` | `scalar-sequential-f32`; f32 RMSNorm/LayerNorm sums, with artifact/architecture epsilon. |
| `activation` | `mdma-native-activations-v1`; native f32 SiLU, tanh GELU, ReLU, scale and softcap. Encoder GELU uses the Abramowitz–Stegun 7.1.26 erf polynomial with f64 intermediates, rounded back to f32. |
| `rope` | `scalar-f32-powf-sin-cos`; interleaved/split-half/partial/encoder geometry and factors are artifact/architecture-defined. |
| `math` | `rust-native-system-math`; standard Rust/platform transcendental functions, not fast-math GPU shaders, BLAS or a portable bit-exact math package. |
| `target_arch` | The compiled `std::env::consts::ARCH` string |
| `target_os` | The compiled `std::env::consts::OS` string |
| `batching` | Decoder `native-request-prefill`, or Laya `serial-chunks-16` |
| `decisions` | Decoder `none`, or Laya `f32-temperature-softmax-f64-confidence-round4`; calibration clamps/buckets and output rounding follow the loaded predictor and binary. |

The artifact digest separately binds weights, tokenizer, architecture, RoPE
parameters, decision heads and calibration. Aliases can share a profile while
retaining different context/admission limits. Request sampling/effective limits
remain operation policy, not interchangeable-profile permission.

## Current boundary

`mdma-serve` captures one profile per workload after warm-up, only when
federation is explicitly configured. Failure leaves models non-exportable;
ordinary local serving and authenticated empty-catalog control RPCs can continue.
Use the current registry getter for model availability; `is_current()` on a
cloned profile checks only executable provenance, not model readiness or policy.
Profile capture itself creates no grants. See [runtime activation](routing-runtime.md)
for separately approved catalogs, routes and delegated workloads.
