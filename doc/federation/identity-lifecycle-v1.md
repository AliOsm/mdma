# Federation v1: identity and key lifecycle

**Design specification, reviewed 2026-10-03.** The implemented trust store and
transport binding are described in the [security foundation](security-foundation.md).
Complements [identity and catalogs](identity-and-catalog-v1.md). These are local trust
management rules, not additional public RPCs or a frozen configuration schema.

## 1. Identity is a configured authority

`server_id` identifies an operator's deployment, not a machine, endpoint,
certificate or public key. V1 does not discover authority through DNS or
establish global ownership of that name. A local administrator establishes
the association between name and credentials out of band.

The sole URI SAN of each federation leaf certificate MUST be exactly:

```text
https://<server_id>/.well-known/mdma/identity
```

Use the canonical ASCII name from the peer record. No URI normalization,
aliases, port, query, fragment or percent-encoded equivalent is accepted.
This is an application identity label, NOT a URL to fetch. Outbound TLS
service-name verification against `tls_name` remains a separate check.

The host-bearing URI allows URI name constraints to remain meaningful.
The original draft's hostless `urn:mdma:server:...` form is superseded.
An issuer's unsupported or violated name constraints MUST fail validation;
the application identity check never bypasses PKI validation.

## 2. Enrollment and peer states

Exchange a public enrollment bundle containing server identity, endpoint,
TLS service name, CA certificates, role-specific TLS public-key pins,
metadata-signing keys and their proposed validity windows. It MUST NOT
contain private keys, CA signing secrets or permission grants.

Verify the bundle through an independently authenticated administrative
channel. A bundle fetched from the untrusted endpoint, a DNS lookup, a
self-signature, or a signature by the key being enrolled is not sufficient.
For manual verification, compare a SHA-256 digest of the exact exchanged
bundle bytes over the authenticated channel.

Public-key fingerprints are not interchangeable:

| Item | Fingerprint input |
|---|---|
| CA certificate | DER-encoded complete certificate |
| TLS key pin | DER-encoded `SubjectPublicKeyInfo` |
| Ed25519 signing key | The raw 32-byte public key |

Use `sha256:` followed by 64 lowercase hexadecimal digits. Do not hash PEM
text, a hostname, certificate Common Name or an enrollment key label.

Peer state is explicit and persistent:

| State | Permitted behavior |
|---|---|
| `staged` | No workload, routes or catalog access; explicitly granted `Identify` probes only |
| `active` | RPCs permitted only by current credential checks and explicit grants |
| `suspended` | No peer RPCs or new routes; cancel active work and invalidate connections |
| `revoked` | Same denial, with a persistent offboarding record; no automatic reactivation |

Staging defaults to no RPC grants. Transition to `active` requires a local
administrative action, not a successful handshake or `Identify`.

A safe initial activation grants only `Identify` and `ScanCapabilities`, with
empty model exports, execution grants and route imports. Verify a nonce-bound
signed empty catalog, then separately approve workload permissions.
Each operator performs its own enrollment; neither action enrolls it at
the other operator.

Recovery from `suspended` or `revoked` requires reviewed enrollment through
`staged`. Workload grants start empty; they are never inherited from the
old enrollment. A server-name change is a new enrollment, not a redirect.
Keep existing execution-ID tombstones when reenrolling the same name.

## 3. TLS binding and certificate lifetime

Each enabled direction MUST have a matching accepted TLS public-key pin:
`client_auth` pins for inbound peers and `server_auth` pins for outbound
servers. Pins have explicit finite local validity windows. An empty role's
pin set denies that direction.

For a connection to be eligible, ALL of these checks must succeed:

1. TLS 1.3 full handshake and proof of possession of the leaf private key.
2. Normal certificate-chain validation under this peer's approved roots,
   including validity, constraints and critical extensions.
3. Non-CA leaf with `digitalSignature` key usage and an explicit matching
   `clientAuth` or `serverAuth` extended key usage.
4. Exact application identity URI, plus outbound DNS/IP SAN verification
   of `tls_name`; no Common Name fallback.
5. Matching role-specific SPKI pin, a currently valid pin window and no
   local key-material revocation.

SPKI pinning is additional to CA validation, not a replacement for it.
An issuer cannot silently introduce a new leaf public key. TLS and catalog
keys are different credentials; possession of one does not prove possession
of the other. TLS authentication and catalog signing MUST use different
private key material. TLS client/server keys SHOULD also be distinct;
sharing requires explicit pins for both roles and both purposes checked.

V1 MUST disable TLS session resumption, session tickets and 0-RTT early
data. No application RPC is processed before the full mTLS handshake has
completed. Reconnection must present the currently accepted credentials.
An optimized resumption design would need a separate identity/revocation
review before becoming an extension.

The peer supplies its intermediate chain. Certificate URLs MUST NOT
trigger automatic AIA, CRL or OCSP network fetches. V1 uses locally supplied
trust material and revocation records; publishing an issuer CRL alone does
not update MDMA's trust state.

Separate listeners alone do not prevent a client-RPC bypass. Federation
credentials MUST NOT authenticate as unrestricted client-facing `Mdma` clients.
Do not add a peer's issuer to the client-CA store. If trust roots
overlap, independently allowlisted client identities and rejection
of federation-profile certificates are required before enabling federation.
Otherwise reject that deployment configuration, rather than pretending
federation grants constrain the client endpoint. Separately granted client
credentials remain a separate administrative responsibility.

A binding records the authenticated identity, role, leaf SPKI, validated
trust anchor, leaf/intermediate expiry, local certificate expiry and the
local trust generation. Outbound bindings additionally record endpoint
authority and `tls_name`. Check current eligibility on EVERY RPC.
`Identify` is neither a bearer token nor permanent connection authorization.

An endpoint/reference-name change forbids new RPCs on the old outbound
binding. Already admitted work may drain if its credentials and grants
remain valid; a change cannot silently redirect an existing stream.

Compute a monotonic authorization deadline from the earliest applicable
certificate/pin expiry, considering both local and remote credentials.
No clock adjustment may extend that binding's deadline. Expiry denies
new RPCs, closes the affected binding and cancels its streams at safe
compute boundaries. A normal renewal does not refresh an old binding's
certificate lifetime: a new handshake is required.

## 4. Signing-key windows and catalog eligibility

Each accepted signing key has immutable `(server_id, key_id, public_key)`
association and locally approved `valid_from_ms` / `valid_until_ms`.
Both timestamps are finite nonnegative integers within the catalog's
integer range, with `valid_from_ms < valid_until_ms`.

A key is usable only when:

```text
valid_from_ms <= local_now_ms < valid_until_ms
```

The catalog's `issued_at_ms` MUST also be inside that window. The 30-second
catalog timestamp tolerance does not enlarge a key's validity window.
A catalog's local expiry is additionally capped by the signing key's
`valid_until_ms`; a route may not outlive its key even if its signed
`expires_at_ms` is later. An observed expiry is not reversed by clock rollback.

Before every route selection/dispatch, check the current peer state,
catalog lifetime, signing-key eligibility and local import/disclosure
permissions. Cached successful verification is not a permanent grant.

Local signer selection is explicit: one current key ID and its private
key. Preloading another private key does not activate it. No automatic
fallback to an unknown, retired or expired key is permitted.

Retain historical key-ID associations. The same ID can never acquire
different bytes. Revocations track public-key material as well as IDs,
so relabeling a compromised key cannot reenroll it. Remote key documents
and cross-signatures cannot mutate local trust. Persist observed key-window
retirement so a clock rollback or ordinary reload cannot reactivate it.

## 5. Planned rotation

Every overlap has an explicit finite end, agreed administratively per peer.
There is no default perpetual overlap and no assumption that offline peers
acknowledged a change.

| Credential change | Sequence |
|---|---|
| New signing key | Deliver public key/window out of band; recipients approve overlap; select the new signer; verify fresh catalogs; retire the old key |
| Renewed TLS certificate, same key | Issue correct identity/service SANs and EKU; establish new full handshakes; let still-valid old bindings finish before expiry |
| New TLS key | Approve the new role-specific SPKI/window first; deploy the leaf; establish new bindings; remove the old pin at the agreed end |
| New CA | Approve the root separately; ensure required leaf pins remain approved; deploy the new chain; verify new bindings; remove the old root |
| Endpoint change | Approve endpoint/`tls_name` locally; validate its certificate and existing identity/pins; no discovery, redirect or identity rename |

Plan TLS replacement before the old authorization deadline, allowing the
maximum admitted request duration for existing work to finish. Removing a
still-used old pin or root closes the affected bindings; rotation is not
a promise of uninterrupted streams.

Administrative rollback is allowed only to still-authorized, unrevoked
credentials within their approved windows. It does not revive an expired
window or restore grants removed during recovery.

## 6. Revocation, races and recovery

| Event | Required effect |
|---|---|
| Routine signing-key retirement | Remove its catalogs/routes; admitted work on still-valid TLS bindings may finish |
| Routine TLS pin/root removal or expiry | Deny new use; close affected bindings and cancel their work |
| Execution grant removal | Deny new admissions and cancel work now outside the remaining grants |
| Import/disclosure grant removal | Withdraw those routes and cancel affected outbound work |
| Lower resource quota | Apply to new admissions; keep existing resource charges until workers finish |
| Suspected compromise of any credential | Suspend the peer, revoke affected material, drop catalogs/connections and cancel all its work |
| Offboarding | Revoke the peer and retain its identity/key-material denial records |

Compromise recovery has NO trusted-old-key rollover. Contact the operator
through an independent administrative channel, assess which credentials
were exposed, enroll replacement material, and explicitly approve grants.
If an issuer is affected, review every peer trusting that issuer; do not
treat one replaced leaf as complete issuer-compromise recovery.

Every trust/policy publication increments a local monotonically increasing
generation. Publish an entirely validated candidate atomically. An invalid
candidate makes no change and reports failure; it must not appear to have
applied a requested revocation.

Publication discards cached catalogs and outstanding nonces for affected
peers. A late response tagged with the old generation cannot republish
routes. New admission and the final pre-`Accepted` check must revalidate
against the current generation; there is no authorization from a stale
snapshot. Additions need not terminate bindings/work still eligible under
the new policy.

Revocation cancellation also gates output forwarding. Bytes already handed
to the transport cannot reliably be recalled, and launched GPU work may need to
finish, but no new application output is deliberately forwarded after the
local cancellation barrier. Keep charges until workers have actually joined.

Persist revocations before reporting administrative success. Retain denied
TLS/signing key material, immutable signing-ID history and offboarding
records across reloads/restarts. A missing or unreadable required trust
state fails closed; it cannot trigger fresh TOFU.
TLS SPKI denials apply to both TLS roles and all certificates using that
SPKI; signing-key denials apply to raw Ed25519 bytes under every key ID.
Recovery must generate genuinely new key material, not merely a new
certificate, label or public-key wrapper.

Trusted administrators, trusted local storage and a dependable UTC clock
are assumptions. The generation counter prevents local races, not malicious
disk/backup rollback. Backup restoration must reconcile revocations before
enabling peers; a detected invalid clock stops new credential acceptance.
Trust persistence does not make inference deduplication crash-safe.

## 7. Review gates and primary references

Future tests MUST include staged/default-deny enrollment; wrong role/URI/SAN;
allowed root with unpinned leaf; URI-name-constrained chains; renewed
same-key certificates; new-key and CA overlap; expiry on pooled connections;
client entry points under separate listeners and overlapping CA roots;
disabled resumption/early data; removed grants during validation; late
catalog responses after revocation; signer-window cutoff and clock rollback;
compromised-key relabeling; offboarding/restart; and lost trust state.

Before implementation, verify that the selected TLS stack exposes enough
authenticated certificate/binding data and fails closed on unsupported
constraints. Before wire freeze, publish corresponding credential fixtures
and MDMA signing vectors. The stricter application profile is an MDMA choice,
not a claim that the RFCs mandate SPKI pins or disable all resumption.

| Primary reference | Sections |
|---|---|
| `https://www.rfc-editor.org/rfc/rfc5280.html` | 4.2.1.6, 4.2.1.9..12, 6: names, constraints, usage and validation |
| `https://www.rfc-editor.org/rfc/rfc9525.html` | 4, 6: service identities and verification |
| `https://www.rfc-editor.org/rfc/rfc8446.html` | 2.2, 2.3, 4.4, 8: resumption, authentication and early-data replay |
| `https://www.rfc-editor.org/rfc/rfc8032.html` | 5.1, 7.1: Ed25519 and test vectors |
