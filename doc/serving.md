# Serving

`mdma-serve` is configured by environment variables - no CLI flags - and
refuses to start without mTLS material.

```sh
# one-time: self-signed CA + server cert + one client cert into ./certs
cargo run --release --bin mdma-certs

MDMA_GGUF_PATH=/models/model-Q4_K_M.gguf \
MDMA_LISTEN=0.0.0.0:50051 \
MDMA_TLS_CERT=certs/server.pem \
MDMA_TLS_KEY=certs/server-key.pem \
MDMA_TLS_CA=certs/ca.pem \
MDMA_BACKEND=rocm \
MDMA_N_CTX=4096 \
cargo run --release --features rocm --bin mdma-serve
```

`MDMA_BACKEND` accepts `cpu` (default), `rocm`, `level0`, `vulkan`, `cuda`,
or `metal`, with the corresponding build feature and supported hardware.
`MDMA_N_CTX` defaults to 4096 and caps each native request's prompt plus
output, bounded by the model's trained context. Output is capped to the
remaining space; prompts that leave no room for requested output are
rejected. Explicit invalid configuration values fail startup.
Context rejection is reported as gRPC `RESOURCE_EXHAUSTED`, before tensor allocation.

`mdma-serve` keeps native weights resident and finishes inference warm-up before
opening its listener. `MDMA_WARMUP_ROUNDS` defaults to **2** (integer **0..8**;
**0** explicitly disables it). Each unique GGUF runs up to 16 synthetic prefill
tokens and four decode steps per round, reduced for small contexts. Failures
caused by memory exhaustion unload the affected model and continue startup;
other failures abort startup. Temporary KV/workspace/graphs do not seed user requests.
Wait for the mTLS `Introspect.ready` response, not just a running service process.
See [service startup and readiness](services.md#startup-warm-up).

Fresh request KV caches are sized to the prompt plus the output tokens
that actually need evaluation, not the full context limit. One-token
verdicts use prefill logits directly, without a decode workspace, graph,
or extra forward pass. Allocation runs on the blocking inference worker;
allocation/backend failures are reported through the response stream.

`MDMA_NATIVE_TENSOR_BYTES` optionally sets one shared **native tensor**
budget in integer bytes (unset: no limit). Unique weights count once;
requests reserve new KV plus peak workspace before allocation and return
gRPC `RESOURCE_EXHAUSTED` when they do not fit. Retained prefix KV stays
charged. This is **not a total RAM/VRAM cap**: backend-private scratch,
driver/staging allocations and Laya are excluded. Leave separate headroom;
do not set this to total device memory. Gemma's generated norm weights,
shared-KV geometry and sequential-prefill scratch are included.

On HIP/CUDA/Vulkan, `MDMA_DEVICE_BUFFER_BYTES` and `MDMA_HOST_BUFFER_BYTES`
optionally cap explicit backend buffers, including private scratch, graph
buffers and deferred frees. Unset means tracked but unlimited; unsupported
backends reject configured quotas. These checks are separate from native
tensor admission and can fail during execution. Driver internals and ordinary
host allocations still need headroom.


## Several models on one port

`MDMA_MODELS` replaces `MDMA_GGUF_PATH` with a registry file, so one
process serves native GGUF decoders on `MDMA_BACKEND`, Laya checkpoints
on the CPU, and models on other MDMA nodes over mTLS gRPC.
Requests name the model; an empty name means `default`.

```toml
default = "minicheck"

[[models]]
name = "minicheck"
gguf = "/models/Bespoke-MiniCheck-7B-Q4_K_M.gguf"
n_ctx = 4096                       # optional; MDMA_N_CTX otherwise

[[models]]
name = "laya"
laya = "/models/laya"              # a Laya checkpoint directory; Decide, not Synthesize

[[models]]
name = "qwen"
[models.upstream]
url = "https://worker.example:50051"
model = "qwen"                    # optional; the remote default otherwise
tls_cert = "/certs/client.pem"
tls_key = "/certs/client-key.pem"
tls_ca = "/certs/ca.pem"
```

Startup fails with the entry's name if a local model or upstream
credentials cannot be loaded. Upstreams connect on demand and require an HTTPS origin.
Certificate paths are relative to the server's working directory unless
absolute. The remote node must trust the client certificate's CA, and
`tls_ca` must trust the remote server certificate.

`Synthesize` and `Decide` forward to the configured remote model, preserving
token IDs, text, sampler fields, and gRPC errors. Local admission rules
still apply; the remote node enforces its own context and power limits.
Forwarding is limited to eight hops to bound configuration loops.

`Introspect` lists each model's backend, architecture, context, and local
on-disk weight size (zero for upstreams). A remote model is ready only
when both nodes and that model are ready; a failed or timed-out remote
health check marks the entry unready. Each remote check has a two-second
timeout. Top-level readiness describes the local node.
Native entries pointing to the **same GGUF file** share one weight upload
and tokenizer, including symlink/hardlink aliases. Names and `n_ctx` caps
remain independent; request KV is not shared. Separate files are not
deduplicated. Introspect's on-disk size is repeated per alias, not additive
resident memory.

Vulkan's automatic f16 prefill setting is shared across native decoders:
every unique loaded decoder must pass calibration to enable it. Aliases
do not recalibrate the same weights. Any rejection
or calibration error keeps the registry on f32; explicit
`MDMA_VK_F16_WEIGHTS=0` or `1` still overrides automatic calibration, but cannot
enable a staging pipeline unavailable on the device.

`mdma-client` is a minimal mTLS client (Introspect with no prompt, else a
streamed completion; `MDMA_MODEL` picks a registry entry, `--decide
request.json` sends typed-decision questions to a Laya model):

```sh
MDMA_ADDR=localhost:50051 MDMA_TLS_CERT=certs/client.pem MDMA_TLS_KEY=certs/client-key.pem \
MDMA_TLS_CA=certs/ca.pem cargo run --release --bin mdma-client -- The capital of France is
```

It waits for mTLS Introspect readiness before sending work, including the selected
model's readiness. `MDMA_READY_TIMEOUT_SECS` defaults to **120** (integer **0..86400**);
**0** makes one bounded check with no retries. Each Introspect call is capped at five
seconds and the overall deadline is respected. Unknown models and nontransient
gRPC errors fail immediately. Generation/decision requests are never retried.
For a persistent local Linux daemon and an `mdma` wrapper that loads your client
credentials, see [the user-service installation](services.md#linux--systemd-user-local-daemon).

Clients present `certs/client.pem` / `client-key.pem` and trust `ca.pem`.
The development helper creates a new CA on each invocation and never
writes its private key to disk. Use your PKI to issue additional clients
or deploy beyond localhost.

## Shutdown and power

SIGTERM/SIGINT stops new requests and drains existing streams and inference
workers. `MDMA_SHUTDOWN_TIMEOUT_SECS` defaults to 30 seconds; exceeding it
terminates remaining work with a failure exit. CPU is the portable default;
service support does not imply a GPU backend exists on every OS.

The server follows the host's power supply (Linux `/sys/class/power_supply`,
FreeBSD `hw.acpi.acline`, macOS IOKit internal battery). On battery it goes
**degraded** and keeps serving:
`max_tokens` is capped at `MDMA_DEGRADED_MAX_TOKENS` (256) and the KV context
at `MDMA_DEGRADED_N_CTX` (2048). Output is further capped to the context space
left after the prompt, respecting the server and model context limits.
Overlong prompts, or full-context prompts requesting output, get
`RESOURCE_EXHAUSTED` before streaming.
At or below `MDMA_POWER_CRITICAL_PCT` (10) it refuses new generations with
`UNAVAILABLE` and lets running streams finish. The supply is polled every
`MDMA_POWER_POLL_SECS` (5), and a change must hold for `MDMA_POWER_DEBOUNCE`
(2) polls. With the cuda backend it also reads the GPU's enforced power limit
through NVML and degrades on AC when firmware holds it below
`MDMA_POWER_GPU_CAP_PCT` (50) of the default (0 disables). `MDMA_POWER=off` disables it.
The initial power reading applies before the listener opens. Missing
supplies or failed supply reads are treated as mains power; a failed GPU
probe is ignored. Thermal throttling without a reduced power limit is
not detected.

On supported Macs, use `MDMA_BACKEND=metal` and
`cargo run --release --features metal --bin mdma-serve` with the same model,
listen and mTLS environment variables.

## gRPC API

```proto
service Mdma {
  rpc Synthesize(SynthesizeRequest) returns (stream SynthesizeResponse);  // token stream; `model` names a registry entry
  rpc Decide(DecideRequest)         returns (DecideResponse);             // Laya typed decisions, JSON in and out
  rpc Introspect(IntrospectRequest) returns (IntrospectResponse);         // server state, power, and one ModelStatus per model
}
```

`SynthesizeRequest` carries the prompt, `max_tokens`, and an optional
`SamplerConfig` (temperature, top-p, min-p, repetition penalty and
window). Responses stream one token at a time with its decoded text.
