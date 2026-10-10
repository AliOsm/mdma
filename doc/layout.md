# Source layout

```
src/gguf.rs        GGUF metadata and weight loading (mmap-free, eager)
src/tokenizer.rs   SentencePiece, Gemma 4 BPE and byte-level BPE
src/model.rs       decoder dispatch, preflight, memory planning, hybrid placement
src/model/arch/    Llama, Gemma 4, GLM-4, Qwen and ModernBERT
src/backend.rs     the Backend trait; cpu, rocm, level0, vulkan, cuda, metal and their kernels
src/kv_cache.rs    KV cache
src/sampler.rs     temperature, top-p, min-p, repetition penalty
src/session.rs     inference session lifecycle (state machine)
src/registry.rs    named decoders, Laya checkpoints and upstream models
src/memory.rs      native tensor admission and backend buffer quotas
src/cache.rs       prepared model cache
src/laya.rs        Laya decision heads, prompting and calibration
src/grpc.rs        gRPC service and mTLS client
src/upstream.rs    forwarding to other MDMA nodes
src/federation.rs  peer trust, signed catalogs, routing and execution
src/power.rs       power-aware admission
src/bin/           mdma-serve, mdma and companion binaries
proto/             gRPC definitions
deploy/            systemd, launchd and rc.d templates
doc/               operating guides and protocol notes
```

Modules are `<name>.rs` with submodules under `<name>/`; tests live in
`<mod>/tests.rs`. Model architecture code selects operation parameters.
Backends implement generic compute primitives and advertise optional support
through `Backend::supports(Operation)`. Shared validation lives in
`backend/shape.rs`; backend compatibility checks live in `backend/test_support/`.
