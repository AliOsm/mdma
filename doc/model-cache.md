# Prepared model cache

`mdma-cache` prepares persistent, backend-specific weights. It does not generate
tokens, run a server, or upload model weights. It supports ROCm Q4_K matrices
and is experimental. Compact layout v3 replaces the larger v1 layout without
increasing weight bytes. Layout conversion is not a guaranteed speedup over
raw weights; validate correctness and throughput on the target hardware.

## Workflow

Build the preparation and inference binaries together:

```sh
cargo build --release --features rocm --bin mdma-cache --bin mdma-run --bin mdma-serve

export MDMA_CACHE_DIR=/path/to/model-cache
target/release/mdma-cache /models/model-Q4_K_M.gguf
target/release/mdma-cache /models/model-Q4_K_M.gguf --force

MDMA_BACKEND=rocm MDMA_CACHE=required target/release/mdma-run \
  /models/model-Q4_K_M.gguf "The capital of France is" 16
```

Preparation currently initializes HIP and loads its kernel module, so it needs
a working ROCm device even though packing runs on the host. It validates the
supported decoder and tokenizer. Models without a Q4_K matrix are rejected
rather than copied into a cache with no transformation. Other backends and
Laya checkpoints are not supported by this preparer.

The CLI prints the artifact path to stdout and a preparation/reuse summary to
stderr. `--force` rebuilds the current entry from the local GGUF; it does not
download a model or modify the GGUF. Without it, an existing entry is verified
and reused.

`--cache-dir DIR` overrides the directory for that CLI invocation only. Point
inference at the same directory using `MDMA_CACHE_DIR`. Otherwise the order is:

1. `MDMA_CACHE_DIR`
2. `$XDG_CACHE_HOME/mdma`
3. `$HOME/.cache/mdma`

An explicitly empty directory variable is an error; `XDG_CACHE_HOME` must be
absolute. Keep the cache private to the account running inference.

## Loading and invalidation

Normal `Model::load`, budgeted loads, `mdma-run`, and the native registry use:

| `MDMA_CACHE` | Behavior |
|---|---|
| `auto` (default) | Use a compatible entry; on a miss, load the original GGUF. |
| `required` | Fail if no compatible entry exists or the backend cannot use it. |
| `off` | Skip automatic cache lookup and load the original layout. |

Inference never creates or rebuilds entries. Corrupt entries fail closed, even
in `auto`; run `mdma-cache MODEL --force` to repair them, or use `off` to bypass.
Library callers can explicitly `ModelCache::attach` a cache before loading;
that attachment takes precedence over automatic lookup settings.

For repeated requests, keep `mdma-serve` running instead of launching a fresh
`mdma-run` process. Source/cache verification happens at startup; requests reuse
resident weights without another attachment or SHA pass. The daemon completes
bounded inference warm-up before listening; wait for `Introspect.ready`. See
[service startup](services.md#startup-warm-up). This amortizes startup work;
it does not fix a kernel or memory-bandwidth regression.

The original GGUF is still required. Its metadata and tokenizer are used as
usual; its whole-file SHA-256 selects the artifact. Identical files at different
paths reuse an entry. A changed byte, target architecture, layout version, or
compiled HIP kernel SHA selects a different entry. Old entries are not deleted
automatically. Prepare and load with the same kernel build. After updating from
v1/v2 to v3, rerun preparation; those artifacts are not compatible.

Lookup hashes the source on each fresh attachment and verifies each cached
tensor before upload. It does **not** trust file size or timestamps as a
substitute for SHA. File snapshots also reject source mutation while parsing,
hashing, preparing, or loading. Keep source files immutable during these steps.
When the cache directory does not exist, `auto` skips hashing.

Artifacts are private files below:

```text
<cache-root>/<source-sha256>/<profile-sha256>.mdmac
```

A writer lock serializes preparation for each entry. A temporary file is
flushed, then atomically replaces the destination, so interrupted preparation
does not publish a partial entry. Already-open readers keep their old artifact
when `--force` replaces it. Clear unused entries only when no preparers are
running; do not delete active lock files.

## What is prepared

Q4_K matrices keep the original half-precision block scales and 4-bit values.
The eight 6-bit scale/min pairs are regrouped for branchless extraction.
A 256-value block remains 144 bytes, exactly the original weight size:

| Byte range | Contents |
|---|---|
| 0–3 | Original `d`, `dmin` halves |
| 4–7 | Low four bits of each scale, two scales per byte |
| 8–11 | Low four bits of each minimum, two minima per byte |
| 12–15 | High two bits of `scale0, scale1, min0, min1`, one group per byte |
| 16–143 | Original quantized nibbles |

Each group covers two 32-value sub-blocks. Its high-bit byte uses bit offsets
0, 2, 4, and 6. All six bits of every scale/minimum are preserved.
Unlike v1, there is no expanded header or padding. Unlike experimental v2,
the fields do not cross 32-bit header words.

This is lossless layout conversion, not dequantization to FP16 or a new
quantization. Q6_K and other tensors retain their GGUF bytes. ROCm embedding,
F32 matmul, int8 decode, grouped QKV, fused gate/up, and WMMA prefill have
prepared-layout paths. Uncached weights continue using the existing kernels.

Only one layout is uploaded, not both. Native tensor admission and device
buffer budgets count the prepared bytes, which equal the original weight bytes.
The cache adds roughly a second model-sized file on disk.
Preparation holds one source tensor and its packed form in host memory at once.

Less repeated header unpacking may reduce compute work. Hashing and checksums
add startup I/O. Neither faster
startup nor higher tokens/second is guaranteed; compare the raw and prepared
paths explicitly rather than extrapolating from another layout.

## Validation on ROCm hardware

The host tests cover lossless packing against CPU dequantization, SHA
invalidation, reuse/force, malformed and corrupt entries, concurrent writers,
source mutation, and weight-budget accounting. GPU tests are explicitly
ignored unless requested:

```sh
cargo test --release --features rocm --lib backend::rocm::prepared::tests:: -- \
  --ignored --test-threads=1 --skip bench_prepared_q4k \
  --skip real_cached_model_matches_raw_continuation \
  --skip prepared_real_decode_matrices_match_raw
cargo test --release --features rocm --lib bench_prepared_q4k -- \
  --ignored --test-threads=1 --nocapture

# Requires a prepared entry from the same kernel build.
# Compares longer eager and graph continuations, including every logit.
MDMA_TEST_GGUF=/models/Bespoke-MiniCheck-7B-Q4_K_M.gguf \
  MDMA_HIP_GRAPHS=1 \
  cargo test --release --features rocm --lib real_cached_model_matches_raw_continuation -- \
  --ignored --test-threads=1 --nocapture
```

Also compare real-model generation and throughput with `MDMA_CACHE=off` and
`MDMA_CACHE=required`, using the same model, compute mode, and prompt. The
microbenchmark is not an end-to-end throughput claim.

For reproducible comparisons, run opposite-order process rounds with warmups
and repeated timed samples. Report initialization and hashing separately from
resident prefill/decode, and distinguish eager from graph execution.
Correctness gates should compare every continuation logit, not just a short
greedy token sequence: small rounding drift can be amplified by activation
requantization. Other models and compute modes require their own validation.
