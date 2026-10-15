//! Federation set up with nothing but mdma-fed: a real executor serves its
//! signed catalog to the origin, whose routes then pass mdma-serve's startup
//! checks, and takes models the origin pushes. One daemon runs per machine,
//! so the origin is not started here.
#[path = "runtime_cli/fixture.rs"]
mod fixture;
#[path = "runtime_cli/process.rs"]
mod process;

use process::{Process, command, free_port};
use rustls_pki_types::pem::PemObject;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// One mdma-serve per machine: tests that start one take turns.
static DAEMON: Mutex<()> = Mutex::new(());

fn daemon_turn() -> MutexGuard<'static, ()> {
    DAEMON.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fed(args: &[&str]) -> Output {
    command(env!("CARGO_BIN_EXE_mdma-fed"))
        .args(args)
        .output()
        .unwrap()
}

fn ok(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

struct Node {
    dir: PathBuf,
    models: PathBuf,
    client: u16,
}

impl Node {
    /// A node whose models folder holds `model` as tiny.gguf, or nothing.
    fn new(root: &Path, name: &str, model: Option<&Path>) -> Self {
        let dir = root.join(name);
        let id = format!("{name}.fleet.test");
        ok(&fed(&[
            "init",
            path(&dir),
            "--id",
            &id,
            "--endpoint",
            &format!("https://127.0.0.1:{}", free_port()),
            "--days",
            "1",
        ]));
        let models = root.join(format!("{name}-models"));
        std::fs::create_dir(&models).unwrap();
        if let Some(model) = model {
            std::fs::copy(model, models.join("tiny.gguf")).unwrap();
        }
        let bundle = root.join(format!("{name}.json"));
        std::fs::write(&bundle, ok(&fed(&["export", path(&dir)]))).unwrap();
        Self {
            dir,
            models,
            client: free_port(),
        }
    }

    fn serve(&self, certs: &Path, log: &Path) -> Process {
        Process(
            command(env!("CARGO_BIN_EXE_mdma-serve"))
                .env("MDMA_MODELS_DIR", &self.models)
                .env("MDMA_N_CTX", "64")
                .env("MDMA_BACKEND", "cpu")
                .env("MDMA_LISTEN", format!("127.0.0.1:{}", self.client))
                .env("MDMA_POWER", "off")
                .env("MDMA_FEDERATION_CONFIG", self.dir.join("runtime.toml"))
                .env("MDMA_TLS_CERT", certs.join("server.pem"))
                .env("MDMA_TLS_KEY", certs.join("server-key.pem"))
                .env("MDMA_TLS_CA", certs.join("ca.pem"))
                .stdout(std::fs::File::create(log).unwrap())
                .stderr(std::fs::File::create(log.with_extension("err")).unwrap())
                .spawn()
                .unwrap(),
        )
    }
}

/// Waits until the server's stderr log mentions `line`; returns the log.
fn logged(log: &Path, line: &str) -> String {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let text = std::fs::read_to_string(log.with_extension("err")).unwrap_or_default();
        if text.contains(line) {
            return text;
        }
        assert!(Instant::now() < deadline, "waiting for {line:?}:\n{text}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The client and server certificates both nodes' mdma-serve use.
fn certs(root: &Path) -> PathBuf {
    let certs = root.join("certs");
    assert!(
        command(env!("CARGO_BIN_EXE_mdma-certs"))
            .arg(&certs)
            .output()
            .unwrap()
            .status
            .success()
    );
    certs
}

/// The origin routing the executor's catalog for the test client.
fn routes(origin: &Node, certs: &Path) -> Output {
    fed(&[
        "routes",
        path(&origin.dir),
        "--peer",
        "executor.fleet.test",
        "--client-cert",
        path(&certs.join("client.pem")),
    ])
}

/// Asserts `output` failed, saying `why`.
fn refused(output: &Output, why: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(why),
        "expected a refusal for {why:?}: {stderr}"
    );
}

/// Retries `attempt` until it succeeds, for things that wait on a starting node.
fn eventually(what: &str, log: &Path, mut attempt: impl FnMut() -> Output) -> String {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let output = attempt();
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).into_owned();
        }
        assert!(
            Instant::now() < deadline,
            "{what}: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(log.with_extension("err")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn an_origin_routes_an_executors_model_set_up_with_mdma_fed() {
    let _turn = daemon_turn();
    let model = fixture::model(8);
    let root = tempfile::tempdir().unwrap();
    let certs = certs(root.path());
    let (origin, executor) = (
        Node::new(root.path(), "origin", Some(model.path())),
        Node::new(root.path(), "executor", Some(model.path())),
    );

    let enrolled = ok(&fed(&[
        "enroll",
        path(&executor.dir),
        path(&root.path().join("origin.json")),
        "--execute",
        "all",
        "--models",
        path(&executor.models),
    ]));
    assert!(
        enrolled.contains("enrolled origin.fleet.test") && enrolled.contains("runs tiny"),
        "{enrolled}"
    );
    ok(&fed(&[
        "enroll",
        path(&origin.dir),
        path(&root.path().join("executor.json")),
    ]));

    let executor_log = root.path().join("executor.log");
    let running = executor.serve(&certs, &executor_log);
    // The executor lists its model once it has hashed it.
    let routes = eventually("routes", &executor_log, || routes(&origin, &certs));
    assert_eq!(routes.trim(), "executor-tiny -> executor.fleet.test tiny");
    let status = ok(&fed(&["status", path(&origin.dir)]));
    assert!(
        status.contains(
            "route    executor-tiny -> executor.fleet.test tiny (synthesize, 1 client(s))"
        ),
        "{status}"
    );

    // Startup on the origin checks exactly this: routes authorized, polling on.
    let config = mdma::federation::runtime::Config::load(&origin.dir.join("runtime.toml")).unwrap();
    assert!(config.enable_routing && config.poll_catalogs && !config.enable_execution);
    let pem = |name: &str| {
        rustls_pki_types::CertificateDer::pem_file_iter(certs.join(name))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let client_listen = format!("127.0.0.1:{}", origin.client).parse().unwrap();
    drop(
        mdma::federation::runtime::Prepared::load(
            config,
            client_listen,
            &pem("ca.pem"),
            &pem("server.pem"),
        )
        .unwrap(),
    );

    // A running node shows its state but refuses changes.
    let shown = ok(&fed(&["status", path(&executor.dir)]));
    assert!(
        shown.contains("peer     origin.fleet.test Active")
            && shown.contains("runs     tiny (synthesize)"),
        "{shown}"
    );
    let refused = fed(&["export", path(&executor.dir)]);
    assert!(
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stderr).contains("stop mdma-serve first")
    );

    // Only a node that routes may start without local models.
    drop(running);
    let empty = root.path().join("empty-models");
    std::fs::create_dir(&empty).unwrap();
    let mut plain = command(env!("CARGO_BIN_EXE_mdma-serve"))
        .env("MDMA_MODELS_DIR", &empty)
        .env("MDMA_BACKEND", "cpu")
        .env("MDMA_LISTEN", format!("127.0.0.1:{}", free_port()))
        .env("MDMA_POWER", "off")
        .env("MDMA_TLS_CERT", certs.join("server.pem"))
        .env("MDMA_TLS_KEY", certs.join("server-key.pem"))
        .env("MDMA_TLS_CA", certs.join("ca.pem"))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_mins(1);
    while plain.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "a node without models kept running"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = plain.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("no usable models"),
        "{stderr}"
    );
    std::fs::remove_file(origin.models.join("tiny.gguf")).unwrap();
    let origin_log = root.path().join("origin.log");
    let _origin = origin.serve(&certs, &origin_log);
    let text = logged(&origin_log, "mdma listening on");
    assert!(text.contains("serving routes only"), "{text}");
}

#[test]
fn an_empty_executor_takes_the_models_its_peers_push() {
    let _turn = daemon_turn();
    let (tiny, check, other) = (fixture::model(8), fixture::minicheck(8), fixture::model(16));
    let root = tempfile::tempdir().unwrap();
    let certs = certs(root.path());
    let (origin, executor) = (
        Node::new(root.path(), "origin", None),
        Node::new(root.path(), "executor", None),
    );
    let enroll = |floor: &str| {
        ok(&fed(&[
            "enroll",
            path(&executor.dir),
            path(&root.path().join("origin.json")),
            "--install",
            floor,
            "--installed",
        ]))
    };
    let enrolled = enroll("0");
    assert!(
        enrolled.contains("runs every installed model") && enrolled.contains("installs models"),
        "{enrolled}"
    );
    ok(&fed(&[
        "enroll",
        path(&origin.dir),
        path(&root.path().join("executor.json")),
    ]));

    let log = root.path().join("executor.log");
    let running = executor.serve(&certs, &log);
    logged(&log, "waiting for peers to install some");
    let push = |file: &Path, name: &str| {
        fed(&[
            "push",
            path(&origin.dir),
            path(file),
            "--peer",
            "executor.fleet.test",
            "--name",
            name,
        ])
    };
    let pushed = eventually("push", &log, || push(tiny.path(), "tiny"));
    assert!(
        pushed.starts_with("pushed tiny to executor.fleet.test"),
        "{pushed}"
    );
    // The same bytes under another name are not sent again.
    assert_eq!(
        ok(&push(tiny.path(), "copy")).trim(),
        "executor.fleet.test already holds these bytes as tiny"
    );
    // A taken name and a file that is no model are refused, leaving nothing behind.
    refused(&push(other.path(), "tiny"), "model name taken");
    let junk = root.path().join("junk.gguf");
    std::fs::write(&junk, b"not a model").unwrap();
    refused(&push(&junk, "junk"), "not a model this node serves");
    let files = || {
        let mut files: Vec<_> = std::fs::read_dir(&executor.models)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        files
    };
    assert_eq!(files(), ["tiny.gguf"]);
    // A decision model is installed as one.
    ok(&push(check.path(), "check"));
    assert_eq!(files(), ["check.gguf", "tiny.gguf"]);
    let status = ok(&fed(&["status", path(&executor.dir)]));
    assert!(
        status.contains("model    check (decide) installed by origin.fleet.test")
            && status.contains("model    tiny (synthesize) installed by origin.fleet.test"),
        "{status}"
    );

    // The origin sees and routes what it pushed, without a grant naming it.
    let route = || routes(&origin, &certs);
    let routed =
        "executor-check -> executor.fleet.test check\nexecutor-tiny -> executor.fleet.test tiny";
    assert_eq!(ok(&route()).trim(), routed);

    // Installs outlive a restart, and a node keeps the disk it is told to.
    drop(running);
    enroll("1000000T");
    let _restarted = executor.serve(&certs, &log);
    assert_eq!(
        eventually("routes after restart", &log, route).trim(),
        routed
    );
    refused(
        &push(other.path(), "other"),
        "bytes would leave less than the 1099511627776000000 bytes this node keeps free",
    );
    assert_eq!(files(), ["check.gguf", "tiny.gguf"]);
}
