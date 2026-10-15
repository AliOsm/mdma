//! Federation set up with nothing but mdma-fed: a real executor serves its
//! signed catalog to the origin, whose routes then pass mdma-serve's startup
//! checks. One daemon runs per machine, so the origin is not started here.
#[path = "runtime_cli/fixture.rs"]
mod fixture;
#[path = "runtime_cli/process.rs"]
mod process;

use process::{Process, command, free_port};
use rustls_pki_types::pem::PemObject;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

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
    fn new(root: &Path, name: &str, model: &Path) -> Self {
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
        std::fs::copy(model, models.join("tiny.gguf")).unwrap();
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
    let model = fixture::model(8);
    let root = tempfile::tempdir().unwrap();
    let certs = root.path().join("certs");
    assert!(
        command(env!("CARGO_BIN_EXE_mdma-certs"))
            .arg(&certs)
            .output()
            .unwrap()
            .status
            .success()
    );
    let (origin, executor) = (
        Node::new(root.path(), "origin", model.path()),
        Node::new(root.path(), "executor", model.path()),
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
    let routes = eventually("routes", &executor_log, || {
        fed(&[
            "routes",
            path(&origin.dir),
            "--peer",
            "executor.fleet.test",
            "--client-cert",
            path(&certs.join("client.pem")),
        ])
    });
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
