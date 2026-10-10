# Matrix reference for MDMA federation

This design pins **Matrix v1.19**. The pinned release page gives its release date
as **July 8, 2026**. Do not confuse the moving stable alias with `/unstable/`,
proposal discussions or implementation-specific behavior.

## Primary references

| Reference | Official source |
|---|---|
| Stable-version check | `https://spec.matrix.org/latest/` |
| Server-server API | `https://spec.matrix.org/v1.19/server-server-api/` |
| Appendices | `https://spec.matrix.org/v1.19/appendices/` |
| Versioned changelog index | `https://spec.matrix.org/v1.19/changelog/` |
| V1.19 release | `https://spec.matrix.org/v1.19/changelog/v1.19/` |

Relevant section anchors:

- Server API: `#resolving-server-names`, `#retrieving-server-keys`,
  `#request-authentication`, `#client-tls-certificates`, `#transactions`,
  `#room-state-resolution`.
- Appendices: `#canonical-json`, `#signing-details`,
  `#url-safe-unpadded-base64`, `#security-threat-model`,
  `#cryptographic-test-vectors`.

Rendered HTML may change without changing protocol semantics. Review the
pinned normative sections rather than treating `/latest/` or an HTML checksum
as a stable protocol definition.

## What MDMA borrows

Matrix separates logical server identity from connection discovery, uses
signatures for authenticated federation requests, publishes signing keys,
and defines canonical JSON. Its discovery uses well-known/SRV resolution;
transactions transport bounded event batches.

MDMA adopts independent operator control, explicit origin/destination
binding, canonical signed metadata and bounded communication. It does not
implement Matrix endpoints or claim interoperability.

## Deliberate departures

| Area | MDMA v1 |
|---|---|
| Discovery | Configured endpoints; no well-known/SRV resolution |
| RPC authentication | Full-handshake TLS 1.3 mTLS with role-specific SPKI pins, not `X-Matrix` request signing |
| Metadata keys | Administratively pinned keys; no key-notary trust |
| Synchronization | Recipient-specific full catalog snapshots, not event transactions |
| Room/event state | None; no DAG, event authorization chain or room-state resolution |
| Inference output | Direct bounded streams, not replicated events |

Matrix authenticates requests at the HTTP layer; client TLS certificates
are optional in that protocol. MDMA's mTLS requirement is its own choice.
It consequently cannot blindly inherit Matrix deployment assumptions about
TLS-terminating load balancers.

The [identity lifecycle](identity-lifecycle-v1.md) defines MDMA's additional
certificate profile, enrollment, key windows and live revocation behavior.
These controls are not inherited Matrix requirements.

Only Matrix v1.19 canonical JSON is adopted as an encoding definition.
MDMA defines its own signed envelope and domain separation. Do not substitute
RFC 8785/JCS or ordinary `serde_json` serialization without establishing
that the selected implementation conforms to the pinned encoding.

## Before wire freeze

- Select established Rust implementations and test canonical JSON against
  the pinned upstream fixtures.
- Publish MDMA catalog preimage/signature vectors, including Unicode,
  escaped characters, wrong recipients and key-ID substitution.
- Define each backend's execution-profile mode vocabulary.
- Review future Matrix changes explicitly; a new Matrix release does not
  automatically change the MDMA protocol.
