# Native architectures and capabilities

MDMA dispatches on the GGUF `general.architecture`, not the model's brand
or filename. An architecture implementation and its supported task are
separate: a text decoder does not automatically provide reranking, image
input, OCR, embeddings, or arbitrary llama.cpp compatibility.

## Llama/InternLM2 and Gemma 4

Tested model: a llama-architecture 7B (`internlm2` family) in Q4_K_M -
Q4_K and Q6_K weights, F32/F16 for the rest. Correctness is gated on a
byte-for-byte greedy-continuation match against llama.cpp on the same
file, on the CPU and supported GPU backends.

TinyLlama 1.1B Chat and **Gemma 4 E2B IT (text-only)** Q4_K_M pass the pinned
CPU/Metal reference gates on Vulkan with f32 staging and ROCm's explicit
f32 compute/KV mode (`HipBackend::new_f32()`). Decoder kernels run on the GPU
without a CPU fallback. HIP's default int8/f16 fast path also matches Gemma's
stored greedy tokens, but is not an f32 log-probability parity mode. On TinyLlama
it still fails the strict continuation and prefill/decode consistency gates.

Gemma also runs on CPU and Metal. Its decoder
uses shared KV, local/global attention, per-layer inputs, and tied embeddings.
ROCm and Vulkan support its BF16/Q5_K weights, scale, GELU-tanh gating,
softcap, split-half RoPE, and windowed attention with heads up to 512. Gemma prefills
sequentially; use a 2048-token session cap. Audio, vision, E4B, MoE, and
long-context support are not included.

Native GGUF loading validates Llama/InternLM2, Gemma 4, dense GLM-4, and
Qwen2/Qwen3 text decoder layouts, not arbitrary llama.cpp models. Qwen2-VL
accepts text input only: its image projector/OCR path is not implemented.
The pinned GLM-4-32B checkpoint gate covers three bounded greedy references on
ROCm in f32 and fast modes; Qwen3's checkpoint gate remains open.
Architecture, geometry, tensor shapes, backend dtypes and file
extents are checked before weight uploads; the registry preflights every
native GGUF before loading any weights.

## Dense Qwen text decoder

`qwen2`, `qwen3`, and the **text-only** `qwen2vl` decoder have a separate
native execution/preflight module:

- Separate Q/K/V projections; Qwen2 Q/K/V biases or Qwen3 per-head Q/K RMSNorm.
- Head width independent of embedding width; grouped-query attention.
- Split-half RoPE, RMSNorm, SwiGLU, explicit or tied output embeddings.
- GGUF byte-level BPE using `tokenizers`, the declared pre-tokenizer and merge
  ranks, and original special-token IDs. No implicit space or BOS for Qwen.
- Incremental BPE decoding preserves characters spanning multiple tokens.
  A token's streamed `text` may be empty until its UTF-8 sequence completes.
  Incomplete text at truncation is not emitted; token IDs remain available.
- Sequential prefill, including cached prefixes at nonzero positions. This is
  a correctness-first path, not the optimized batched Llama prefill.
- HIP graph replay is disabled for Qwen; its positions use eager execution.
- Generated RoPE constants and independent-width scratch count toward native
  tensor budgets; the existing Llama and Gemma4 allocation layouts are preserved.

Backend admission requires split-half RoPE and, for Qwen2, bias addition.
CPU and ROCm implement both; Vulkan/Metal currently lack Qwen2 bias addition.
Backend dtype/head limits still apply. Q8_0 is supported by the CPU decoder,
not the current ROCm decoder.

Unknown architecture metadata, scaling modes, extra tensors, classifier
heads, malformed geometry, incompatible dtypes and truncated payloads fail
before backend allocation/upload. The registry also validates tokenizers
before loading weights. Prompts are raw text; supply chat markers explicitly.

### Checkpoint validation boundary

Synthetic CPU/ROCm tests cover Qwen2/Qwen3 in f32 and fast modes. Checkpoint
tokenizer tests compare Qwen3-8B and NuMarkdown against llama.cpp, including
Unicode and special markers.

NuMarkdown's **text** decoder matches the reference's English, chat/arithmetic
(including EOS), and Chinese greedy continuations. The optional
`numarkdown_rocm_greedy_checkpoint` test pins the model SHA-256 and those
sequences. It also checks prefill/decode agreement and eager execution with
HIP graphs requested. This does not validate its vision projector or OCR.

Qwen3-8B still has an **open checkpoint gate**: fast mode matches the chat and
Chinese sequences, but differs at English output position 11. F32 mode matches
the English sequence, but differs at chat position 26. Matching KV precision
and sequential reference prefill did not eliminate the fast-mode discrepancy.
The same teacher-forced prefix can select different leading tokens in the two
MDMA precision modes. Do not mistake a passing metadata preflight or diagnostic
test for full checkpoint parity, or deploy this checkpoint as fully validated.

Reference: llama.cpp `6f41ac59e0a49a00483a316a22ada6b04edd2950`, HIP,
context 128, microbatch 1, f16 KV, flash attention off, greedy/no penalties.
F32 KV does not force llama.cpp's quantized matrix operations to use f32
activations. Exact token equality is not guaranteed across precision modes.

## Dense GLM-4 text decoder

`glm4` has its own native decoder and metadata-only preflight:

- Separate Q/K/V projections and independent head width with GQA.
- **Interleaved partial RoPE**, with frequencies based on the rotary prefix
  width, not the full head width. The remaining head values are unchanged.
- Pre-attention/pre-FFN RMSNorm and post-attention/post-FFN RMSNorm **before**
  each residual addition.
- Packed FFN projection: SiLU of the first half multiplied by the second half,
  followed by the down projection. Explicit or tied output embeddings.
- Sequential cached-prefix prefill and eager HIP decode, even with graphs
  requested. Packed FFN scratch is included in native memory reservations.
- GGUF byte-BPE with `pre=glm4`; raw prompts and explicit chat markers.

CPU and ROCm implement the required partial-RoPE primitive. Other backends
reject GLM-4 before weight upload rather than substituting another decoder.
Unknown scaling, multimodal RoPE sections, MTP, MoE and task heads are rejected.
Weight dtype and head-width limits still apply.

The independent scalar CPU decoder, rejection and memory-accounting fixtures
pass. A small Q5_K/Q6_K ROCm fixture matches CPU in f32 and fast modes, checks
prefill/decode agreement, and confirms eager execution. Partial-RoPE primitive
tests cover full/partial rotation, multiple heads and unchanged suffixes.
The pinned GLM-4-32B-0414-Q5_K_M-matteov2 checkpoint gate compares the reference's
English 32-token and Chinese 32-token continuations, and arithmetic `[19,
151336]` (`4` plus EOS), in **both f32 and fast** ROCm modes. Prompt token IDs
also match llama.cpp. The `glm4_rocm_greedy_checkpoint` test pins the source
SHA-256, prompt IDs and reference continuations; it passed in both modes.
It also checks finite logits, prefill/decode agreement, eager execution and
native-memory refunds. Qwen's small GPU fixture and NuMarkdown's pinned
checkpoint test pass against the rebuilt GLM kernel as well.

Reference: llama.cpp `6f41ac59e0a49a00483a316a22ada6b04edd2950`, HIP,
context 128, microbatch 1, f16 KV, flash attention off, greedy/no penalties.
These are bounded text gates, not long-context, CPU-checkpoint, vision, MTP,
MoE, or throughput validation.

This particular GGUF contains Q5_K/Q6_K matrices, not Q4_K. The current cache
preparer refuses raw-only copies, so use `MDMA_CACHE=off` or an `auto` cache
miss for this file; `required` cannot admit it. Other admitted Q4_K files may
use prepared caches. Cache policy still applies globally to the daemon.

## Remaining architecture and task work

| Architecture | Example artifact | Remaining validation/task work |
|---|---|---|
| `qwen3` + classifier head | Qwen3-Reranker-4B | Dedicated two-class head/pooling and reranking API; do not treat two class logits as vocabulary logits |
| `qwen35` | Qwen3.6-9B | Hybrid gated-delta recurrent layers, convolution/state lifetime, hybrid attention, and vision projector |
| `qwen2vl` | NuMarkdown-8B | Text decoder is separate from its image encoder/projector and OCR API |
| `glm4` | GLM-4-32B-0414 | Native CPU/ROCm decoder and bounded ROCm checkpoint gate; raw-only cache policy and long-context validation |
| `gemma2` | ShieldGemma-9B | Scaled embedding, four norms, GELU gating, attention/final softcaps, alternating local/global attention |

Native DeepSeek V2/V3/R1 additionally require MLA and MoE routing. A
DeepSeek **distillation** may instead be a Qwen or Llama decoder; inspect its
GGUF metadata. No native DeepSeek, MoE, recurrent, or vision support is claimed
by the Qwen implementation.

## Multi-model daemon

The registry can already serve multiple admitted models on one mTLS port,
keeping loaded weights resident. Startup skips models which cannot fit in
memory without evicting other models; restart after freeing memory to retry.
Size the resident model set with headroom for KV, scratch, and driver overhead.
Use `MDMA_GREEDY=1` for deterministic client comparisons; omitted sampler fields
request random server defaults.

Registry preflight rejects unsupported architectures at startup.
Rebuilding ROCm kernels changes the cache profile:
prepare compatible caches before restarting a daemon using `MDMA_CACHE=required`.

## Focused checks

Use one build job and one test thread on thermally constrained hosts:

```sh
CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 cargo test --lib qwen -- --test-threads=1
CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 cargo test --lib byte_bpe -- --test-threads=1
CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 cargo test --lib glm4 -- --test-threads=1
MDMA_TEST_GLM4_GGUF=/models/GLM-4-32B-0414-Q5_K_M.gguf \
  CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --lib glm4_real_model_preflight -- --ignored --test-threads=1
CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --release --features rocm --lib glm4_rocm_matches_cpu_and_uses_eager_decode \
  -- --ignored --test-threads=1
MDMA_TEST_GLM4_GGUF=/models/GLM-4-32B-0414-Q5_K_M-matteov2.gguf \
  MDMA_CACHE=off CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --release --features rocm --lib glm4_rocm_greedy_checkpoint \
  -- --ignored --test-threads=1
MDMA_TEST_QWEN_GGUF=/models/dense-Qwen.gguf \
  CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --lib real_qwen -- --ignored --test-threads=1
MDMA_TEST_QWEN_GGUF=/models/dense-Qwen.gguf \
  CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --lib qwen_real_model_preflight -- --ignored --test-threads=1
CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --release --features rocm --lib qwen_rocm_f32_and_fast_decoder \
  -- --ignored --test-threads=1
MDMA_TEST_NUMARKDOWN_GGUF=/models/NuMarkdown-8B-Thinking.Q4_K_M.gguf \
  MDMA_CACHE=off CARGO_BUILD_JOBS=1 RAYON_NUM_THREADS=1 \
  cargo test --release --features rocm --lib numarkdown_rocm_greedy_checkpoint \
  -- --ignored --test-threads=1
```

Synthetic tests compare against an independent scalar decoder and exercise
nonzero-prefix prefill, tied embeddings, head norms/biases, exact memory
reservations, fail-before-upload rejection, BPE ranks and Unicode streaming.
Checkpoint/tokenizer tests and GPU tests require explicit fixtures/hardware;
synthetic success alone is not a deployment gate.
The pinned NuMarkdown source SHA-256 is
`1212e4e1df30b31891a43cf793a966bcfd236756ce1013a0233b648e3beafa4d`.
The pinned GLM-4 source SHA-256 is
`73d94979af37a34d94081b28894cc19d75571f1c18507464157f6d3d588598a3`.
The GLM checkpoint test needs room for roughly 22 GiB of weights, plus
driver/scratch/KV storage; run one heavyweight GPU test at a time.

For teacher-forced score diagnostics, set `MDMA_TEST_QWEN_GGUF`,
`MDMA_TEST_QWEN_PROMPT`, and optionally `MDMA_TEST_QWEN_SUFFIX_IDS` (a JSON
array of reference token IDs), then explicitly run
`qwen_real_model_teacher_forced_logprob_diagnostic` with ROCm and `--ignored`.
It reports the leading logits/log-probabilities in both modes without claiming
reference parity.
Use release builds for heavyweight checks and the same kernel profile as the
cache-preparation binary. A debug test may correctly reject a release cache.
