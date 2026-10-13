# Federation v1: routing and execution

**Draft.** See [identity and catalogs](identity-and-catalog-v1.md).

## 1. Routing and disclosure

The origin selects a peer from an unexpired verified catalog, constrained
by explicit local route imports and the local caller's disclosure policy.
No discovery is triggered by user-provided URLs, remote defaults or catalog
contents. Local aliases resolve to explicit remote model names, artifacts
and profiles.

Selection is pinned for the request. A request cannot change peers,
artifacts or profiles halfway through execution. If several targets are
explicitly authorized, local selection may use readiness and locally
measured latency, but cannot treat advertised capacity as reserved capacity.
Expired catalogs, unready models and nonaccepting nodes are not eligible.
Peer state, signing-key validity, disclosure/import grants and TLS binding
eligibility must remain valid at dispatch. A pooled connection or earlier
`Identify` is not authorization under the current trust policy.

The executor MUST resolve the selected model to a local GGUF or Laya entry.
It MUST NOT call another MDMA node or return a redirect. V1 does not accept a hop count permitting onward forwarding.
This prevents loops by construction.

An authenticated peer represents itself, not its individual end users.
Caller-provided user IDs or tracing metadata MUST NOT grant rights or
escape that peer's quotas. Per-user isolation at the origin is local policy.
Until cross-peer cache isolation is separately designed, federated requests
MUST NOT share prompt/KV prefix caches.

## 2. Request semantics

Both execution RPCs carry a common required envelope:

| Field | Meaning |
|---|---|
| `origin` | Authenticated requesting server |
| `destination` | Local executing server |
| `request_id` | Random 128-bit ID; 16 bytes, not a prompt hash |
| `model` | Explicit nonempty exported model name |
| `artifact_id` | Exact selected artifact digest |
| `profile_id` | Exact selected execution-profile digest |
| `timeout_ms` | Remaining positive execution budget, at most 300000 |

Human-readable request IDs use 22-character URL-safe unpadded Base64.
IDs are unique per origin across both execution operations.

`Synthesize` additionally carries `prompt`, positive `max_tokens` and the
existing optional sampler fields. Missing fields retain presence semantics:
they are not zero. The executor resolves them against its defaults.
The origin may supply explicit settings when local policy requires them.
Invalid/nonfinite sampler values are rejected, not canonicalized into
signed catalog metadata. Prompts are raw text; federation adds no chat
template rendering.

`Decide` carries `state_json` and `questions_json` with the semantics of the
descriptor's `decision`: Laya's typed questions, or MiniCheck's document and
claims (see [serving](../serving.md#grpc-api)). Their combined UTF-8 size is
bounded by `max_input_bytes`; JSON/schema validation and question limits apply.
The origin checks the result against the same contract: Laya's typed answers,
or exactly one `{"supported", "probability"}` per claim with `supported` true
only above 0.5 and no output tokens. A MiniCheck request runs its claims on
one prefix-reusing session of its own; nothing is shared across requests.
Neither operation silently switches models.

Every execution call MUST have a gRPC deadline. The origin subtracts
negotiation, catalog and routing time from the caller's budget. The executor
uses the minimum of the gRPC deadline, `timeout_ms` and its local limit.
Forwarding MUST NOT reset the original deadline. Execution budgets are
measured with monotonic clocks; catalog timestamp skew is unrelated.

The advertised input bound is a byte bound, not a token count. The executor
tokenizes and applies its actual trained/model/power context limits.
For `Synthesize`, the accepted output cap may be reduced by these limits
as in local serving, but the resolved cap MUST be reported in admission.
Power caps apply here, not in the catalog, which keeps the configured maxima.
An overlong prompt or a request with no permissible output space is rejected.

## 3. Admission and duplicate IDs

Before starting model execution, the executor MUST:

1. Validate message bounds, the peer's mTLS binding, origin and destination.
2. Enforce request-rate and pre-admission work limits.
3. Claim `(origin, request_id)` atomically against concurrent duplicates.
4. Check current model/operation authorization and exact artifact/profile.
5. Validate inputs and resolve effective parameters and context caps.
6. Recheck power/drain state, concurrency and memory/resource admission.

Before sending `Accepted` or starting unary inference, revalidate against
the current trust/policy generation. Removed credentials or grants cannot
be bypassed by validation begun under an older generation. Credential expiry
also caps the call's lifetime.

Authorization is checked even if a recent catalog advertised the model.
Peer/model quotas must count validating, queued, admitted and canceling work,
as applicable; no unbounded queue precedes admission. A request waiting for a
worker still consumes its deadline.

A receiver MUST maintain active IDs until their workers finish and retain
terminal tombstones for at least 300 seconds in the running process.
Duplicate IDs return `ALREADY_EXISTS`; they never attach to an old stream
or replay a result, regardless of whether the payload differs.
Tombstones contain identity, ID, terminal status and timestamps, not inputs.

The default per-peer ledger bound is 1024 active/tombstone entries.
If full, reject new work rather than evicting live or unexpired entries.
Failures after claiming an ID also leave a tombstone. Operators can tune
the bound and request rate together.

This is process-local deduplication, NOT an exactly-once guarantee. Restart
may lose all entries, and retention eventually ends. Callers must not
redispatch ambiguous requests. There is no durable request journal or
recoverable result store in v1.

An admission reservation can still be followed by allocation or backend
failure. Advertisements and `Accepted` do not guarantee successful completion.

## 4. Synthesis stream

A successful `Synthesize` stream has this shape:

```text
Accepted -> Token* -> Completed -> gRPC OK
```

`Accepted` is the first event, emitted only after admission. It includes
`request_id`, executor identity, selected model/artifact/profile, effective
sampler configuration, prompt token count and effective output/context caps.
It is sent before token generation, but its loss does not prove admission
never occurred.

The origin MUST verify the admission identity tuple, request ID and
effective parameters against its selected target and local constraints.
Unary response identities require the same checks. A mismatch cancels
the call as a protocol failure, without redispatch.

Each `Token` contains a zero-based contiguous `index`, the existing
`token_id` and decoded `text`. Text may be empty. IDs are interpreted with
the selected artifact's tokenizer; they cannot be combined across models.
The origin preserves event order and content without resampling.

`Completed` includes the request ID, emitted token count and a finish reason:
`eos`, `max_tokens` or `context_limit`. Counts refer to emitted token events,
not hidden EOS handling. It also carries the executor's `prefill_ms` (worker
launch to first token) and `decode_ms` (first to last token); together they
cannot exceed the request's `timeout_ms`. Completion is emitted only once the
worker has finished and its execution resources can safely be released.

Success requires both a valid `Completed` event and gRPC `OK` termination.
An empty stream, missing/duplicate completion, out-of-order token index,
extra events after completion or `OK` without completion is a protocol
failure. A valid completion followed by transport failure is not retried.

Before admission, errors terminate with a non-OK gRPC status and no events.
After admission, failures may leave a partial stream and terminate non-OK.
They MUST NOT emit a successful `Completed` event. The origin must preserve
partial-result/failure semantics; it cannot append output from another peer.

For unary `Decide`, successful admission and completion are local state
transitions. Its response includes the request ID, selected identity tuple
and `result_json`, followed by gRPC `OK`. Loss of the response is ambiguous;
there is no streaming `Accepted` acknowledgement for this operation.

## 5. Retry, cancellation and backpressure

`Identify` and `ScanCapabilities` can retry with bounded exponential backoff and
jitter. Authentication failures do not loop. Catalog retries use new nonces.

Inference RPCs MUST NOT have service-config retries, application retries or
hedging. Automatic target failover is permitted only before dispatching an
execution RPC. No tokens received, no admission event received, a timeout,
`UNAVAILABLE` or a connection reset does not establish that execution never
started. Transport retry behavior must be audited before implementation.

An explicit new user request receives a new ID; it is a new execution, not
recovery of the old one. After a transport failure, its predecessor's outcome
may remain unknown. V1 offers no resume or status-query RPC.

Client cancellation, deadline expiry, disconnect or peer revocation MUST
propagate to the executor. Stop admitting new work and stop inference at
the next safe compute boundary. In-flight GPU operations may not be
immediately preemptible. Hold resource charges until those operations have
finished; releasing a stream is not proof that its memory is reusable.

Credential expiry and removed execution/import/disclosure grants follow
the same cancellation boundary under the
[identity lifecycle](identity-lifecycle-v1.md#6-revocation-races-and-recovery).
Routine signing-key retirement or catalog expiry alone does not cancel
already admitted work on otherwise authorized TLS bindings.

Stream buffering MUST be bounded to 64 events and 1 MiB total, whichever
is reached first. Slow receivers apply backpressure at safe boundaries.
Deadline/cancellation checks remain active while writes are blocked.
Do not spool prompts/results to disk to accommodate slow clients.

### Leases

An origin keeps a routed model loaded on its executor with
`AcquireLease{origin, destination, model, artifact_id, profile_id, ttl_ms}`,
`RenewLease{lease_id, ttl_ms}` and `ReleaseLease{lease_id}`. Acquire and Renew
need the same Execute grant as the model's operation and count against the
peer's control budget; Release needs only an authenticated binding. A changed
artifact or profile is `FAILED_PRECONDITION / selection_changed`, and an
ungranted model is `PERMISSION_DENIED` whether or not it exists.

The executor starts the load in the background and answers at once with
`resident`; its catalog reports when the model is loaded. A peer holds at most
4 leases. A lease ends after `ttl_ms` (5 s to 10 min, 60 s by default) unless
renewed, so a vanished origin, or one whose grant was removed (it can no
longer renew), lets the model go within one TTL. Leases are not shared between
peers or with local clients, and ids are only valid for the peer that took
them.

The origin holds one executor lease per routed alias while any local lease on
that alias is live, renews it every 20 s with a 60 s TTL, retakes it if the
executor forgot it, and releases it with the last local lease.

## 6. Errors, bounds and observability

Use ordinary gRPC status codes, with sanitized machine-readable reasons:

| Situation | Status / reason |
|---|---|
| Identity/origin mismatch after TLS | `UNAUTHENTICATED / identity_mismatch` |
| Peer/model/operation not authorized | `PERMISSION_DENIED / forbidden` |
| Bad envelope, sampler or input schema | `INVALID_ARGUMENT / invalid_request` |
| Authorized model no longer present | `NOT_FOUND / model_unavailable` |
| Artifact/profile changed or nonlocal entry | `FAILED_PRECONDITION / selection_changed` |
| Duplicate ID | `ALREADY_EXISTS / duplicate_request_id` |
| Context, concurrency, ledger or memory exhausted | `RESOURCE_EXHAUSTED / limit_exceeded` |
| Critical/draining node | `UNAVAILABLE / not_accepting` |
| Deadline or cancellation | `DEADLINE_EXCEEDED` or `CANCELLED` |
| Backend/internal failure | `INTERNAL / execution_failed` |
| Invalid response identity or stream shape | `DATA_LOSS / protocol_violation` |

Check permission before exposing whether a named model exists. Error messages
must not contain private paths, secrets or input text. Structured details
may distinguish `not_admitted`, `admitted` and `unknown`, but are diagnostic,
not permission to automatically retry. Bare transport errors imply unknown.

Default message bounds: 1 MiB uncompressed per gRPC message, 64 KiB per
token event, 64 KiB total execution input, and 512 KiB for `result_json`.
Descriptors can impose smaller limits. Pre-admission decoding/tokenization,
control calls and connection counts also require local bounded budgets.
The initial per-peer inference concurrency limit is one, configurable only
within the node's own supported scheduling/resource envelope.
The initial queue capacity is zero; later local bounded queues must charge
waiting requests to an explicit per-peer in-flight budget.

Default logs contain peer, random request ID, model identity, operation,
admission/outcome, timing and aggregate token counts. Do not log prompts,
decision inputs, output text or authorization secrets. Remote metrics and
headers are untrusted; bound cardinality and allowlist propagated metadata.

## 7. Conformance gates

Future implementation tests MUST cover:

- No permissions by default; catalog visibility without execution permission.
- Local caller disclosure restrictions and no onward execution.
- Artifact/profile changes between advertisement and admission.
- Presence-preserving sampler forwarding and receiver-resolved limits.
- Concurrent duplicate IDs, full ledgers and documented restart behavior.
- No redispatch when failure occurs before the first token or after admission.
- Valid success, partial failure, malformed stream and lost unary responses.
- Deadline propagation, slow consumers, disconnect and cancellation during GPU work.
- Charges retained until workers finish; draining and peer revocation.
- Revocation between validation and admission; credential expiry during streams.
- Removed disclosure grants and stale pooled bindings without redispatch.
- No input/output leakage in catalogs, errors, logs or shared prefix caches.

These tests need synthetic CPU fixtures and fault-injected peers first.
GPU or full-checkpoint tests are additional gates, not prerequisites for
proving the protocol's control flow.
