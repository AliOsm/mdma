# Vulkan residual fusion and tiled attention

## Selection

- Residual addition plus weighted RMSNorm uses one dispatch by default,
  through the existing `Backend::add_rmsnorm` interface.
  `MDMA_VK_FUSED_NORM=0`, `with_fused_norm(false)` or
  `set_fused_norm(false)` selects separate add/norm dispatches.
  Both paths write the residual and normalized outputs and preserve
  oversized output tails. The fused add explicitly rounds to f32 before
  computing the norm.
- Tiled attention shares a 32-key K/V tile across eight queries. K and V
  reuse the same shared allocation; outputs and all arithmetic remain f32.
  Its tiling/online-softmax structure is adapted from llama.cpp
  `bed0a8566`'s generic `flash_attn.comp`, with MIT attribution in
  `LICENSE-llama.cpp`. It does not require WMMA/cooperative matrices or
  requantize activations.
- The tiled path is enabled by default when its pipeline is available.
  It requires configurable wave32 compute subgroups, arithmetic subgroup
  operations, 128-thread workgroups and 17,920 bytes of shared memory.
  Missing capabilities or pipeline creation failure retain the original
  attention kernel.
- Selection is limited to head widths 32 through 128 and batches of at
  least four queries. One-token decode, smaller batches and other head
  widths retain the original attention kernel (up to the existing limit
  of 512). This change does not implement split-KV decode.
- `MDMA_VK_TILED_ATTN=0`, `with_tiled_attention(false)` or
  `set_tiled_attention(false)` disables tiling.
  `tiled_attention_available()` reports pipeline support, while
  `tiled_attention()` reports whether selection is enabled; individual
  shapes still have to meet the limits above.
- Causal masking, cached prefixes, grouped-query heads, sliding windows,
  partial query/key tiles and head widths not divisible by four are
  supported. Masked rows contribute zero mass without `exp(-inf - -inf)`.
  Changing the reduction order can change f32 rounding: this is not a
  bit-identical attention mode.
- No additional persistent/device scratch buffers are allocated.

## Correctness gates

Run the gates on the target GPU/driver. The tiled kernel has no
hardware-specific matrix-fragment layout, but that is not proof of hardware
compatibility or performance.

- Residual/norm against CPU: widths 1/3/255/256/257/4096/8192/16385,
  single- and multi-token rows, exact residual bytes, zero cancellation
  rows, normalized outputs within 2e-5, both output canary tails, and
  invalid shapes. Both fused and separate paths are checked.
- Attention against CPU: widths 32/33/63/64/65/96/97/127/128, 4 through
  17 queries, prefixes through 2,049 tokens, six query heads/two KV heads,
  windows 0/1/7/33/512, positive/negative/zero score scales, and output
  canary tails. Both original and tiled paths use a 3e-4 absolute tolerance.
- A separate gate makes score maxima increase across successive tiles,
  including scores large enough to require stable softmax rescaling.
- MiniCheck prefill plus its stored llama.cpp greedy continuation, with fused
  norm and tiled attention enabled.
- A real MiniCheck cached-prefix gate compares every final logit with
  original-kernel execution, with f32 matmul and a shared 1,024-token
  prefix (absolute tolerance 1e-3). This is a bounded model/prompt gate,
  not an assertion of identical output for every model.

## Reproduce

```sh
cargo test --release --features vulkan --lib backend::vulkan::norm \
  -- --include-ignored --nocapture --test-threads=1

export MDMA_TEST_GGUF=/path/to/Bespoke-MiniCheck-7B-Q4_K_M.gguf
cargo test --release --features vulkan --lib backend::vulkan::attention \
  -- --include-ignored --nocapture --test-threads=1
```

The ignored tiled-attention/model gates require the selected pipeline
and fail rather than silently benchmarking the fallback.
Compare fused/separate and tiled/original paths on the same model, device,
and workload. Hot-buffer kernel timings are not whole-model speedups, and
fusion is not guaranteed to be faster for every shape.
