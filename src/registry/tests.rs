use super::*;

#[test]
fn parses_every_kind_and_picks_the_default() {
    let file = ModelsFile::parse(
        r#"
default = "laya"

[[models]]
name = "minicheck"
gguf = "/models/minicheck.gguf"
n_ctx = 2048

[[models]]
name = "laya"
laya = "/models/laya"

[[models]]
name = "qwen"
[models.upstream]
url = "https://worker.example:50051"
model = "remote-qwen"
tls_cert = "/certs/client.pem"
tls_key = "/certs/client-key.pem"
tls_ca = "/certs/ca.pem"
"#,
    )
    .unwrap();
    assert_eq!(file.models.len(), 3);
    assert_eq!(file.default_index(), 1);
    assert_eq!(file.models[0].kind().unwrap(), Kind::Gguf);
    assert_eq!(file.models[0].n_ctx, Some(2048));
    assert_eq!(file.models[1].kind().unwrap(), Kind::Laya);
    assert_eq!(file.models[2].kind().unwrap(), Kind::Upstream);
    assert_eq!(file.models[2].upstream.as_ref().unwrap().model, "remote-qwen");
}

#[test]
fn rejects_ambiguous_duplicate_empty_and_unknown_default() {
    for (text, needle) in [
        ("[[models]]\nname = \"a\"\ngguf = \"x\"\nlaya = \"y\"\n", "exactly one"),
        ("[[models]]\nname = \"a\"\n", "exactly one"),
        ("[[models]]\nname = \"a\"\ngguf = \"x\"\n[[models]]\nname = \"a\"\ngguf = \"y\"\n", "twice"),
        ("models = []\n", "no models"),
        ("[[models]]\nname = \"\"\ngguf = \"x\"\n", "empty name"),
        ("default = \"b\"\n[[models]]\nname = \"a\"\ngguf = \"x\"\n", "not listed"),
        ("[[models]]\nname = \"a\"\ngguf = \"x\"\nn_ctx = 0\n", "positive"),
    ] {
        let err = ModelsFile::parse(text).err().unwrap_or_else(|| panic!("accepted {text:?}")).to_string();
        assert!(err.contains(needle), "{text:?}: {err}");
    }
}

#[test]
fn single_gguf_is_named_after_the_file() {
    let file = ModelsFile::single_gguf("/models/Bespoke-MiniCheck-7B-Q4_K_M.gguf");
    assert_eq!(file.models[0].name, "Bespoke-MiniCheck-7B-Q4_K_M");
    assert_eq!(file.models[0].kind().unwrap(), Kind::Gguf);
    assert_eq!(file.default_index(), 0);
}

#[test]
fn upstream_requires_mtls_configuration() {
    let entry = "[[models]]\nname = \"remote\"\n";
    assert!(ModelsFile::parse(&format!("{entry}upstream = \"http://localhost:8181\"")).is_err());
    assert!(ModelsFile::parse(&format!("{entry}[models.upstream]\nurl = \"https://localhost:50051\"")).is_err());
    let tls = "tls_cert = \"/nonexistent/client.pem\"\ntls_key = \"/nonexistent/client-key.pem\"\ntls_ca = \"/nonexistent/ca.pem\"\n";
    let text = format!("{entry}[models.upstream]\nurl = \"https://localhost:50051\"\n{tls}");
    let file = ModelsFile::parse(&text).unwrap();
    assert!(file.models[0].upstream.as_ref().unwrap().model.is_empty());
    let err = Registry::load(&CpuBackend, &file, 64, |_| {}).err().unwrap().to_string();
    assert!(err.starts_with("model remote:"), "{err}");
    for invalid in [
        text.replace("https://", "http://"),
        text.replace("/nonexistent/ca.pem", ""),
        format!("{text}model = \"  \""),
        format!("{text}tls_typo = \"x\""),
    ] {
        assert!(ModelsFile::parse(&invalid).is_err(), "{invalid}");
    }
}

#[test]
fn a_missing_gguf_names_the_model_in_the_error() {
    let file = ModelsFile::parse("[[models]]\nname = \"ghost\"\ngguf = \"/nonexistent.gguf\"\n").unwrap();
    let err = Registry::load(&CpuBackend, &file, 64, |_| {}).err().unwrap().to_string();
    assert!(err.starts_with("model ghost:"), "{err}");
}
