# MDMA federation

**Status: v1 implemented as an opt-in, single-hop runtime.** Federation is
disabled by default.

The [security foundation](federation/security-foundation.md) provides
enrollment, strict mTLS binding and an explicitly configured dedicated server
listener. Control RPCs exchange authenticated identities and recipient-filtered
signed native catalogs over HTTP/2; outbound polling maintains a verified
expiring cache with bounded deadlines, jitter and backoff. Separate explicit
switches enable native CPU/GPU execution and pinned single-hop routes with
local caller disclosure. See the [routing runtime](federation/routing-runtime.md)
for supported modes, gates and acceptance limits.

The separate [LAN resource POC](federation/poc-resources.md) starts
with CPU, memory and disk advertisements and a machine-wide daemon lock.
It does not implement the security or execution protocol described below.

Federation connects independently operated MDMA servers. Each operator owns
its models, identities, admission policy and resources. A node can request
inference from an explicitly trusted peer without a central scheduler or
shared certificate authority.

This is inference-service federation, not federated learning or splitting
one model's tensors across machines.

## Reading order

1. This document: scope, architectural boundaries and decisions.
2. [Identity and catalogs](federation/identity-and-catalog-v1.md): trust,
   authenticated peers, signed advertisements and model identity.
3. [Identity/key lifecycle](federation/identity-lifecycle-v1.md): enrollment,
   transport pins, credential windows, rotation, revocation and recovery.
4. [Execution](federation/execution-v1.md): routing, admission, streaming,
   cancellation and failure semantics.
5. [Matrix reference](federation/matrix-reference.md): pinned upstream
   specification and deliberate departures.

The uppercase requirements in the protocol documents are proposed v1
requirements. The message descriptions define semantics, not frozen
Protobuf field numbers.

## V1 scope

```text
local client -> origin MDMA -> execution MDMA
                    |              |
              local policy    independent policy
                    +---- authenticated mTLS gRPC ----+
```

- Invite-only peers with bilateral, potentially asymmetric permissions.
- Explicitly configured HTTPS endpoints; no open discovery.
- Peer-specific, signed, short-lived catalogs of locally hosted models.
- Whole-request text synthesis and typed-decision delegation, one federation hop.
- Direct token streaming, bounded resource use and cancellation.
- No automatic inference retries after dispatch.

Not included: public registration, gossip, multi-hop inference, remote model
downloads, tensor/KV transfer, replicated conversations, global consensus,
payment settlement, portable end-user identity, durable job queues or stream
resumption.

Being reachable, having a valid certificate, having a pinned signing key,
being allowed to view a model, and being allowed to execute that model are
different things.

## Client and federation boundary

The client-facing [`Mdma` API](../proto/mdma.proto) has `Synthesize`, `Decide`,
`Introspect` and `Watch`. Federation routes are the only way a node serves
another node's models.

The proposed `mdma.federation.v1.Federation` service defines:

| RPC | Purpose |
|---|---|
| `Identify` | Confirm authenticated server identity and protocol support |
| `ScanCapabilities` | Obtain a signed, recipient-specific full snapshot |
| `Synthesize` | Execute a local decoder and stream admission/tokens/completion |
| `Decide` | Execute a local typed-decision model and return its result |

The client and federation services use distinct RPC paths, message types and
authorization policies. Federation's `Synthesize` and `Decide` require an
identity/model/request envelope and the federation execution lifecycle.
Catalog execution-operation labels are `synthesize` and `decide`, respectively.
`ScanCapabilities` polls the configured peer's authorized catalog; it does
not scan the network or discover peers.

Federation is disabled by default. The client service and registry remain
available independently. Enabling federation requires the safe credential
separation described below. There is no automatic upgrade or fallback between
client RPCs and federation RPCs.

The initial implementation should use a separate federation listener.
Whether listeners are separate or shared, federation credentials must not
reach client RPCs to bypass federation policy. Keep peer issuers out of
the client trust store; overlapping roots require independent client
identity allowlists and rejection of federation-profile certificates before
enabling federation.

## Control plane versus execution plane

The control plane describes what a peer is currently willing to offer.
The execution plane asks that peer to do a particular piece of work.

Catalogs do not reserve GPU capacity. The executor rechecks authorization,
the selected artifact and execution profile, power state, context limits,
concurrency and memory before admitting each request.

Only metadata crosses the control plane. Prompts, decision inputs, tokens
and results are not replicated or broadcast.

A remote executor sees the inputs and outputs it processes. mTLS protects
transport, not secrecy from that operator. The origin must obtain local
authorization to disclose inputs to the selected operator.

## Draft decisions

| Area | V1 decision |
|---|---|
| Identity | Stable configured server name; exact host-bearing certificate identity URI; role-specific TLS and metadata-signing pins |
| Transport | TLS 1.3 full-handshake mTLS; no global CA, resumption or early data |
| Enrollment | Out-of-band public bundle verification; staged/control-only activation before workload grants |
| Revocation | Persistent denied key material; live binding/stream invalidation; no old-key-authorized recovery |
| Discovery | Explicit peer endpoints only |
| Catalogs | Pull full snapshots; challenge-bound, signed and expiring |
| Export | Local native models only, filtered per peer |
| Routing | Explicit local aliases and allowed targets; no remote-default lookup |
| Execution | Exactly one federation hop; never proxy incoming work onward |
| Model matching | Complete artifact digest plus execution-profile digest |
| Retry | Control-plane retries allowed; inference never automatically redispatched |
| Deduplication | Active IDs plus bounded terminal tombstones; process-local, not crash-safe |
| Completion | Explicit completion event plus successful gRPC termination |

V1 uses poll-based catalogs rather than gossip or event synchronization.
Full replacement snapshots make withdrawals unambiguous, and
per-poll challenges avoid relying on a persistent global event sequence.

Matching artifacts does not guarantee bit-identical numerical results across
backends. Availability and execution claims remain claims by a trusted
operator, not remotely verifiable proof of its hardware or computation.

## Initial implementation sequence

1. Peer configuration, enrollment and certificate-to-server/SPKI binding;
   deny by default and enforce lifecycle changes on active bindings.
2. `Identify`, signed catalog generation/verification and exact artifact identity.
3. Explicit route import and recipient/model/operation authorization.
4. Local-only federation execution, admission and stream lifecycle.
5. Resource bounds, cancellation, failure injection and interoperability tests.

Use established implementations for networking, TLS, Ed25519, Base64 and
canonical JSON. Keep federation outside compute kernels and backend traits.
Reuse local inference/admission mechanisms on the executor.

## Review before freezing

- Validate the lifecycle profile against the chosen TLS stack: host-bearing
  URI constraints, role-specific SPKI pins, full-handshake controls and
  authenticated binding data. The documented design is not yet a tested
  certificate fixture or implementation.
- TLS/private-key storage and persistent trust/revocation storage design,
  including backup restoration procedures.
- Verify that client-listener trust cannot bypass federation grants.
  Shared roots need additional client authorization, not merely
  a second listening socket.
- Full-artifact hashing cost for large GGUFs and directory checkpoints.
- The distinction between artifact identity and execution-profile identity.
- Default request/catalog limits and whether the proposed deadlines fit use.
- The explicit process-local deduplication boundary. Crash-safe execution
  records would require a separate persistence design before adding retries.

The conformance requirements in the protocol documents are acceptance
criteria for future implementation, not tests claimed to pass today.
