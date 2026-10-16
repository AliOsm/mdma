# Serving

`mdma-serve` is configured by environment variables - no CLI flags - and
refuses to start without mTLS material.

```sh
# one-time: self-signed CA + server cert + one client cert into ./certs
cargo run --release --bin mdma-certs

MDMA_MODELS_DIR=/models \
MDMA_LISTEN=0.0.0.0:6362 \
MDMA_TLS_CERT=certs/server.pem \
MDMA_TLS_KEY=certs/server-key.pem \
MDMA_TLS_CA=certs/ca.pem \
MDMA_BACKEND=rocm \
cargo run --release --features rocm --bin mdma-serve
```

`MDMA_LISTEN` defaults to `127.0.0.1:6362` ("mdma" on a phone keypad), so a
bare server is reachable only from its own machine; set it to serve others.
Clients default to `MDMA_ADDR=localhost:6362`.

`MDMA_BACKEND` accepts `cpu` (default), `rocm`, `level0`, `vulkan`, `cuda`,
or `metal`, with the corresponding build feature and supported hardware.
`MDMA_CPU_MATMUL=exact|packed` picks how CPU work multiplies quantized weights
(see [backends](backends.md#cpu-matmul)); `packed` is several times faster on
the CPU and changes logits.
Each model uses the context it was trained for (`<arch>.context_length` in its
GGUF); `MDMA_N_CTX` optionally lowers it for every model and never raises it. A
prompt must fit that context. Output is capped by the space left and by
`MDMA_MAX_OUTPUT_TOKENS` (default 4096): asking for more is lowered, not refused,
and the `accepted` event reports the cap. Prompts that leave no room for
requested output are rejected. Explicit invalid configuration values fail
startup. Context rejection is reported as gRPC `RESOURCE_EXHAUSTED`, before
tensor allocation.

`mdma-serve` opens its listener once the models folder is read; it loads no
weights at startup. A model loads on its first use (one load at a time per
file, shared by every request waiting for it), warms up, and stays resident.
`MDMA_WARMUP_ROUNDS` defaults to **1** (integer **0..8**; **0** disables it):
each load runs up to 16 synthetic prefill tokens and four decode steps per
round, reduced for small contexts. A load that runs out of memory leaves the
model `available` and is retried on its next use; any other load failure marks
it `failed`. Temporary KV/workspace/graphs do not seed user requests.
See [service startup and readiness](services.md#startup-warm-up).

Fresh request KV caches are sized to the prompt plus the output tokens
that actually need evaluation, not the full context limit, and are reserved up
front for that whole case; the output cap is what bounds it. With 128 KiB of
f16 KV per token (a 7B model on CUDA), a 4096-token cap reserves at most about
512 MiB beyond the prompt's share. Gemma 4's sliding-window layers keep only
their window (512 rows on E2B) as a ring, so its KV grows mostly with the few
global layers; a prefix hit that needs rows the ring already overwrote
re-prefills from the start. One-token
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


## The models folder

`MDMA_MODELS_DIR` is the one folder a server serves from:

```text
/models/
  minicheck.gguf          -> served as "minicheck" (Decide; Synthesize for raw prompts)
  qwen3-8b.gguf           -> "qwen3-8b"
  tiny.gguf -> minicheck.gguf   a symlink: an alias sharing one upload
  laya/                   -> "laya": a Laya checkpoint folder (Decide)
```

Every `*.gguf` is a GGUF decoder named after its file stem, and every
subfolder holding `model.safetensors`, `rl_agent_config.json`,
`encoder/config.json` and `tokenizer/tokenizer{,_config}.json` is a Laya
checkpoint named after the folder. Names must match `[A-Za-z0-9._-]{1,128}`.
Dotfiles, other files and GGUFs that fail preflight (unsupported
architecture, missing tokenizer) are skipped with a log line. Symlinks are
followed, so a model can live on another disk. Requests name the model; an
empty name means `MDMA_DEFAULT_MODEL`, else the first name alphabetically.
Models on other MDMA nodes are reached through [federation](federation.md)
routes.

A server refuses an empty folder unless it routes to peers. A node that only
routes is a front door: it lists its routed models, is ready without local
ones, and answers a request with an empty model name with `NOT_FOUND`
("this server has no local models").

### Memory, eviction and leases

When a load, or a request's KV and workspace, does not fit, the server unloads
the least recently used models that nobody holds a lease on and no request is
using, until it fits. If it still does not fit, the request fails with
`RESOURCE_EXHAUSTED` naming what holds the memory. CUDA, ROCm, Vulkan and Metal check free device memory before
each load; the CPU backend budgets 75% of RAM unless
`MDMA_NATIVE_TENSOR_BYTES` says otherwise. `MDMA_IDLE_UNLOAD_SECS` (default 0,
never) also unloads models unused that long.

A lease keeps a model loaded: `Acquire{model, ttl_ms, wait}` returns a lease
id (with `wait`, once the model is loaded; otherwise the load starts in the
background), `Renew` extends it and `Release` ends it. A lease lasts `ttl_ms`
(5 s to 10 min, 60 s by default) unless renewed, so a client that disappears
lets its models go. Anyone may use a loaded model; a client certificate may
hold 8 leases, 256 in all.

A lease on a federated alias is held by this node, which keeps one lease on
the executor while any local lease on the alias lasts. `Acquire` returns at
once there (`wait` is ignored); `residency` follows the executor's last
catalog.

`Introspect` lists each model's backend, architecture, context, on-disk size,
`leases` and `residency`: `available` (on disk), `loading`, `resident` or `failed`. A
model that does not fit in memory stays `available` with its `error` and is
retried on the next use; a corrupt file or one changed on disk is `failed`
until restart and marks the server `degraded`. Names for the **same file**
(symlinks, hardlinks) share one weight upload, tokenizer and context (the
trained one, or `MDMA_N_CTX` when lower); request KV is not shared.

Each model also reports what its file header says, read when the server lists
it: `parameters` (elements across its tensors), `quantization` (llama.cpp's
file type such as `Q4_K_M`, else the tensor type holding the most bytes, such
as `Q4_K`) and `trained_ctx` (its own context, before `MDMA_N_CTX`). A Laya
checkpoint reads them from its `model.safetensors` header (the dtype, such as
`F32`) and reports its `max_len`; a header that does not parse fails the
listing, as a corrupt GGUF does. `mdma-client` prints them as
`params=1.1B q=Q4_K_M trained=2048`.

Vulkan's f16 prefill staging is fixed at startup by `MDMA_VK_F16_WEIGHTS=1`
(f32 otherwise) and shared by every decoder; it cannot enable a staging
pipeline unavailable on the device.

`mdma-client` is a minimal mTLS client (Introspect with no prompt, else a
streamed completion; `MDMA_MODEL` picks a registry entry, `--decide
request.json` sends typed-decision questions to a Laya model):

```sh
MDMA_ADDR=localhost:6362 MDMA_TLS_CERT=certs/client.pem MDMA_TLS_KEY=certs/client-key.pem \
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
`RESOURCE_EXHAUSTED` before streaming. A federation executor applies the same
caps to routed requests; its catalog reports `degraded` but keeps advertising
the configured limits, so going on or off battery does not break routes.
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
  rpc Decide(DecideRequest)         returns (DecideResponse);             // decision models (Laya, MiniCheck), JSON in and out
  rpc Introspect(IntrospectRequest) returns (IntrospectResponse);         // server state, power, and one ModelStatus per model
  rpc Watch(WatchRequest)           returns (stream ServerSnapshot);      // status, memory and (operators) federation, on change
  rpc Push(PushRequest)             returns (stream PushProgress);        // (admin pins) a models-folder file to a federation peer
  rpc ImportRoutes(ImportRoutesRequest) returns (ImportRoutesResponse);   // (admin pins) a peer's ready models as routes, live
}
```

`SynthesizeRequest` carries the prompt, `max_tokens`, and an optional
`SamplerConfig` (temperature, top-p, min-p, repetition penalty and
window). Responses stream one token at a time with its decoded text.
`SynthesizeRequest` options:

- `emit_events`: an `accepted` message before the tokens (prompt tokens, output
  cap, `n_ctx`) and a `completed` one after them (emitted tokens, finish
  reason, prefill and decode milliseconds). Routed models report the
  executor's numbers and timings.
- `chat`: the prompt is sent as a user turn in the model's chat format
  (`ModelStatus.chat_format`: `chatml`, `llama3`, `zephyr`, `gemma`, `gemma4`
  or `inst`). `system` adds a system message. `mdma-client` sets them with
  `MDMA_CHAT=1` and `MDMA_SYSTEM`. For a routed model the origin uses the
  format its executor's signed catalog advertises and forwards the wrapped
  turn.

`ModelStatus.geometry` gives a native decoder's layers, widths, heads and KV
bytes per position.

`ModelStatus.class` says what a model is for: `generative`, `decision`,
`embedding` or `reranker`. Laya checkpoints are `decision`; a GGUF is
classified by its `general.tags` (`text-classification` makes MiniCheck a
`decision` model).
`ModelStatus.capabilities` lists `reasoning` for models whose chat template
thinks before answering (`<think>`).

`ModelStatus.decision` says what a decision model takes through `Decide`:

- `laya`: a state and typed questions (see [Laya](laya.md)).
- `minicheck`: Bespoke-MiniCheck (a GGUF whose `general.basename` is
  `Bespoke-MiniCheck`, tagged `text-classification`, with a ChatML template).
  `state_json` is `{"document": "..."}` and `questions_json` maps 1 to 128 ids
  to `{"claim": "..."}`; nothing may be empty and no other fields are allowed.
  The answer is
  `{"model": "minicheck", "answers": {"<id>": {"supported": true, "probability": 0.986}}, "usage": {"input_tokens": 210, "output_tokens": 0}}`.

  Each claim is checked the way MiniCheck's reference implementation does:
  its system prompt and `Document: …\nClaim: …` as one ChatML turn, prefilled
  without generating, scored by the probability of a first token reading `yes`
  in any case; `supported` means above 0.5. The system prompt and document are
  prefilled once and shared by every claim of the request. A prompt beyond
  `n_ctx` is refused: the document is not chunked and claims are not split
  into sentences, so send one claim per question.

Other decision-class GGUFs are served through `Synthesize` only, and a text
generator refuses `Decide`.

`Watch` streams a `ServerSnapshot` on every change: the Introspect answer,
memory (native budget by weights, KV cache and workspace; buffer quotas;
device and free memory; weights per upload; requests in flight) and the
federation topology (peers, trust state, grants, catalogs, routes). The
topology goes to operators only: list their client certificate pins in
`MDMA_OPERATOR_SPKI` (comma-separated; `mdma-certs --spki client.pem` prints
one). Each server reports its `Host`: name (`MDMA_NODE_NAME` or the host
name), OS (`FreeBSD 16.0-CURRENT`, `macOS 15.6`), architecture, CPU and
accelerator, with live figures: RAM free of total, the accelerator's memory
free of total, and the free space of the disk holding the models folder (after
following symlinks, so a folder on another disk reports that disk). Upstream and federated models carry the host they run on. [mdma-web](web.md) is a browser UI
built on these.

Two RPCs change a running federation server, and only clients whose pins
are in `MDMA_ADMIN_SPKI` may call them. That list is separate from
`MDMA_OPERATOR_SPKI`, so a gateway that shows operators the topology cannot
change the federation for its own users.

- `Push` sends `<file>.gguf` from the server's models folder to a peer that
  grants installs, streaming bytes sent and then the outcome; dropping the
  stream cancels it. `mdma-client --push PEER FILE [MODEL]` calls it.
- `ImportRoutes` routes every ready model a peer offers now, replacing its
  earlier routes, for the calling client or the pin in `client_spki`, without
  a restart; it needs a server that already routes.
  `mdma-client --routes PEER [PREFIX]` calls it.

See [pushing models](federation/routing-runtime.md#pushing-models-to-a-peer).
