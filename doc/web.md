# Web UI (`mdma-web`)

`mdma-web` is a browser UI for a running `mdma-serve`. It listens on localhost
and connects to the server with a client certificate, like `mdma-client`.

| View | Shows |
|---|---|
| Memory & placement | Each device as a column: weights of the loaded models (banded by layer), live KV cache and workspace, the native budget and free memory; models not loaded are listed under it. Federated models orbit as remote nodes. |
| Federation | A molecule: this server and its models at the centre, peers around it on a ring (up to eight) or a fullerene shell banded by OS, each with its OS, architecture and accelerator. Models are atoms coloured by class (generative, decision, embedding, reranker) with an orbit per capability (reasoning, tools, vision, audio). Loaded models are solid, models on disk translucent, loading ones breathe and failed ones are ringed in red; labels count leases. **Load** keeps a model loaded under the gateway's lease (renewed while it runs, released on **Release** or when it stops); on a federated route the origin holds the lease on the executor. Hover a peer for its models. Operators also see trust state, grants, catalogs and routes. |
| Playground | Pick a model, write a prompt and a system message, and watch the reply stream while the context window fills. A model on disk shows `loading` until the server has loaded it. |

## Run

```sh
cargo build --release --bin mdma-web
MDMA_TLS_CERT=certs/client.pem MDMA_TLS_KEY=certs/client-key.pem MDMA_TLS_CA=certs/ca.pem \
  target/release/mdma-web
# mdma-web listening on http://127.0.0.1:6364/?token=3f9c…
```

Open the printed URL; the token is new on every start.

| Variable | Default | Meaning |
|---|---|---|
| `MDMA_WEB_LISTEN` | `127.0.0.1:6364` | Loopback `ip:port`. |
| `MDMA_ADDR` | `localhost:6362` | `mdma-serve`. |
| `MDMA_TLS_CERT`, `MDMA_TLS_KEY`, `MDMA_TLS_CA` | required | Client certificate, key and CA. |
| `MDMA_WEB_MAX_STREAMS` | `2` | Concurrent playground generations (1..8). |
| `MDMA_WEB_WATCH_MS` | `2000` | Status refresh interval (250..60000). |
| `MDMA_RPC_TIMEOUT_MS` | `300000` | Generation deadline (1..300000); federation routes require one. |

To see federation peers, add the gateway's certificate pin to the server's
`MDMA_OPERATOR_SPKI`; the Federation view shows the exact line.

## Service

`deploy/systemd-user/mdma-web.service` (with `web.env.example`) and
`deploy/launchd/org.seuros.mdma-web.plist` run it next to a local server. The
URL is in the log:

```sh
journalctl --user -u mdma-web | grep 'listening on'      # systemd
grep 'listening on' ~/Library/Logs/mdma-web.log           # launchd
```

## Front end

The three.js stage lives in `web/` and is bundled into
`src/bin/mdma-web/assets/scene.js`:

```sh
cd web && bun install && bun test && bun run build
```

`just web-preview` shows the Federation view on a seeded stub fleet:
`http://localhost:3000/?peers=100&seed=1&federated=1`.
