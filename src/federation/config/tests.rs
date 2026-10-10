use super::*;

pub(crate) fn peer() -> Peer {
    Peer {
        server_id: "remote.mdma.test".into(),
        endpoint: "https://127.0.0.1:9443".into(),
        tls_name: "remote.mdma.test".into(),
        ca_roots: vec![CaRoot {
            path: "peer-ca.pem".into(),
            certificate_sha256: format!("sha256:{}", "cd".repeat(32)),
        }],
        tls_pins: vec![TlsPin {
            role: TlsRole::ClientAuth,
            spki_sha256: format!("sha256:{}", "ab".repeat(32)),
            window: Window {
                valid_from_ms: 100,
                valid_until_ms: 2000,
            },
        }],
        signing_keys: vec![SigningKey {
            key_id: "ed25519:first".into(),
            public_key: URL_SAFE_NO_PAD.encode([7; 32]),
            window: Window {
                valid_from_ms: 100,
                valid_until_ms: 1000,
            },
        }],
        control_permissions: ControlPermissions::default(),
        catalog_exports: vec![],
        execute_exports: vec![],
        route_imports: vec![],
        limits: Limits::default(),
    }
}

#[test]
fn defaults_deny_all_grants() {
    let text = r#"
server_id = "local.mdma.test"
trust_state = "trust.json"
[[peers]]
server_id = "remote.mdma.test"
endpoint = "https://127.0.0.1:9443"
tls_name = "remote.mdma.test"
ca_roots = [{ path = "ca.pem", certificate_sha256 = "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd" }]
"#;
    let file = File::parse(text).unwrap();
    assert_eq!(
        file.peers[0].control_permissions,
        ControlPermissions::default()
    );
    assert!(file.peers[0].execute_exports.is_empty());
    assert!(file.peers[0].route_imports.is_empty());
    assert!(file.peers[0].tls_pins.is_empty());
}

#[test]
fn parses_explicit_credential_windows() {
    let p = peer();
    let text = toml::to_string(&p).unwrap();
    let parsed: Peer = toml::from_str(&text).unwrap();
    assert_eq!(parsed, p);
    parsed.validate().unwrap();
}

#[test]
fn unknown_fields_fail_including_nested_grants() {
    let text = toml::to_string(&peer()).unwrap();
    assert!(toml::from_str::<Peer>(&format!("mystery = true\n{text}")).is_err());
    let text = text.replace(
        "identify = false",
        "identify = false\nexecute_everything = true",
    );
    assert!(toml::from_str::<Peer>(&text).is_err());
}

#[test]
fn reject_self_duplicate_and_oversized_config() {
    let p = peer();
    let file = File {
        server_id: p.server_id.clone(),
        trust_state: "state".into(),
        peers: vec![p.clone()],
    };
    assert!(file.validate().is_err());
    let file = File {
        server_id: "local.mdma.test".into(),
        peers: vec![p.clone(), p],
        ..file
    };
    assert!(file.validate().is_err());
    assert!(matches!(
        File::parse(&"x".repeat(MAX_CONFIG_BYTES + 1)),
        Err(Error::Size)
    ));
}

#[test]
fn endpoint_is_only_an_explicit_https_origin() {
    for value in [
        "http://node.test:443",
        "https://node.test",
        "https://node.test:0",
        "https://user@node.test:443",
        "https://node.test:443/path",
        "https://node.test:443/?",
        "https://node.test:443/#x",
        "https://node.test:443/%2f",
        "https://node.test:65536",
        "//node.test:443",
        "https://node.test:443\\x",
    ] {
        assert!(validate_endpoint(value).is_err(), "{value}");
    }
    for value in [
        "https://node.test:443",
        "https://node.test:443/",
        "https://[::1]:9443/",
    ] {
        validate_endpoint(value).unwrap();
    }
}

#[test]
fn reject_bad_windows_encodings_and_duplicate_credentials() {
    assert!(
        Window {
            valid_from_ms: 10,
            valid_until_ms: 10
        }
        .validate()
        .is_err()
    );
    assert!(
        Window {
            valid_from_ms: 0,
            valid_until_ms: MAX_SAFE_TIMESTAMP + 1
        }
        .validate()
        .is_err()
    );
    let mut p = peer();
    p.signing_keys[0].public_key.push('=');
    assert!(p.validate().is_err());
    let mut p = peer();
    p.signing_keys.push(p.signing_keys[0].clone());
    assert!(p.validate().is_err());
    let mut p = peer();
    p.tls_pins[0].spki_sha256 = format!("sha256:{}", "AB".repeat(32));
    assert!(p.validate().is_err());
    let mut p = peer();
    p.tls_pins.push(p.tls_pins[0].clone());
    assert!(p.validate().is_err());
}

#[test]
fn no_direction_or_permission_inherited_from_other_role() {
    let p = peer();
    p.validate().unwrap();
    assert!(p.tls_pins.iter().all(|p| p.role != TlsRole::ServerAuth));
    let mut p = p;
    p.control_permissions.scan_capabilities = true;
    assert!(p.execute_exports.is_empty());
    assert!(p.route_imports.is_empty());
}

#[test]
fn resource_bounds_cannot_be_unlimited() {
    let mut p = peer();
    p.limits.ledger_entries = 0;
    assert!(p.validate().is_err());
    p.limits.ledger_entries = 2;
    p.limits.concurrency = 3;
    assert!(p.validate().is_err());
}
