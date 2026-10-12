use super::*;
use std::collections::HashMap;

fn load_exact(env: &HashMap<&str, &str>) -> Result<Config, BoxError> {
    Config::load(|name| env.get(name).map(|v| (*v).to_owned()).ok_or(VarError::NotPresent))
}

fn load(vars: &[(&str, &str)]) -> Result<Config, BoxError> {
    let mut env: HashMap<&str, &str> = HashMap::from([
        ("MDMA_TLS_CERT", "client.pem"),
        ("MDMA_TLS_KEY", "client-key.pem"),
        ("MDMA_TLS_CA", "ca.pem"),
    ]);
    env.extend(vars.iter().copied());
    load_exact(&env)
}

#[test]
fn defaults_bind_loopback_and_reach_local_server() {
    let config = load(&[]).unwrap();
    assert_eq!(config.listen.to_string(), "127.0.0.1:6364");
    assert_eq!(config.server, "https://localhost:6362");
    assert_eq!(config.max_streams, 2);
    assert_eq!(config.watch_interval, Duration::from_secs(2));
}

#[test]
fn listen_accepts_loopback_only() {
    for (listen, ok) in [
        ("127.0.0.1:0", true),
        ("[::1]:8080", true),
        ("0.0.0.0:6364", false),
        ("[::]:6364", false),
        ("192.168.1.10:6364", false),
        ("localhost:6364", false),
    ] {
        assert_eq!(load(&[("MDMA_WEB_LISTEN", listen)]).is_ok(), ok, "listen: {listen}");
    }
}

#[test]
fn rejects_missing_credentials_and_out_of_range_limits() {
    let mut env = HashMap::from([("MDMA_TLS_CERT", "c"), ("MDMA_TLS_KEY", "k")]);
    let error = load_exact(&env).err().unwrap().to_string();
    assert!(error.contains("MDMA_TLS_CA"), "{error}");
    env.insert("MDMA_TLS_CA", " ");
    let error = load_exact(&env).err().unwrap().to_string();
    assert!(error.contains("must not be empty"), "{error}");

    for (name, value) in [
        ("MDMA_WEB_MAX_STREAMS", "0"),
        ("MDMA_WEB_MAX_STREAMS", "9"),
        ("MDMA_WEB_WATCH_MS", "100"),
        ("MDMA_WEB_WATCH_MS", "fast"),
    ] {
        let error = load(&[(name, value)]).err().unwrap().to_string();
        assert!(error.contains(name), "{name}={value}: {error}");
    }
}
