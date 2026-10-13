# Federation v1: identity and catalogs

**Draft.** See the [scope and decisions](../federation.md) and the
[identity/key lifecycle](identity-lifecycle-v1.md).

## 1. Server identity and peer configuration

`server_id` is an operator-selected, lowercase ASCII DNS-form name with at
least two nonempty labels: at most 63 bytes per label, letters/digits/interior
hyphens, at most 253 bytes overall, no port, scheme or trailing dot. IDNs use
their ASCII A-label representation. IP literals are not server IDs.
Examples: `alice.example.net`, or `node-a.mdma.test` for private deployments.

The name is stable when endpoints or keys change. In v1 it is an
out-of-band configured identifier, not proof of DNS ownership. Duplicate
peer names and a peer name equal to the local identity MUST be rejected.

Each local peer record MUST contain:

| Field | Meaning |
|---|---|
| `server_id` | Expected remote identity |
| `endpoint` | Explicit HTTPS origin, including port |
| `tls_name` | Expected remote certificate DNS name or IP address |
| `ca_roots` | Roots authorized to authenticate this peer |
| `tls_pins` | Role-specific accepted leaf SPKI hashes and finite validity windows |
| `signing_keys` | Pinned key IDs, Ed25519 public keys and validity intervals |
| `state` | Explicit staged/active/suspended/revoked enrollment state |
| `generation` | Local trust/policy publication generation, not a wire field |
| `revocations` | Persistent denied credential material and signing-ID history |
| `control_permissions` | Explicit `Identify` and `ScanCapabilities` grants |
| `catalog_exports` | Local models the peer may see |
| `execute_exports` | Local models/operations the peer may execute |
| `route_imports` | Remote models/profiles local callers may use |
| `limits` | Local per-peer request, concurrency and control-plane budgets |

No permissions are granted by default. Permission to read a catalog does
not grant execution. Trust is not transitive or necessarily reciprocal.
Endpoints MUST NOT contain user information, paths other than `/`, query
strings or fragments. Redirects MUST NOT be followed. Catalog contents
cannot introduce endpoints or new trusted peers.

TLS verification MUST use the peer's configured roots, role-specific public-key
pins, certificate validity and applicable certificate purpose. Outbound
connections MUST additionally verify `tls_name`. Both client and server leaf
certificates MUST contain exactly one URI SAN, with the exact identity:

```text
https://<server_id>/.well-known/mdma/identity
```

This label is not fetched for discovery. The URI MUST match the peer record.
An inbound certificate must validate under roots authorized for that
specific peer, not merely any root in a
combined listener trust store. A trusted CA signature alone is insufficient
to authorize an RPC, as is a public-key pin without a valid chain. Missing
or ambiguous identities fail closed. The
[TLS binding profile](identity-lifecycle-v1.md#3-tls-binding-and-certificate-lifetime)
requires TLS 1.3 full handshakes, explicit leaf usage/purpose, no Common Name
fallback, no session resumption or early data, and client/federation credential
separation even when listeners are separate.

Private CAs may differ between operators. No insecure verification mode,
TOFU, automatic key acceptance or shared federation-wide CA is part of v1.
TLS termination must preserve this binding; v1 supports end-to-end mTLS to
MDMA, including TLS-pass-through proxies, not identity headers from an
untrusted terminating proxy.

Disabling a peer MUST evict its catalogs/routes, deny new RPCs, invalidate
its bindings and cancel its active requests. Trust updates also invalidate
pending catalog responses through their local generation. Resource release
still follows worker completion.

## 2. Negotiation and key lifecycle

All RPCs MUST independently authorize the mTLS principal and current peer
state/grants; `Identify` does not mint a session credential. Certificate/pin
expiry also applies to pooled connections and streams. `Identify` is safe to
retry and contains the caller's
`server_id` and supported major versions. The response contains the
responder's `server_id` and supported major versions.

Both identities MUST match their certificates and configuration. V1 uses
major version `1`; lack of a common supported major fails before sending
inputs. Callers MUST NOT fall back to client-facing `Mdma` RPCs. Every subsequent
RPC carries or implies major version `1` and checks it independently.

Signing keys are separate from TLS keys. Key IDs have the form
`ed25519:<label>`, where the label is 1..32 lowercase letters, digits or
hyphens. Public keys are 32 bytes. Reusing a key ID for different key
material is forbidden.

Enrollment, role-specific TLS rotation, signing windows, routine retirement
and compromise recovery follow the
[lifecycle rules](identity-lifecycle-v1.md). New keys advertised by a remote
server MUST NOT update local pins. Neither certificate renewal nor key
rotation automatically changes permissions. Suspected compromise suspends
the peer rather than performing trusted-old-key rollover.

## 3. Catalog exchange

`ScanCapabilities` requests contain `origin`, `destination` and a fresh random
32-byte `nonce`. The recipient verifies the origin against mTLS and the
destination against its own identity before exposing inventory.

The response is a `SignedCatalog` envelope:

| Field | Representation |
|---|---|
| `key_id` | Pinned signing key identifier |
| `body_json` | Canonical JSON bytes for the body below |
| `signature` | 64-byte Ed25519 signature |

Binary values use Protobuf `bytes`; human-readable configuration/examples
use URL-safe unpadded Base64. This gives 43 characters for a nonce/public
key and 86 for a signature. Receivers MUST enforce decoded lengths.

The signed body has these required fields:

| Field | Meaning |
|---|---|
| `type` | Literal `mdma.catalog.v1` |
| `origin` | Catalog issuer; matches the authenticated server |
| `destination` | Specific authorized recipient |
| `nonce` | URL-safe unpadded Base64 of the request challenge |
| `issued_at_ms` | Nonnegative Unix timestamp in milliseconds |
| `expires_at_ms` | Exclusive expiry timestamp |
| `server_state` | `serving`, `degraded`, `critical` or `draining` |
| `models` | Complete visible local export set for this recipient |
| `host` | The issuer's machine, described below |

`host` holds `name`, `os` (`linux`, `macos`, `freebsd`, ...), `os_name`
(`FreeBSD 16.0-CURRENT`), `arch`, `cpu`, `device`, `memory_bytes`, and the
live `memory_available_bytes`, `device_memory_bytes`, `device_available_bytes`,
`disk_bytes` and `disk_available_bytes` (the models folder's filesystem). Its strings
are printable ASCII of at most 128 bytes; no other keys are allowed.

Metadata MUST use Matrix v1.19's
[canonical JSON definition](matrix-reference.md), not a similarly named
canonicalization scheme. V1 rejects duplicate object keys, invalid Unicode,
floating-point numbers and unknown body/descriptor fields. Its integer
fields are restricted to `0..9007199254740991`.

The exact signature preimage is:

```text
UTF8("mdma.federation.catalog.v1\n")
  || MatrixCanonicalJSON({"body": B, "key_id": K})
```

`B` is the parsed catalog body object and `K` the envelope key ID. The
prefix ends with one LF byte, not a backslash followed by `n`.
`body_json` MUST equal the canonical encoding of `B`. The key ID and
every body field are covered. The signature itself is not in the preimage.
This is an MDMA envelope, not Matrix's signed-object container.

Receivers MUST validate envelope sizes before parsing, then canonical
encoding, the pinned key and its current validity, signature, origin,
destination, outstanding nonce, timestamps, descriptor uniqueness and schema.
The key's local validity window must cover both current time and
`issued_at_ms`. Unverified metadata MUST NOT become routable.

A catalog's lifetime MUST be positive and at most 60 seconds. Reject
`issued_at_ms` more than 30 seconds in the future, and reject a catalog
already expired at receipt. Clock tolerance does not extend its expiry.
The local monotonic lifetime MUST be capped at 60 seconds and the remaining
wall-clock lifetime at receipt, additionally capped by the key's local
`valid_until_ms`. Subsequent wall-clock adjustments cannot extend it.
Current peer state, key eligibility and import permissions are rechecked
before using cached metadata, not just when the response is verified.

At most one catalog poll per peer may be outstanding. Nonces MUST NOT be
reused or accepted after that poll has completed/timed out. Polls use a
two-second deadline and normally refresh every 20 seconds with jitter.
Failed polls use bounded backoff and never revive expired state.

After complete validation, atomically replace the cached snapshot. A valid
empty `models` array withdraws all routes for that peer. An omitted model
is withdrawn, not inherited from the previous snapshot. A malformed response
does not replace valid cached state, which remains usable only until expiry.
Already admitted work is unaffected by a catalog withdrawal; peer revocation
is different and cancels work.

## 4. Model descriptors

Each descriptor contains:

| Field | Meaning |
|---|---|
| `model` | Nonempty exported name, 1..128 bytes of `[A-Za-z0-9._-]` |
| `kind` | `gguf` or `laya` |
| `artifact_id` | `sha256:` plus 64 lowercase hexadecimal digits |
| `profile_id` | Same digest format, for the execution profile |
| `architecture` | Architecture identifier |
| `backend` | Effective executor backend |
| `operations` | Exactly `["decide"]` when `decision` is set, else `["synthesize"]` |
| `ready` | Current eligibility, not a capacity reservation |
| `max_input_bytes` | Positive byte limit, no more than 65536 |
| `limits` | Operation-specific limits described below |
| `class` | `generative`, `decision`, `embedding` or `reranker` |
| `capabilities` | Unique subset of `reasoning`, `tools`, `vision`, `audio`, in that order |
| `resident` | Weights are loaded now; otherwise the first request loads them. Not part of the selection: routes and admission ignore it |
| `parameters` | Elements across the artifact's tensors, at most 2^53 - 1; `0` when unknown |
| `quantization` | llama.cpp's file type (`Q4_K_M`, `F16`), else the tensor type holding the most bytes (`Q4_K`); Laya's safetensors dtype (`F32`). At most 32 bytes of `[A-Za-z0-9._-]`; empty when unknown |
| `trained_ctx` | The model's own context; for `synthesize`, at least `n_ctx`. `0` when unknown |
| `chat_format` | The turn markers of its chat template: `chatml`, `llama3`, `zephyr`, `gemma`, `gemma4` or `inst`; empty when unknown and always empty for Laya |
| `decision` | What `decide` takes: `laya` (always, for Laya), `minicheck` (a Bespoke-MiniCheck GGUF), or empty for a text generator |

`parameters`, `quantization`, `trained_ctx`, `chat_format` and `decision` are
read from the artifact's header when the server lists the model, without loading weights.
They are fixed per artifact and part of the selection. Routes carry no chat
requests; `chat_format` tells an origin's clients which markers to write into a
raw prompt.

`synthesize` limits contain positive `n_ctx` and `max_output_tokens`, with
`max_output_tokens <= n_ctx`. A native executor advertises its model's context
(the trained one, or its `MDMA_N_CTX` when lower) and the lower of that and its
output cap (`MDMA_MAX_OUTPUT_TOKENS`). These are the configured maxima in every
power state: a degraded executor applies its battery caps when it admits a
request and reports them in `Accepted`, so a power flip changes
`server_state`, never the selection. `decide` limits contain positive
`max_questions` and `max_result_bytes`, capped at 128 and 524288 respectively.
Only the matching operation's limit fields are permitted. A MiniCheck GGUF
serves `decide` only: its route, grant and limits are `decide`'s.

Names are local aliases, not global model identities. Duplicate exported
names in a snapshot are invalid. Multiple aliases may identify the same
artifact with different limits.

`critical` and `draining` catalogs MUST mark every model unready. A degraded
node keeps its models ready and their limits unchanged; it admits requests
with lower effective caps. Origins MUST exclude unready models and
nonaccepting nodes from new route selection.

For GGUF, `artifact_id` is SHA-256 of the complete GGUF bytes, including its
embedded tokenizer and metadata, not its pathname, size or model label.

For Laya, it is SHA-256 of the canonical JSON manifest:

```text
{"format":"mdma.laya-artifact.v1","files":[
  {"path":<relative-path>,"sha256":<64-lowercase-hex-digits>}, ...
]}
```

The file list includes every file consumed for weights, tokenizer,
architecture, decision heads and calibration; it is sorted by path.
Paths use `/`, are relative, and contain no empty, `.` or `..` components.
Absolute paths and symlink targets are not identities. The loader must
freeze the consumed file set; missing or ambiguous manifests cannot export.

`profile_id` is SHA-256 of a canonical JSON manifest containing
`format="mdma.execution-profile.v1"`, the executor binary's SHA-256
`build_id`, `backend`, `activation_dtype`, `kv_dtype` (`none` if unused),
and a `modes` object covering all selected numerical/kernel modes.
Mode names and string-valued vocabulary need a backend-specific definition
before a backend can export. Profiles are not interchangeable by default.
Artifact bytes, not derived/prepared cache bytes, define `artifact_id`.

Hashing must describe the immutable artifacts actually loaded, not a later
replacement path. This adds a content-identity requirement; the current
[load-once aliasing](../serving.md#several-models-on-one-port) is file-identity
deduplication, not content verification. Unverified content cannot export.

Catalogs MUST NOT reveal local filesystem paths, TLS secrets, client
inventories or prompts. A claimed digest does not prove a remote executor
actually used that artifact. Licensing/export permission remains a local
operator responsibility.

## 5. Bounds and conformance

Default hard bounds are 256 descriptors and 256 KiB of `body_json`, plus a
maximum JSON nesting depth of 16. Larger catalogs are not paginated in v1;
the operator must reduce exports. Ordinary gRPC message bounds still apply.
Local operators may choose smaller limits.

Future implementation tests MUST cover:

- A trusted certificate with the wrong identity, roots, purpose or name.
- Valid chains with unpinned leaf keys, wrong pin roles or expired pin windows.
- A claimed origin different from mTLS; wrong destination; client-RPC bypass.
- Unknown/revoked signing keys, tampered signatures and key-ID substitution.
- Duplicate JSON keys, invalid Unicode, noncanonical bytes and number bounds.
- Nonce replay, concurrent/stale polls, clock changes and catalog expiry.
- Recipient filtering, empty snapshots, removal and invalid partial updates.
- Artifact replacement, alias separation and profile changes.
- Signing-window expiry, revoked-material relabeling and stale trust generations.

The lifecycle document adds enrollment, connection expiry, rotation and
recovery gates; these are part of v1 conformance as well.

Matrix canonicalization fixtures may be used as upstream compatibility
tests. MDMA-specific signing test vectors remain a prerequisite for freezing
the wire schema.
