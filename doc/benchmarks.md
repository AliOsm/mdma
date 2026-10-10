# CPU and Metal decoder benchmarking

Use `mdma-bench` to measure model loading, logical native memory, prefill,
and fixed-length greedy decode in isolation:

```sh
cargo build --release --features metal --bin mdma-bench
MDMA_BACKEND=metal /usr/bin/time -l target/release/mdma-bench \
  /path/to/model.gguf 128 16 3
```

Use `MDMA_BACKEND=cpu` for the same CPU workload. For a CPU-only build, omit
`--features metal`. The arguments select 128 prompt tokens, 16 decode steps,
and three timed trials after one complete untimed warmup.

## Method and limits

- The input cycles the tokenization of `"The capital of France is Paris.\n"`.
  Each trial uses fresh KV. KV/workspace allocation is outside throughput
  timers; prefill workspace is released before decode.
- Decode includes greedy selection and logits downloads. EOS does not shorten
  the fixed workload. Gemma prefill is sequential.
- Load includes GGUF reading, tokenizer construction, and weight loading,
  but excludes backend initialization. Filesystem and driver/compiler caches
  are not forced cold.
- Logical native peak is weights + KV + max(prefill workspace, decode
  workspace). It excludes backend-private scratch, driver and host overhead.
  macOS `/usr/bin/time -l` reports process peak RSS separately. Neither is
  total RAM/VRAM usage; do not add the two metrics together.
- These are library measurements, not streaming latency, P95, sampling,
  network latency, or a comparison with another inference engine.

Run one model/backend per fresh process, serially, without competing inference
or builds. Record model checksum, release revision, Rust/driver versions,
hardware, power/thermal conditions, workload, and per-trial durations alongside
results. Run the relevant [correctness gates](testing.md) first.
Model weights are not bundled.

## Metal batching and tiles

Use the same binary and `MDMA_BENCH_DECODER=executor` for both modes:

```sh
MDMA_BACKEND=metal MDMA_BENCH_DECODER=executor \
  MDMA_METAL_BATCH=1 MDMA_METAL_TILE=1 \
  /usr/bin/time -l target/release/mdma-bench \
  /path/to/model.gguf 128 16 3
```

Set both switches to `0` for the control. Executor setup remains outside the
decode timer; prefill always calls `Model::prefill`. Compare modes with identical
models, prompts, trial counts, and power conditions. A measured kernel or
workload gain does not establish a universal speedup.
