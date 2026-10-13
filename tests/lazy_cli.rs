//! Lazy loading against a real mdma-serve: models load on first use, leases
//! keep them loaded, idle models unload, and a full native budget evicts the
//! least recently used idle model but never a leased one. One daemon runs per
//! machine, so the three configurations run one after another.
#[path = "runtime_cli/fixture.rs"]
mod fixture;
#[path = "runtime_cli/process.rs"]
mod process;
#[path = "runtime_cli/wait.rs"]
mod wait;

use mdma::grpc::client::{Client, mtls_client};
use mdma::grpc::{
    AcquireRequest, IntrospectRequest, MemorySnapshot, ModelStatus, ReleaseRequest,
    SynthesizeRequest, WatchRequest,
};
use process::{Process, command, free_port};
use rama::http::grpc::{Code, Status};
use std::assert_matches;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio_stream::StreamExt;

/// Bounds every RPC and every wait for a state change.
const TIMEOUT: Duration = Duration::from_secs(30);

/// One mdma-serve over `root/models`, and an mTLS client for it. Prints the
/// server's log if the test fails while it runs.
struct Server {
    process: Process,
    log: PathBuf,
    client: Client,
    runtime: tokio::runtime::Runtime,
}

impl Server {
    /// Starts mdma-serve with `env` over the shared settings and waits until
    /// it answers Introspect. Its stderr goes to `root/serve-<name>.log`.
    fn start(root: &Path, name: &str, env: &[(&str, &str)]) -> Self {
        let certs = root.join("certs");
        let port = free_port();
        let log = root.join(format!("serve-{name}.log"));
        let process = command(env!("CARGO_BIN_EXE_mdma-serve"))
            .env("MDMA_MODELS_DIR", root.join("models"))
            .env("MDMA_BACKEND", "cpu")
            .env("MDMA_POWER", "off")
            .env("MDMA_WARMUP_ROUNDS", "0")
            .env("MDMA_LISTEN", format!("127.0.0.1:{port}"))
            .env("MDMA_TLS_CERT", certs.join("server.pem"))
            .env("MDMA_TLS_KEY", certs.join("server-key.pem"))
            .env("MDMA_TLS_CA", certs.join("ca.pem"))
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut server = Self {
            process: Process(process),
            log,
            client: mtls_client(
                &format!("https://127.0.0.1:{port}"),
                &certs.join("client.pem"),
                &certs.join("client-key.pem"),
                &certs.join("ca.pem"),
            )
            .unwrap(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        };
        let answered = wait::until(TIMEOUT, || {
            // Another daemon holding the machine lock ends it here.
            if let Some(status) = server.process.0.try_wait().unwrap() {
                panic!("mdma-serve exited during startup: {status}");
            }
            let probe = server.client.introspect(IntrospectRequest {});
            let answer = async { tokio::time::timeout(Duration::from_secs(5), probe).await };
            server.runtime.block_on(answer).ok()?.ok()
        });
        assert!(answered.is_some(), "mdma-serve never answered Introspect");
        server
    }

    /// Runs `rpc` to completion within [`TIMEOUT`].
    fn call<F: Future>(&self, rpc: F) -> F::Output {
        self.runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, rpc).await })
            .expect("the server answers in time")
    }

    fn models(&self) -> Vec<ModelStatus> {
        self.call(self.client.introspect(IntrospectRequest {}))
            .unwrap()
            .into_inner()
            .models
    }

    fn model(&self, name: &str) -> ModelStatus {
        self.models()
            .into_iter()
            .find(|model| model.name == name)
            .unwrap_or_else(|| panic!("no model {name}"))
    }

    /// Each model as `name residency leases=n`, in folder order.
    fn states(&self) -> Vec<String> {
        self.models()
            .iter()
            .map(|m| format!("{} {} leases={}", m.name, m.residency, m.leases))
            .collect()
    }

    /// Polls Introspect until `name` is `residency`.
    fn until(&self, name: &str, residency: &str) {
        let reached = wait::until(TIMEOUT, || {
            (self.model(name).residency == residency).then_some(())
        });
        assert!(
            reached.is_some(),
            "{name} never became {residency}: {:?}",
            self.states()
        );
    }

    /// Four tokens from `model`, which loads first if it is not resident.
    fn synthesize(&self, model: &str) -> Result<usize, Status> {
        self.call(async {
            let mut tokens = self
                .client
                .synthesize(SynthesizeRequest {
                    prompt: "hi".into(),
                    max_tokens: 4,
                    model: model.into(),
                    ..SynthesizeRequest::default()
                })
                .await?
                .into_inner();
            let mut emitted = 0;
            while let Some(token) = tokens.next().await {
                token?;
                emitted += 1;
            }
            Ok(emitted)
        })
    }

    /// A lease on `model` with the default TTL, granted once it is loaded.
    fn acquire(&self, model: &str) -> String {
        let lease = self
            .call(self.client.acquire(AcquireRequest {
                model: model.into(),
                ttl_ms: 0,
                wait: true,
            }))
            .unwrap()
            .into_inner();
        assert_eq!(
            (lease.model.as_str(), lease.residency.as_str()),
            (model, "resident")
        );
        lease.lease_id
    }

    fn release(&self, lease_id: String) {
        self.call(self.client.release(ReleaseRequest { lease_id }))
            .unwrap();
    }

    /// The native budget now, from the first Watch snapshot.
    fn memory(&self) -> MemorySnapshot {
        self.call(async {
            let mut snapshots = self
                .client
                .watch(WatchRequest { interval_ms: 250 })
                .await
                .unwrap()
                .into_inner();
            snapshots.next().await.unwrap().unwrap()
        })
        .memory
        .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "--- {} ---\n{}",
                self.log.display(),
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}

#[test]
fn models_load_on_use_and_unload_when_idle_or_evicted_unless_leased() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(
        command(env!("CARGO_BIN_EXE_mdma-certs"))
            .arg(root.join("certs"))
            .output()
            .unwrap()
            .status
            .success()
    );
    let models = root.join("models");
    std::fs::create_dir(&models).unwrap();
    for (name, width) in [("a", 8), ("b", 16)] {
        std::fs::copy(
            fixture::model(width).path(),
            models.join(format!("{name}.gguf")),
        )
        .unwrap();
    }

    // Defaults: models load on first use and stay until evicted.
    let server = Server::start(root, "lazy", &[]);
    assert_eq!(
        server.states(),
        ["a available leases=0", "b available leases=0"],
        "nothing loads before use"
    );
    assert_matches!(server.synthesize("a"), Ok(4));
    assert_eq!(
        server.states(),
        ["a resident leases=0", "b available leases=0"]
    );
    let lease = server.acquire("b");
    assert_eq!(
        server.states(),
        ["a resident leases=0", "b resident leases=1"]
    );
    server.release(lease);
    assert_eq!(
        server.states(),
        ["a resident leases=0", "b resident leases=0"],
        "released, b stays: idle unload is off by default"
    );
    // What each model holds in the budget: its weights. Request memory (KV,
    // workspace) is returned when a stream ends.
    let memory = server.memory();
    let weights = |name: &str| {
        memory
            .weights
            .iter()
            .find(|upload| upload.reserved && upload.models.iter().any(|m| m == name))
            .unwrap_or_else(|| panic!("{name} holds no weights: {memory:?}"))
            .bytes
    };
    let (a, b) = (weights("a"), weights("b"));
    assert!(
        a < b && memory.used_bytes == a + b,
        "two idle models hold only their weights: {memory:?}"
    );
    drop(server);

    // Idle unload: a model unused for a second unloads, unless leased.
    let server = Server::start(root, "idle", &[("MDMA_IDLE_UNLOAD_SECS", "1")]);
    let lease = server.acquire("b");
    assert_matches!(server.synthesize("a"), Ok(4));
    server.until("a", "available");
    // b was idle longer than a, so only its lease kept it.
    assert_eq!(
        server.states(),
        ["a available leases=0", "b resident leases=1"]
    );
    server.release(lease);
    server.until("b", "available");
    drop(server);

    // Eviction: either model fits with a request on it, both together do not.
    // A load is admitted on weights alone, so `a + b - 1` refuses the pair
    // exactly and leaves b all of a's size (13 KB) for a request; this one
    // reserves about 3 KB (KV for 5 positions, workspace for a 2-token prefill).
    let budget = a + b - 1;
    let server = Server::start(
        root,
        "budget",
        &[("MDMA_NATIVE_TENSOR_BYTES", &budget.to_string())],
    );
    assert_matches!(server.synthesize("a"), Ok(4));
    assert_matches!(server.synthesize("b"), Ok(4));
    assert_eq!(
        server.states(),
        ["a available leases=0", "b resident leases=0"],
        "loading b evicted the idle a"
    );
    let lease = server.acquire("b");
    let refused = server.synthesize("a").unwrap_err();
    let held = format!(
        "native tensor memory budget exhausted: requested {a} bytes, used/reserved {b}, \
         limit {budget}; held by b (1 lease)"
    );
    assert!(
        refused.code() == Code::ResourceExhausted && refused.message().ends_with(&held),
        "a leased b is not evicted: {refused:?}"
    );
    assert_eq!(
        server.states(),
        ["a available leases=0", "b resident leases=1"]
    );
    assert_eq!(server.model("a").error, held, "Introspect says why");
    server.release(lease);
    assert_matches!(server.synthesize("a"), Ok(4));
    assert_eq!(
        server.states(),
        ["a resident leases=0", "b available leases=0"],
        "released, b is evicted like any idle model"
    );
}
