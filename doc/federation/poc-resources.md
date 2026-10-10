# LAN resource POC

This POC implements resource advertisement only. It is **not** the draft
federation execution protocol: no inference routing, model transfer,
enrollment, signed catalogs, or trust lifecycle. Transport is plaintext,
unauthenticated HTTP/gRPC. Use only a trusted private LAN; never expose it
to the Internet.

## Start a node

Build with `cargo build --bin mdma`. No model or GPU initialization is needed.
Run one daemon per machine:

```sh
mdma node serve --listen 192.0.2.10:50052 --storage /path/to/models
```

All `192.0.2.x` addresses here are documentation examples: replace them with
actual LAN interface addresses. `--listen` and `--storage` are required;
storage must be an existing directory. There is no automatic public bind.

Both the resource daemon and the inference `mdma-serve` binary take an exclusive
kernel lock on `/tmp/mdma-machine.lock`. Normal exit or process termination
releases the lock. **Never delete or replace the lock file while MDMA is
running.** Read-only CLI commands do not take the lock. This is cooperative
singleton enforcement, not protection against unrelated binaries or containers
with separate `/tmp` namespaces.

## Discover and query peers

Each daemon advertises and browses `_mdma._tcp.local.` using embedded
`mdns-sd`, standard mDNS/DNS-SD on multicast UDP port 5353. Discovery normally
stays within one LAN/VLAN; firewalls or multicast filtering can prevent it.

```sh
mdma node discover
mdma node discover --seconds 8
mdma node discover --watch
mdma node discover --watch --table
```

JSON events are the default. The table shows resources, network observations,
identity, status, and snapshot age. `?` means unknown; `~` marks retained
historical values. An empty LAN is a successful empty discovery.

Advertisement is restricted to the interface owning the explicit bind IP.
Wildcard, loopback, and IPv6 link-local binds cannot advertise. Use
`--no-discovery` for explicit-address-only operation, including loopback tests.
Scoped IPv6 transport is deferred.

When multicast is unavailable, query peers explicitly:

```sh
mdma node scan --peers http://192.0.2.10:50052,http://192.0.2.20:50052
mdma node interface --address 192.0.2.10
```

Scans emit one JSON object per peer and exit nonzero if any peer is
unreachable or rejected. Requests have a five-second timeout. Explicit scans
have no snapshot cache; repeat them for fresh observations.

## Resource and network observations

Snapshots report logical CPU count, CPU model when available, total/available
RAM, and total/available disk bytes for the selected storage filesystem.
Disk figures are not directory sizes or quotas. Available resources are
observations, not reservations or capacity guarantees. Remote timestamps do
not establish synchronized clocks.

The selected NIC reports observed negotiated link speed when available.
Missing rates remain null, never filled from hardware or driver ceilings.
Virtual interfaces and identified guests do not establish a physical medium
or host NIC speed. Underlying interface members are not summed or treated as
proof of per-flow routing. See the [network catalog](../network-catalog.md).

The querying node separately records TCP-connect RTT for numeric-IP endpoints
and resource RPC elapsed time. Neither is one-way latency, an ICMP measurement,
or a bandwidth benchmark. Hostname endpoints can return resources without an
available numeric-IP latency probe.

Discovery polls resources every ten seconds after each completed request,
with at most eight concurrent probes and 128 retained services. Removed
services remain offline for 60 seconds before being purged. Statuses distinguish
`discovered`, `online`, `unreachable`, `offline`, and `rejected`. A retained
snapshot's `last_success_age_ms` explicitly marks historical data. Graceful
shutdown withdraws advertisements; crashes depend on subsequent probes and
mDNS expiry.

## Compatibility, not authentication

Peers advertise and exchange their semantic version, build ID, and resource
protocol version (currently **2**). The default minimum peer release version
is the local binary's package version; protocol versions must match exactly.

```sh
mdma node discover --watch --table --min-peer-version 0.1.0
mdma node scan --peers http://192.0.2.10:50052 --require-peer-build FULL_BUILD_ID
```

`mdma-serve` accepts the same compatibility policy flags. An `Identify` handshake
precedes resource collection, and its identity must agree with discovery and
resource responses. Version checks are **unauthenticated, self-reported
compatibility checks**, not the future federation security boundary.

Git builds embed a commit ID, with `-dirty` for tracked changes. Source archives
use `unknown` unless the packager supplies `MDMA_BUILD_ID`. Build tokens are
diagnostics/pins, not source-provenance proofs or update mechanisms.
