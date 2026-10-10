# Vulkan int8 prefill

`MDMA_VK_INT8=1` selects experimental Q4_K/Q6_K int8 prefill, preferring
cooperative matrices and falling back to accelerated packed dot products.
`MDMA_VK_INT8=dot` forces packed dot products, including on RDNA3/4.
The cooperative-matrix kernel and shared packed-weight loader are adapted
from llama.cpp `70c4e1582` (`mul_mmq_cm1.comp` and its helpers).
The upstream MIT notice is in `LICENSE-llama.cpp` and ships with the crate.

## Selection and numerics

- Off by default; library callers can use `with_int8(true)` or
  `set_int8(true)`, or force packed dot with `with_int8_dot(true)` /
  `set_int8_dot(true)`. `int8_available()` reports either path's support;
  `int8()` reports whether the mode is enabled. `int8_dot_available()`
  and `int8_coopmat_available()` report individual path support;
  `int8_dot()` identifies the selected packed-dot mode.
- Cooperative matrices require a RADV device identified as GFX11 or GFX12, the advertised
  signed int8 16x16x16 cooperative matrix with int32 accumulation,
  Vulkan memory model support, configurable wave32 compute subgroups,
  and sufficient shared memory.
  Unknown fragment layouts never use the cooperative-matrix kernel.
- The RDNA2-compatible packed-dot kernel requires
  `VK_KHR_shader_integer_dot_product`, accelerated signed packed 4x8 dot
  products, configurable wave32 compute subgroups, 256-thread workgroups
  and at least 15,744 bytes of shared memory. It does not require WMMA or
  a hardware-specific fragment layout. Missing features retain existing
  kernels; an incomplete optional pipeline group does not disable the
  other supported path.
- Q4_K/Q6_K batches of two or more tokens use a 64x64 output tile for
  cooperative matrices, or 64x32 for packed dot.
  One-token decode, float weights, BF16 and Q5_K are unchanged.
- Activations are quantized once per matmul to 32-element Q8_1 blocks:
  `d = f16_RNE(amax/127)`, `q = round_away(x * (127/amax))`, with an exact
  integer block sum. Zero blocks produce zero scales and quants.
  Q4_K applies the asymmetric min correction to the quantized sum;
  Q6_K applies its separate 16-element scales. Final sums are f32.
- This changes activation rounding and is **not** an f32 parity mode.
  Kernel gates compare against a CPU reference with the same requantized
  activations, not against the original f32 inputs.
- A budget-accounted scratch buffer is reused, grows to the largest
  batch seen, and remains resident until the backend is dropped.
- Int8 takes precedence over `MDMA_VK_F16_WEIGHTS`. The server skips f16
  calibration while int8 is enabled; direct `calibrate_f16` callers must
  disable int8 first.
- Vulkan builds need a GLSL compiler supporting `GL_KHR_cooperative_matrix`
  and `GL_EXT_integer_dot_product`
  (tested with glslangValidator 16.4.0).

## Validation

Run these gates on the target GPU/driver:

- Both paths: synthetic Q4_K/Q6_K matmul against the requantized CPU reference:
  1/3/31/33/63/64/65/129 rows, 256/512/768 input dimensions, and
  2/3/31/33/63/64/67/128 tokens; absolute tolerance 1e-3.
- Zero blocks, half-away quant rounding, half-scale nearest-even rounding,
  subnormal scales, signed block sums, two-dimensional quantization grids,
  partial tiles, and odd-aligned 210-byte Q6_K rows.
- Real MiniCheck Q4_K/Q6_K rows at 67 tokens use the same 1e-3 gate.
- MiniCheck prefill followed by the stored llama.cpp greedy continuation
  with int8 enabled. This is a bounded fixture gate, not a claim
  that all models or prompts have identical continuations.

Passing a forced packed-dot gate on one device does not establish performance
or hardware compatibility on another GPU generation.

## Prefill benchmarking

Use the same model, device, and workload for each mode. Disable residual
fusion and query-tiled attention when isolating int8's effect. Prefill
measurements are not decode speedups or general backend comparisons.

```sh
MDMA_TEST_GGUF=/path/to/Bespoke-MiniCheck-7B-Q4_K_M.gguf \
  cargo test --release --features vulkan --lib \
  backend::vulkan::coopmat -- --include-ignored --test-threads=1

MDMA_TEST_GGUF=/path/to/Bespoke-MiniCheck-7B-Q4_K_M.gguf \
  MDMA_VK_INT8=1 MDMA_VK_FUSED_NORM=0 MDMA_VK_TILED_ATTN=0 \
  cargo test --release --features vulkan --lib \
  backend::vulkan::tests::bench_prefill -- --ignored --nocapture --test-threads=1
```

## RDNA2 hardware gate

With a Vulkan driver advertising the required accelerated dot features:

```sh
export MDMA_TEST_GGUF=/path/to/Bespoke-MiniCheck-7B-Q4_K_M.gguf
cargo test --release --features vulkan --lib backend::vulkan::dot \
  -- --include-ignored --nocapture --test-threads=1
MDMA_VK_INT8=dot cargo test --release --features vulkan --lib \
  backend::vulkan::coopmat::tests::quantize_matches \
  -- --nocapture --test-threads=1
MDMA_VK_INT8=dot cargo test --release --features vulkan --lib \
  backend::vulkan::tests::bench_prefill -- --ignored --nocapture --test-threads=1
```

The ignored packed-dot real-row and greedy tests **require** the selected
path and fail rather than silently exercising a fallback. Compare the
benchmark with `MDMA_VK_INT8=0 MDMA_VK_F16_WEIGHTS=0` and
`MDMA_VK_INT8=0 MDMA_VK_F16_WEIGHTS=1` on the same device.
