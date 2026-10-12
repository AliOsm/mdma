# Federation security foundation

The Rust library now provides peer policy validation, durable local enrollment
and strict rustls certificate verification. **Federation is disabled by
default.** The server can explicitly enable a dedicated catalog/control listener.
Separate default-off switches now integrate native CPU synthesis/typed decisions
and explicit single-hop origin routes. Real mTLS loopback and two-host CPU
synthesis are tested. See [runtime acceptance](routing-runtime.md) for the
physical typed-decision fixture and actual-checkpoint/hardware limitations.

## Local policy

`federation::config::File::parse` validates bounded TOML, rejects unknown fields,
self/duplicate identities and non-origin endpoints, and grants nothing by
default. Enrollment state and policy generations belong to the persistent
trust store, not to a remotely supplied document.

Each `ca_roots` entry contains `path` and `certificate_sha256`. The latter is
`sha256:` plus the lowercase SHA-256 of the **complete certificate DER**. A
root file contains exactly one PEM certificate. Replacing the file cannot
silently approve a different issuer. TLS pins instead hash DER SPKI;
signing keys contain URL-safe unpadded Base64 of 32 raw Ed25519 bytes.

TLS pins specify `role = "client_auth"` or `"server_auth"`, `spki_sha256`,
`valid_from_ms` and exclusive `valid_until_ms`. Signing keys specify `key_id`,
`public_key` and the same finite window fields. Empty roles deny connections.
Control, catalog, execution and route-import permissions are independent.

## Administrative lifecycle

1. Independently authenticate the public enrollment material as described in
   [identity lifecycle](identity-lifecycle-v1.md). Parsing is not verification.
2. Explicitly `Store::initialize` local state once. `Store::open` fails closed
   if the required file is missing, corrupt, unreadable or bound to another
   local identity; it never recreates trust.
3. `stage` clears all proposed grants. `allow_staged_identify` explicitly enables
   probes only. `activate_control` allows reviewed control grants with no model
   exports, execution grants or imports.
4. `approve_policy` is a separate reviewed publication for active peers.
   Suspension/offboarding clears grants. Recovery passes through staged
   enrollment and retains immutable signing-ID history and material denials.
5. `revoke_tls` denies SPKI material across both roles and identities;
   `revoke_signing` denies raw signing bytes under every label. Affected peers
   are suspended. Revocations are written and synced before success is reported.

The store has one locked writer. Keep its adjacent `.lock` file; do not unlink
or replace it. Updates use synced atomic replacement and increment a generation.
An ambiguous post-rename storage failure disables authorization until restart
and reconciliation. Trusted local storage and a dependable UTC clock remain
assumptions; the format does not protect against malicious backup rollback.
Observed window retirement persists even for removed credentials.

## Transport binding

`tls::server_config` and `tls::client_config` build TLS 1.3-only configurations
from scratch with mandatory mTLS, HTTP/2, no resumption/tickets/early data.
Verification requires a currently eligible peer, its own pinned roots, normal
PKI constraints and validity, explicit leaf key usage/EKU, exactly one matching
identity URI, a current role-specific SPKI pin and outbound service-name SAN.
TLS and catalog-signing key material must differ.

Rama's inbound rustls metadata currently omits the peer chain. `BindPeerService`
uses Rama's lossless stream conversion to access the **completed authenticated
rustls connection**, then attaches a private-field `Binding` to stream
extensions. Identity headers and `Identify` responses cannot create bindings.
Bindings record the actual approved anchor, credentials, endpoint/reference
name and an unextendable monotonic lifetime including local certificate expiry.
RPC handlers must call `Binding::authorize` against current trust and monitor
its deadline and store invalidations; the foundation itself is not a workload
dispatcher or cancellation supervisor.

The inbound binding service also watches trust publications and its fixed
deadline, dropping connections whose credentials become ineligible. Workload
handlers must separately gate output and retain charges until workers finish.

Unrestricted client roots must not overlap federation issuers, including
reissued certificates with the same issuer key. The overlap check rejects that
configuration; separate listener ports alone are insufficient.

The current rustls-webpki implementation rejects unsupported URI name
constraints rather than ignoring them. Such issuers cannot be used until the
TLS stack supports those constraints; there is no custom constraint bypass.

## Control-plane milestone

`control::ControlService` implements the `mdma.federation.v1.Federation`
`Identify` and `ScanCapabilities` RPCs. Use `into_server` for bounded control
messages on a dedicated authenticated listener. Requests bind their origin to
the TLS ingress and destination to the local identity. Each call checks current
grants and a separate bounded per-peer control budget. Identify does not change
enrollment or confer subsequent permissions.

Without a native service the catalog is empty. With normal client serving,
explicit per-recipient `catalog_exports` can disclose frozen native CPU GGUF
and CPU Laya entries with current exact artifact/executable-profile identities.
Absent grants disclose nothing; unknown, unavailable, ordinary/unfrozen
and unsupported-profile entries are omitted. It signs with
one explicitly selected, locally approved key ID/private key. Approval, immutable
ID history, retirement and material denials apply to local signers too; there is
no automatic fallback.

`catalog_cache::Cache` permits one outstanding two-second challenge per peer.
Cancellation consumes its nonce; delayed responses, changed trust generations,
invalid signatures and wrong identities/challenges cannot publish a snapshot.
Successful verification replaces the whole snapshot. Eligibility is rechecked
on every use, with both wall-clock expiry and an unextendable monotonic deadline
capped by the signing window. Outbound polling does not grant the peer access to
the local catalog. A verified cache entry still confers **no workload or caller
disclosure permission**.

Tests exercise actual loopback HTTP/2+mTLS control exchange as well as signature,
permission, timeout, revocation and cache cases. Opt-in fleet fixtures also
exercise bidirectional signed empty catalogs, wrong-nonce and forged-origin
rejection between separately launched server processes on physical hosts.
Those earlier control-only tests do not establish inference or GPU capability;
the later native acceptance scope is recorded in the runtime document.

## Explicit server integration

Set `MDMA_FEDERATION_CONFIG` to a reviewed runtime TOML file to enable the
dedicated listener alongside normal client serving. Without this variable,
no federation files are read and no federation socket is opened.

```toml
server_id = "alpha.mdma.example"
listen = "127.0.0.1:50052"
trust_state = "/private/federation/trust.json"
tls_name = "alpha.mdma.example"
tls_cert = "/private/federation/tls.pem"
tls_key = "/private/federation/tls-key.pem"
signer_id = "ed25519:current"
signer_key = "/private/federation/catalog.pk8"
# Optional and independently opt-in; false when omitted.
poll_catalogs = false

[[identity_roots]]
path = "/private/federation/issuer.pem"
# Replace with the independently reviewed SHA-256 of the complete root DER.
certificate_sha256 = "sha256:<64 lowercase hexadecimal digits>"
```

Paths are operator-supplied, relative to the process working directory unless
absolute. The catalog key is DER PKCS#8; TLS credentials are PEM. The selected
signer must already be explicitly approved in the persistent store.
Loading this file **never initializes, enrolls, repairs or activates trust**.
Administrative changes currently require stopping this process and using the
store APIs; there is no live administrative RPC or automatic file reload.

Preparation verifies the local identity under its explicitly pinned issuer,
including service-name SAN and issuer lifetime. It rejects shared client/
federation leaf keys and overlapping issuer keys, covering both local issuers
and all peer roots. Client/federation listeners must be separate. Explicit
catalog-only grants are supported. Execution grants and imports require their
independent explicit workload switches and native-service integration.
Catalog visibility alone never enables delegated inference.

For initial activation without models, set `MDMA_FEDERATION_CONTROL_ONLY=1`.
This requires the runtime config and CPU backend, rejects `MDMA_MODELS_DIR`,
and initializes no compute backend or model registry.
It opens only the federation listener. Normal client-domain TLS certificate/
CA inputs remain required, and `MDMA_LISTEN` (default `127.0.0.1:6362`) is
still checked for isolation; that client address is not bound in bootstrap mode. Normal machine-lock and signal/drain
supervision still apply; this is not a second-daemon bypass.

Set `poll_catalogs = true` only to enable outbound control polling of already
active, explicitly enrolled peers. The exact loaded credential must also pass
client-auth purpose and identity/issuer checks. Polling never activates peers,
changes grants, discovers endpoints or makes a model routable.

Each poll owns a fresh, directly dialed TLS 1.3/HTTP/2 connection. Its principal
comes from the actual completed rustls connection before HTTP dispatch, not
`Identify`, headers or negotiated metadata. Current endpoint/reference name,
trust generation, TLS pins and signer eligibility are checked again.
There are no proxy, redirect, connection-pool or plaintext fallback paths.
Rama's DNS/transport connectors only resolve the reviewed endpoint; DNS is
not enrollment.

The two-second budget includes dialing, TLS handshake, response receipt and
verification. There is at most one outstanding operation per peer and eight
globally; successful polls refresh in 18–22 seconds. Failures use jittered
4–60-second bounded backoff, without extending previous catalog expiry.
Trust notifications cancel all outstanding connections/challenges and
withdraw cached metadata; shutdown also withdraws the runtime cache.
Bad replies never replace the last verified snapshot. This cache is metadata,
not a workload or caller-disclosure permission.

## Frozen native artifact provenance

Ordinary servers retain the existing loader. With federation configured, native
loads instead capture the bytes before parsing/uploading; this does **not**
enable model exports or workload grants.

Each unique GGUF is streamed into a private anonymous temporary file while
hashing its entire contents, including metadata, tokenizer and trailing bytes.
Aliases share that snapshot and one upload, retaining their own context caps.
Source changes during capture fail; later pathname replacement or writes do
not alter the loaded bytes or their digest. Snapshot files are dropped after
upload. This requires temporary disk capacity approximately equal to the unique
GGUF set, in addition to normal model memory; it is not a model transfer.

Laya captures all five consumed files: `model.safetensors`,
`rl_agent_config.json`, `encoder/config.json`, `tokenizer/tokenizer.json` and
`tokenizer/tokenizer_config.json`. Every parser consumes those captured bytes.
Its digest is the canonical, path-sorted `mdma.laya-artifact.v1` manifest,
including the original tokenizer bytes before in-memory canonicalization.
Logical relative paths, not checkpoint location or symlink targets, are hashed;
unconsumed files are excluded.

`Registry::artifact_id` is read-only load provenance, not an export decision.
Ordinary/library-constructed decoders and unavailable native entries
have no publishable identity. Warm-up failure withdraws all affected aliases.
These snapshots protect against accidental concurrent writers/path changes;
they are not OS sealing or attestation against the operator/root.

Frozen GGUF loads bypass prepared caches: a cache's self-declared source digest
does not establish equivalence of its derived payload. `MDMA_CACHE=required`
therefore fails closed for these loads until a separate equivalence gate exists.
Ordinary nonfederated cache behavior is unchanged.

CPU [execution profiles](execution-profile-cpu-v1.md) now bind the actual
kernel-identified executable digest and fixed native numerical/kernel modes.
Frozen native CPU decoders and CPU Laya receive them. Concrete GPU decoders
use separate [GPU profiles](execution-profile-gpu-v1.md), never a manual CPU label.

## Native catalog publication

The federation listener reads the **same immutable loaded registry**, power
receiver and shutdown guard as the native client service. It never calls
other nodes, transfers models, reopens model paths or invents descriptors from
grants. CPU Laya remains CPU even in a GPU daemon. Concrete Metal, ROCm and
Vulkan decoders can export current loader-created GPU profiles; unsupported
backends cannot invent descriptors from grants.
Aliases retain their individual context caps while sharing artifact/profile IDs.

Catalog state follows serving/degraded/critical/draining; native load failures
also degrade serving state. Critical/draining models are unready. A closed power
watcher fails closed as critical unless already draining. During power
degradation, decoder context and output caps use the same native admission
limits. These request-dependent caps are not part of the numerical profile;
power transitions do not change its digest. Readiness is **not** a promise of
memory admission, execution permission, routing permission or shared capacity.

Input/decision limits are bounded by the v1 protocol ceilings (65,536 input
bytes, 128 questions and 524,288 result bytes); native delegated handlers enforce
those limits. The source is re-read immediately before
publication: changes in readiness, availability, caps or executable provenance
abort the response. Current trust generation, peer authorization and signer
eligibility are also rechecked. Oversized canonical bodies are never signed.
Control-only catalogs now report draining through the listener shutdown guard.

Local and real loopback mTLS tests cover nonempty signed catalogs, recipient
filtering, removal of disclosure grants, live power/drain transitions and
mid-signing inventory/trust changes. Earlier physical-host proofs remain
**empty-catalog control-plane proofs**, not native inference proofs.

Administrative CLI/live administration remains unfinished. Catalog visibility
is never execution or caller-disclosure consent.
See the independently gated execution and origin integration below.

## Execution foundations

Private execution primitives now provide a process-local `(origin, 16-byte ID)`
ledger shared across operations. Validation already consumes concurrency.
Duplicates never replay/attach; claimed validation failures and concurrency
rejections leave tombstones. Active entries cannot expire, and terminal IDs
remain for 300 seconds **after actual finish**, not admission. Rate windows and
ID retention are independent. Policy changes cannot evict retained IDs.
The initial local federation ceilings are one active claim, 16,384 entries and
128 peers, in addition to each peer's reviewed limits. Full ledgers reject work;
no live/unexpired eviction, persistence, input/result storage or restart
exactly-once guarantee is provided.

A claim moves into the actual blocking job. Dropping/aborting its handle cannot
release running-job charges. The job only returns a terminal outcome; its
captures are dropped before accounting completes. Panic/unstarted cancellation
defaults to failure; poisoned accounting fails closed instead of reconstructing
empty state. Queued Tokio work remains charged until its captures are destroyed.
The synthesis integration below uses a dedicated bounded native thread rather
than queueing inference behind the shared Tokio blocking pool.

Deadline primitives require exactly one positive `grpc-timeout` with 1–8 ASCII
digits and a valid gRPC unit. The budget is anchored at HTTP ingress, before
body decoding, and capped by the 1–300,000ms envelope budget, local 300-second
ceiling and actual TLS binding lifetime. Dispatch budgets round **down** after
elapsed negotiation/validation, never reset or round up. Rama's general timeout
layer alone does not enforce this contract: it tolerates missing/malformed
headers and wraps response creation, not the full stream/worker lifecycle.

These primitives are wired into an internal native synthesis controller.
Ordinary client inference is unchanged unless an explicit approved route alias
is selected. Workload grants require the default-off runtime activation gates.

## Tracked native generation

Native generation now shares one preparation/compute loop between ordinary
serving and a private tracked path. Preparation tokenizes once, resolves
prompt/output/context/sampler limits and reserves native KV/workspace memory
without allocating or launching compute. Dropping preparation refunds that
reservation. Federated preparation rejects zero output, input above 65,536
bytes and invalid/nonfinite sampler values. It always uses isolated KV state,
even if the local Session opted into prefix reuse; ordinary zero-output,
prefix-cache and sampling behavior remain supported.

The tracked job reports emitted count and `eos`, `max_tokens` or `context_limit`
only through its **actual join** after decoder/KV/workspace teardown and guard
release. When requested output equals the remaining context, `max_tokens` wins;
`context_limit` means the context reduced the requested cap. Empty/partial
channel EOF, backend errors, teardown panic or cancellation cannot stand in for
successful completion. The ledger/native bridge joins the accounting job
before consuming its internal report, so receipt of that report alone also
cannot release charges or establish RPC completion.

Native handoff uses eight worst-case 64KiB slots, reserved **before text decoding**.
Queued, producer and consumer-held token guards share that 512KiB bound, leaving
room for protocol encoding and transport handoff.
Text above 60KiB fails rather than entering the queue, leaving protobuf
overhead within 64KiB. This bounds valid native handoff events, not transient
tokenizer allocations or process/driver memory. Cancellation interrupts both
slot and channel waits even with the receiver retained. Compute stops at the
next safe boundary; memory and ID charges stay held through in-flight prefill,
queued-job cleanup and decoder teardown. Normal EOF does not manufacture caller
cancellation before the completion adapter runs.

Tests cover actual native memory/ledger release, greedy parity, context/EOS
counts, prefix isolation, full/consumer-held buffers, oversized events, failed
or panicking workers, queued disconnect and cancellation during prefill.
## Private authenticated synthesis controller (not activated)

The wire schema now includes native-only `Synthesize`. The installed runtime
still has no execution controller and returns `UNIMPLEMENTED`; startup still
rejects execution/import policies. A crate-private attachment is exercised by
synthetic conformance tests, including real HTTP/2 mTLS loopback execution.

The controller checks the actual transport binding and required ingress
receipt, origin/destination, current operation grant and exact loaded
artifact/profile. Permission precedes model existence checks. Rate/ID and
node/peer concurrency charges precede tokenization. Resolved context, output
cap and sampler presence/defaults are returned in the first `Accepted` event.
Power/readiness/selection and trust generation are checked again before that
event and before launching the job. Compute cannot begin before `Accepted`.
Current grants and fixed deadline are checked again at actual native start and
each safe compute/output boundary, including queued-slot handoff.

Each claimed native job gets a dedicated thread; inference cannot queue behind
ordinary Tokio blocking work. Only joining a finished thread uses the shared
pool, and the ID/concurrency claim remains charged through that actual join.
Failed launch, panic, partial output, cancellation and missing reports cannot
produce successful completion. `Completed` requires joined resources and
matching emitted counts; authorization/deadline are checked again before EOF.

Independent supervision watches trust edits, monotonic deadline and current
credential/pin validity even while a receiver is idle. It cancels the native
handoff and drops unstarted reservations; running jobs keep their own charge
through cleanup. A separate node response permit is held by both the actual
worker and response, so neither worker completion nor response disconnect
alone creates unlimited undrained output. No input/output logging was added.
Errors are fixed sanitized reasons, never native exception/path/input text.

The runtime also bounds connections (eight, including handshake/idle lifetime)
and concurrent HTTP body decoding (two, no wait queue). Workload upload decoding
is capped by its ingress deadline; messages are bounded to 96KiB. The synthesis
stream deliberately yields pending between events so Rama's encoder flushes
one event per chunk rather than batching unbounded tiny tokens. The dedicated
HTTP/2 listener uses a one-byte send-buffer threshold and two streams per
connection. Valid native handoff is at most 512KiB/eight events; each encoded
token chunk is below 64KiB, with bounded single-event encoder/source and HTTP/2
handoff. These are buffering bounds, not a cap on kernel socket buffers,
tokenizer transient allocation, TLS overhead or measured process residency.

The origin integration below additionally gates caller disclosure, imports and
adversarial response validation. The separate physical acceptance results and
limitations are recorded in the runtime document; loopback is not a fleet proof.

## Native typed decisions

The wire schema and private native controller now also implement unary `Decide`,
sharing the same authenticated envelope, exact selection, rate/ID ledger,
native-thread admission and node response quota with synthesis. A synthesis or
catalog grant does not authorize a decision. The native lookup can only select
a frozen, currently profiled local CPU Laya checkpoint; no default fallback
or peer forwarding occurs.

Combined input JSON is capped at 65,536 UTF-8 bytes and questions at 128.
Validation/preparation uses the same actual Predictor algorithm as ordinary
serving; checked eligibility runs between question tokenizations and before/
after workspace allocation and each actual forward batch. Empty question
objects preserve ordinary semantics. Schema/backend errors are sanitized.
The prepared batch reserves its checked peak logical workspace before launch,
including the collator's minimum two-option padding. This is an independent
128MiB federation Laya scratch budget; checkpoint weights, JSON/tokenizer
transients and process/driver memory are not claimed as part of that budget.

Cancellation, deadline and current execution-grant removal can terminate the
RPC while a native batch is still in flight. The worker retains its scratch,
shutdown and concurrency charges through the safe boundary and actual join.
Response identity/result only become eligible after that join and a final
current authorization/deadline check. Serialization uses a bounded writer,
never first renders an oversized result String; result JSON is capped at
512KiB. A response lease follows the actual encoded HTTP body and checks
current output eligibility before releasing frames, even after native compute
has finished. A post-response body failure is an ambiguous non-success, not
permission to retry.

Tests include actual HTTP/2 mTLS unary success and client reset, canceled/
revoked/deadline-expired held workers, preparation refund/budget denial, shared
cross-operation duplicate IDs, input/result/schema limits, empty questions,
sampler-independent decision grants and retained response quotas. Ordinary
prediction parity, safe batch boundaries and padded/overflow workspace
accounting are also tested. Runtime attachment/activation remains default-off,
requiring explicit native-service integration and reviewed permissions.
