//! Explicit checkpoint smoke tests of the actual serve/client binaries.
use std::{fs::File, process::{Child, Command, Stdio}, time::{Duration, Instant}};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn smoke(backend: &str) {
    let paths = ["MDMA_TEST_TINYLLAMA_GGUF", "MDMA_TEST_GEMMA4_GGUF"].map(|variable| {
        let path = std::env::var(variable).unwrap_or_else(|_| panic!("set {variable} to the pinned checkpoint"));
        std::fs::canonicalize(path).unwrap().to_str().unwrap().to_string()
    });
    let dir = tempfile::tempdir().unwrap();
    let certs = dir.path().join("certs");
    let generated = Command::new(env!("CARGO_BIN_EXE_mdma-certs")).arg(&certs).output().unwrap();
    assert!(generated.status.success(), "certificate generation failed");
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    for (name, path) in ["tiny.gguf", "gemma.gguf"].into_iter().zip(&paths) {
        std::os::unix::fs::symlink(path, models.join(name)).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let log = dir.path().join("serve.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_mdma-serve"));
    for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("MDMA_")) { command.env_remove(key); }
    let mut server = Server(command
        .env("MDMA_MODELS_DIR", models).env("MDMA_DEFAULT_MODEL", "tiny").env("MDMA_BACKEND", backend)
        .env("MDMA_LISTEN", addr.to_string()).env("MDMA_POWER", "off")
        .env("MDMA_N_CTX", "64").env("MDMA_SHUTDOWN_TIMEOUT_SECS", "30")
        .env("MDMA_TLS_CERT", certs.join("server.pem"))
        .env("MDMA_TLS_KEY", certs.join("server-key.pem"))
        .env("MDMA_TLS_CA", certs.join("ca.pem"))
        .stdout(Stdio::null()).stderr(File::create(&log).unwrap()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(server.0.try_wait().unwrap().is_none(), "serve exited: {}", std::fs::read_to_string(&log).unwrap());
        if std::net::TcpStream::connect(addr).is_ok() { break; }
        assert!(Instant::now() < deadline, "serve startup timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
    let client = |model: &str, prompt: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdma-client"));
        command.env("MDMA_ADDR", format!("localhost:{}", addr.port()))
            .env("MDMA_TLS_CERT", certs.join("client.pem"))
            .env("MDMA_TLS_KEY", certs.join("client-key.pem"))
            .env("MDMA_TLS_CA", certs.join("ca.pem"))
            .env("MDMA_MODEL", model).env("MDMA_MAX_TOKENS", "3");
        if let Some(prompt) = prompt { command.arg(prompt); }
        command.output().unwrap()
    };
    let health = client("", None);
    assert!(health.status.success(), "Introspect: {}", String::from_utf8_lossy(&health.stderr));
    let startup = std::fs::read_to_string(&log).unwrap();
    assert!(startup.find("mdma warm-up complete").unwrap() < startup.find("mdma listening").unwrap());
    let health = String::from_utf8(health.stdout).unwrap();
    for expected in ["ready=true", "default=tiny", "tiny", "gemma", "arch=llama", "arch=gemma4", "n_ctx=64"] {
        assert!(health.contains(expected), "missing {expected}: {health}");
    }
    for model in ["", "tiny", "gemma"] {
        let output = client(model, Some("The capital of France is"));
        assert!(output.status.success(), "{model}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(!output.stdout.is_empty(), "{model} must stream text");
        if model == "gemma" {
            assert!(String::from_utf8_lossy(&output.stdout).contains("Paris"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("[2 tokens"), "Gemma must stop at end-of-turn");
        }
        let rejected = client(model, Some(&" ab".repeat(100)));
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("ResourceExhausted"));
    }
    assert!(Command::new("kill").args(["-TERM", &server.0.id().to_string()]).status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success(), "shutdown: {}", std::fs::read_to_string(&log).unwrap());
            break;
        }
        assert!(Instant::now() < deadline, "serve shutdown timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(std::net::TcpStream::connect(addr).is_err());
}

#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and MDMA_TEST_GEMMA4_GGUF; starts local mTLS binaries"]
fn two_model_cpu_cli_lifecycle() { smoke("cpu"); }

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
#[ignore = "requires MDMA_TEST_TINYLLAMA_GGUF and MDMA_TEST_GEMMA4_GGUF; starts local mTLS binaries"]
fn two_model_metal_cli_lifecycle() { smoke("metal"); }
