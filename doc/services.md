# Running MDMA as a service

`mdma-serve` stays in the foreground: no double-fork, PID files, or `--daemon`
flag in Rust. The native service manager owns supervision:

| OS | Definition | Configuration | Logs |
|---|---|---|---|
| Linux (systemd) | `deploy/systemd/mdma.service` | `/etc/mdma/mdma.env` | journal |
| Linux (systemd user) | `deploy/systemd-user/mdma.service` | `~/.config/mdma/server.env` | user journal |
| macOS (launchd) | `deploy/launchd/org.seuros.mdma.plist` | plist `EnvironmentVariables` | `/usr/local/var/log/mdma/serve.log` |
| FreeBSD (rc.d) | `deploy/rc.d/mdma` | `/usr/local/etc/mdma/mdma.env` | syslog, tag `mdma` |

All templates default to **CPU**, loopback port 50051 and mandatory mTLS.
System services use an unprivileged service account; the user unit runs as
your login user. GPU use requires a supported runtime/device
and the corresponding build feature. Metal is available on unified-memory
Macs with macOS 12+; the launchd template still defaults to CPU.
Linux without systemd can supervise the same foreground binary using
its own init system; no OpenRC/runit unit is supplied here.

## Build and provision

Build natively on the destination OS with Rust 1.99+, a C/C++ toolchain and
CMake, plus Protocol Buffers' `protoc` on PATH on FreeBSD:

```sh
cargo build --release --locked --bin mdma-serve --target-dir target/service
```

The examples install `target/service/release/mdma-serve` as
`/usr/local/bin/mdma-serve`. Add `--features rocm`, `level0`, `vulkan`, `cuda` or `metal` only on a supported
host. A workspace or custom target directory otherwise changes where
Cargo writes the executable.

Before enabling any service:

1. Provision the account described below.
2. Place a supported GGUF model at the configured absolute path, readable
   by that account. Examples use `model.gguf`; no model is bundled or
   downloaded.
3. Provision `server.pem`, `server-key.pem`, and `ca.pem`. The CA must
   validate client certificates; clients must trust the server's CA and
   its certificate must cover the DNS name/IP they connect to.
4. For system services, keep configuration and service definitions root-owned.
   Grant only the service account/group read access to the server private key;
   do not provision client or CA private keys to that account. For the local
   user service, keep configuration and credentials private and user-owned,
   as described below.

For **local development only**, `umask 077; cargo run --release --bin
mdma-certs -- certs` generates localhost certificates and one client
identity. Each invocation creates a new CA; it does not add clients to
an existing CA. Use your PKI for production/federated deployments.

## Linux / systemd

Run from the repository root. Account-creation commands may need adapting
to your distribution; skip them if a dedicated `mdma` account already exists.

```sh
sudo groupadd --system mdma
sudo useradd --system --gid mdma --home-dir /var/lib/mdma --shell /usr/sbin/nologin mdma
sudo install -d -m 0755 /usr/local/bin
sudo install -o root -g root -m 0755 target/service/release/mdma-serve /usr/local/bin/mdma-serve
sudo install -d -o root -g mdma -m 0750 /etc/mdma
sudo install -d -o mdma -g mdma -m 0750 /var/lib/mdma
sudo install -o root -g root -m 0600 deploy/systemd/mdma.env.example /etc/mdma/mdma.env
sudo install -o root -g mdma -m 0640 certs/server.pem certs/server-key.pem certs/ca.pem /etc/mdma/
sudo install -o root -g root -m 0644 deploy/systemd/mdma.service /etc/systemd/system/mdma.service
```

Edit `/etc/mdma/mdma.env`, and place the model at `/var/lib/mdma/model.gguf`
or change `MDMA_GGUF_PATH`. The env file uses systemd `EnvironmentFile`
syntax, not a shell script: no `export`, substitutions, or commands.
Use quoted literal values for paths containing spaces.

```sh
sudo systemd-analyze verify /etc/systemd/system/mdma.service
sudo systemctl daemon-reload
sudo systemctl enable --now mdma
systemctl status mdma
sudo journalctl -u mdma -f
# Stop / apply configuration or binary changes:
sudo systemctl stop mdma
sudo systemctl restart mdma
# Disable autostart and stop:
sudo systemctl disable --now mdma
```

The unit restarts failures after five seconds, with a five-starts/minute
limit. After correcting a repeated startup failure, use
`sudo systemctl reset-failed mdma` and start again.

On ROCm, models which exhaust device memory are left unloaded instead of
terminating the daemon. ROCm checks live free device memory before uploading each unique
model, leaving 256 MiB of headroom; allocation races and warm-up memory failures
also unload the affected model and all its aliases. Native tensor/buffer quota
failures follow the same skip policy. Other configured models
can still serve. Introspect lists skipped models with `ready=false`; node readiness
is false when every entry is unloaded, and requests to skipped models return
`RESOURCE_EXHAUSTED` with the reason. Upstream reachability is reported
per model, separately from node readiness. The configured default never silently
changes. Free memory and restart MDMA to retry loading; there is no background
retry or eviction of other processes. Invalid configuration, model files and
non-memory driver failures still fail startup.

`ProtectHome=true` deliberately hides home-directory models/certificates.
Use the paths above, not `/home/...`. For GPU use, grant the service account
access to the relevant `/dev/dri/render*` and, for ROCm, `/dev/kfd` devices
(often membership in `render` and/or `video`, depending on the distro).
The unit does not hide devices or forbid executable memory. Put any
driver cache under `/var/lib/mdma`, the writable state directory.

## Linux / systemd user (local daemon)

Use this variant for a workstation whose models already live in your home.
It needs no dedicated account, model copy, or group change when your user
already has GPU access. It uses the conventional `~/.config`, `~/.local/bin`
and `~/.local/state` paths. It is **not** isolation from other processes running
as your user; use the dedicated system account for that deployment.

Build the server and companion CLIs together (add `--features rocm` for ROCm):

```sh
cargo build --release --locked --target-dir target/service \
  --bin mdma-serve --bin mdma-client --bin mdma-certs --bin mdma-cache
install -d -m 0755 "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -d -m 0700 "$HOME/.config/mdma" "$HOME/.local/state/mdma" "$HOME/.cache/mdma"
install -m 0755 target/service/release/mdma-serve "$HOME/.local/bin/mdma-serve"
install -m 0755 target/service/release/mdma-client target/service/release/mdma-cache "$HOME/.local/bin/"
install -m 0755 deploy/systemd-user/mdma "$HOME/.local/bin/mdma"
install -m 0644 deploy/systemd-user/mdma.service "$HOME/.config/systemd/user/mdma.service"
install -m 0600 deploy/systemd-user/server.env.example "$HOME/.config/mdma/server.env"
install -m 0600 deploy/systemd-user/client.env.example "$HOME/.config/mdma/client.env"
```

Provision mTLS credentials before starting. For a **local-only development
daemon**, the localhost helper can create a private CA and one client identity.
Run this once, not over existing deployment credentials:

```sh
umask 077
certs="$(mktemp -d)"
target/service/release/mdma-certs "$certs"
install -d -m 0700 "$HOME/.config/mdma/tls" \
  "$HOME/.config/mdma/tls/server" "$HOME/.config/mdma/tls/client"
install -m 0600 "$certs/ca.pem" "$certs/server.pem" "$certs/server-key.pem" "$HOME/.config/mdma/tls/server/"
install -m 0600 "$certs/ca.pem" "$certs/client.pem" "$certs/client-key.pem" "$HOME/.config/mdma/tls/client/"
rm -r "$certs"
```

The CA private key is not persisted by the helper. The server configuration
does not reference the client private key. For federation or a deployment
beyond localhost, provision identities through your PKI instead.

Edit both env files: replace `/home/YOUR_USER`, set the real model path (or
`MDMA_MODELS` registry), backend and runtime library path. `server.env` is
systemd `EnvironmentFile` syntax: no shell expansion. `client.env` is sourced
by the wrapper **as your user** and must remain private, trusted and user-owned.
Ensure `~/.local/bin` is on your shell's PATH; otherwise use its absolute path.

```sh
systemd-analyze --user verify "$HOME/.config/systemd/user/mdma.service"
systemctl --user daemon-reload
systemctl --user enable --now mdma
mdma                       # waits for readiness, then prints Introspect
mdma The capital of France is
journalctl --user -u mdma -f
# Apply model/config/binary changes:
systemctl --user restart mdma
# Disable and stop:
systemctl --user disable --now mdma
```

For binary updates, stop the unit before replacing the executable, then start
it and check Introspect again.

The client normally leaves sampling to the server defaults (including random
sampling). For repeatable checkpoint comparisons, use
`MDMA_GREEDY=1 mdma The capital of France is`. This explicitly requests zero
temperature and no repetition penalty; it does not change server defaults.
Unset `MDMA_GREEDY` or set it to `0` to restore normal sampling. Other values
are rejected. Direct gRPC clients can use `mdma::grpc::SynthesizeSamplerConfig`
in `SynthesizeRequest.sampler` for explicit per-request settings.

Enabling the unit starts it with your user manager. To also start at boot and
keep it running after logout, explicitly enable lingering (may require admin):

```sh
sudo loginctl enable-linger "$USER"
loginctl show-user "$USER" -p Linger
# Undo boot/logout persistence:
sudo loginctl disable-linger "$USER"
```

The prepared cache defaults to **off** in this CPU template. It is an experimental
ROCm-only feature, not a guaranteed speedup. See
[cache validation and benchmarking](model-cache.md#validation-on-rocm-hardware).
To opt in, use the same kernel build and persistent directory:

```sh
LD_LIBRARY_PATH=/opt/rocm/lib mdma-cache /absolute/model.gguf --cache-dir "$HOME/.cache/mdma"
# Set MDMA_CACHE=required and MDMA_CACHE_DIR=/absolute/home/.cache/mdma
# in server.env, then restart and check:
systemctl --user restart mdma
mdma
```

Preparation is explicit; the daemon never builds a missing cache. SHA or kernel
changes require preparing a new entry and restarting. `--force` rebuilds an
entry, but an already resident daemon keeps its weights until restarted.
Use `MDMA_CACHE=off` again to return to the raw layout.

## macOS / launchd

For a Metal build, add `--features metal` to the build command and set
`MDMA_BACKEND` to `metal` in the installed plist. Verify GPU access and
startup under the service account before enabling it; the Metal kernel
tests validate an interactive M1 session, not launchd deployment. Shaders
compile at runtime, without the standalone Metal compiler.

Provision a dedicated non-login **`_mdma` user and `_mdma` group** using
your account-management tooling. Use unused UID/GID values, home
`/usr/local/var/lib/mdma`, and shell `/usr/bin/false`; do not reuse a
shared account such as `nobody`. Verify with `id _mdma`.
If using another dedicated account, change both `UserName` and `GroupName`
in the plist and the ownership commands below.

```sh
sudo install -d -m 0755 /usr/local/bin
sudo install -o root -g wheel -m 0755 target/service/release/mdma-serve /usr/local/bin/mdma-serve
sudo install -d -o root -g _mdma -m 0750 /usr/local/etc/mdma
sudo install -d -o _mdma -g _mdma -m 0750 /usr/local/var/lib/mdma /usr/local/var/log/mdma
sudo install -o root -g _mdma -m 0640 certs/server.pem certs/server-key.pem certs/ca.pem /usr/local/etc/mdma/
sudo install -o root -g wheel -m 0644 deploy/launchd/org.seuros.mdma.plist /Library/LaunchDaemons/org.seuros.mdma.plist
```

Place the model at `/usr/local/var/lib/mdma/model.gguf`. Edit the installed
plist's `EnvironmentVariables` to change paths, context size, or timeout.
launchd does not source your shell profile or expand `~`, `$HOME`, or shell
commands. `/usr/local` is an explicit manual-install prefix on both Intel
and Apple Silicon; it is not inferred from Homebrew.

```sh
plutil -lint /Library/LaunchDaemons/org.seuros.mdma.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/org.seuros.mdma.plist
sudo launchctl print system/org.seuros.mdma
sudo tail -f /usr/local/var/log/mdma/serve.log
# Stop/unload (also use this before replacing the binary or plist):
sudo launchctl bootout system/org.seuros.mdma
# Start again / apply changes:
sudo launchctl bootstrap system /Library/LaunchDaemons/org.seuros.mdma.plist
```

`RunAtLoad` starts the job at boot; `KeepAlive.SuccessfulExit=false`
restarts failures, throttled to five seconds. To remove boot-time
autostart, boot out the job and remove the installed plist. A clean exit
does not trigger a restart. Arrange log rotation with your macOS
administration tooling; the template writes to a file, not the journal.

## FreeBSD / rc.d

Run these commands **as root**, from the repository root. Skip existing
account creation. The script targets FreeBSD's `daemon(8)` and `rc.subr`,
not Linux's unrelated `daemon` utilities.

```sh
pw groupadd mdma
pw useradd mdma -g mdma -d /var/db/mdma -s /usr/sbin/nologin
install -d -m 0755 /usr/local/bin /usr/local/etc/rc.d
install -o root -g wheel -m 0755 target/service/release/mdma-serve /usr/local/bin/mdma-serve
install -d -o root -g mdma -m 0750 /usr/local/etc/mdma
install -d -o mdma -g mdma -m 0750 /var/db/mdma
install -o root -g wheel -m 0600 deploy/rc.d/mdma.env.example /usr/local/etc/mdma/mdma.env
install -o root -g mdma -m 0640 certs/server.pem certs/server-key.pem certs/ca.pem /usr/local/etc/mdma/
install -o root -g wheel -m 0555 deploy/rc.d/mdma /usr/local/etc/rc.d/mdma
```

Edit `/usr/local/etc/mdma/mdma.env` and place the model at
`/var/db/mdma/model.gguf`. Unlike systemd, this env file **is sourced as
root by `/bin/sh`**: use shell quoting and never make it writable by the
service account. The pre-start hook exports the assignments to the child.

```sh
sysrc mdma_enable=YES
service mdma start
service mdma status
tail -f /var/log/messages
# Stop / apply changes:
service mdma stop
service mdma restart
# Disable autostart and stop:
service mdma stop
sysrc mdma_enable=NO
```

Optional rc.conf settings: `mdma_run_user` (default `mdma`),
`mdma_program` (default `/usr/local/bin/mdma-serve`), and `mdma_config`
(default `/usr/local/etc/mdma/mdma.env`). Use conventional absolute paths
without whitespace for the program and configuration file.
Do not set `mdma_user`: rc.subr would then change the supervisor's user
instead of letting `daemon -u` drop privileges for the child.

`daemon -R 5` restarts an exited child after five seconds. rc.d tracks the
**supervisor PID** in `/var/run/mdma.pid`; stopping it forwards SIGTERM to
MDMA and suppresses restart. Signalling only the child would restart it.
The supervisor owns/removes its PID file, including across restarts.
Output goes to syslog with facility `daemon`, priority `notice`, tag
`mdma` (normally `/var/log/messages`; check your syslog configuration).

## Startup warm-up

`mdma-serve` is a resident daemon under the service manager. Startup is:

```text
Initialize backend → load weights → warm prefill/decode → join device work → listen
```

`MDMA_WARMUP_ROUNDS` defaults to **2** and accepts integers **0..8**. Set **0**
to explicitly skip warm-up; blank, malformed, non-Unicode and out-of-range values
fail startup. Each unique local GGUF model runs up to **16 synthetic token-zero
prefill positions plus four greedy decode steps** per round, capped by its trained
context and the largest configured context among aliases. A one-position context
only prefills. Aliases share one warm-up; Laya and remote upstream entries are
not exercised.

Prefill and decode read logits back to the host, joining device work and rejecting
empty/nonfinite outputs. Decode uses the normal backend factory, including HIP/CUDA
graphs when eligible. Temporary KV, workspaces and captured graphs are released
before listening; no synthetic prefix or output enters user sessions. Request-specific
allocation and graph capture still happen on requests. Weights remain resident.
Warm-up is bounded work, not an arbitrary sleep or a guarantee of stable GPU clocks,
and it does not eliminate steady-state kernel bandwidth costs.

Existing native tensor and backend buffer budgets apply. Leave headroom for warm-up
scratch; an allocation, execution or readback failure exits nonzero with the model
name instead of publishing readiness. Stop requests are checked between models,
rounds, prefill and decode steps; already submitted device work is not interrupted.

The listener is not bound until loading and enabled warm-up complete. No
listener is available while startup is in progress.

### Client readiness waiting

`mdma-client` automatically polls mTLS Introspect before Introspect output,
Synthesize or Decide. `MDMA_READY_TIMEOUT_SECS` defaults to **120**, accepting integers
**0..86400**. Zero disables retries, not the readiness check: it makes one
Introspect call bounded to five seconds and fails if unready.

Introspect with no `MDMA_MODEL` checks node readiness only. Work requests check
the explicit model or server default as well, without waiting for unrelated
unready upstreams. An unknown model on a ready node fails immediately.
Calls are capped at five seconds each and by the remaining overall deadline;
retries pause 200 ms and stop at the deadline. A stalled Introspect call cannot
extend that deadline. The client prints one waiting message to stderr and a
nonzero timeout error containing the last observed reason.

Only gRPC `UNAVAILABLE`, `DEADLINE_EXCEEDED`, timed-out probes and unready
Introspect responses retry. Other gRPC errors, including authentication/permission
errors, fail immediately. Invalid local certificate files fail before polling.
The transport currently reports connection and TLS-handshake failures as
`UNAVAILABLE`, so those retry up to the bounded deadline; mTLS verification is
never bypassed. Readiness is not an admission reservation: the node can shut
down after the check. Synthesize/Decide themselves are never retried, avoiding
duplicate work or repeated streamed output.

## Shutdown and readiness

On **SIGTERM or SIGINT**, on all three OSes:

1. Stop accepting connections and initiate Rama's HTTP connection drain.
2. Reject new `Synthesize` calls reaching the service with gRPC `UNAVAILABLE`;
   `Introspect.ready` becomes false.
3. Let already admitted streams finish. Track their blocking workers too,
   including workers whose clients disconnected during prefill.
4. Exit successfully when drained. After `MDMA_SHUTDOWN_TIMEOUT_SECS`
   (default **30**, integer **1..86400**), log a deadline error and exit
   with failure rather than wait indefinitely for a blocking thread.

The timeout also bounds waiting for backend/device initialization (including
CUDA's PTX JIT), model loading, and warm-up after a stop request. These run in the
guarded blocking startup task, not on the shutdown supervisor's async thread.
At the hard deadline, unfinished streams/work are terminated, not promised
to finish or persist. Active GPU kernels/prefill cannot be safely
interrupted mid-operation. Existing client-disconnect cancellation still
applies; no model, prefix, or KV state is persisted across service restarts.

systemd `TimeoutStopSec` and launchd `ExitTimeOut` are **40 seconds**.
Increase these as well if increasing MDMA's 30-second timeout; similarly
allow enough time in FreeBSD's host shutdown policy. There is no SIGHUP
reload: restart to change the model, certificates, or configuration.

Service-manager “running” is **not readiness**: loading or warm-up may still be
in progress. Check the mTLS `Introspect` RPC. The “mdma listening” log is emitted
only after a successful bind. Missing configuration/TLS files, invalid
numeric configuration, unavailable backends, and bind failures exit
nonzero; mTLS is never disabled to recover.
