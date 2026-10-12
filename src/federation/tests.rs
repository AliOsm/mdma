use std::assert_matches;
use super::canonical_json::{self, Value};
use super::catalog::{self, CatalogBody, Kind, Limits, ModelDescriptor, ServerState, VerifyContext};
use super::server_id;
use super::signing::{self, KeyPairHandle};
use std::collections::BTreeMap;

// -- server_id -------------------------------------------------------------

#[test]
fn server_id_accepts_spec_examples() {
    assert!(server_id::validate("alice.example.net").is_ok());
    assert!(server_id::validate("node-a.mdma.test").is_ok());
}

#[test]
fn server_id_rejects_single_label() {
    assert_eq!(server_id::validate("localhost"), Err(server_id::Error::TooFewLabels));
}

#[test]
fn server_id_rejects_ip_literal() {
    assert_eq!(server_id::validate("127.0.0.1"), Err(server_id::Error::IpLiteral));
    assert_eq!(server_id::validate("::1"), Err(server_id::Error::IpLiteral));
}

#[test]
fn server_id_rejects_port_scheme_and_trailing_dot() {
    assert_eq!(server_id::validate("alice.example.net:8443"), Err(server_id::Error::HasPort));
    assert_eq!(server_id::validate("https://alice.example.net"), Err(server_id::Error::HasScheme));
    assert_eq!(server_id::validate("alice.example.net."), Err(server_id::Error::TrailingDot));
}

#[test]
fn server_id_rejects_uppercase_and_bad_labels() {
    assert_eq!(server_id::validate("Alice.example.net"), Err(server_id::Error::NotLowercase));
    assert_eq!(server_id::validate("-alice.example.net"), Err(server_id::Error::LabelBoundary("-alice".to_string())));
    assert_eq!(server_id::validate("alice..net"), Err(server_id::Error::EmptyLabel(String::new())));
    assert_eq!(server_id::validate("ali ce.example.net"), Err(server_id::Error::InvalidLabelChar("ali ce".to_string())));
}

#[test]
fn server_id_rejects_peer_equal_to_local_identity_is_caller_responsibility() {
    // The spec requires rejecting a peer name equal to the local identity,
    // but that comparison needs the local identity, which lives in the
    // not-yet-implemented peer trust store, not in syntax validation.
    assert!(server_id::validate("self.example.net").is_ok());
}

// -- canonical_json ----------------------------------------------------------

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs.into_iter().map(|(k, v)| (k.to_string(), v))).unwrap()
}

#[test]
fn canonical_json_sorts_keys_and_is_compact() {
    let v = obj(vec![("b", Value::Integer(2)), ("a", Value::Integer(1))]);
    assert_eq!(v.encode(), r#"{"a":1,"b":2}"#);
}

#[test]
fn canonical_json_round_trips_through_parse() {
    let input = br#"{ "b" : 2, "a" : [1, 2, 3], "c": "hi" }"#;
    let v = canonical_json::parse(input, 16).unwrap();
    assert_eq!(v.encode(), r#"{"a":[1,2,3],"b":2,"c":"hi"}"#);
}

#[test]
fn canonical_json_rejects_duplicate_keys() {
    let input = br#"{"a":1,"a":2}"#;
    let err = canonical_json::parse(input, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::DuplicateKey("a".to_string()));
}

#[test]
fn canonical_json_rejects_floats() {
    let err = canonical_json::parse(br#"{"a":1.5}"#, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::NonInteger);
    let err = canonical_json::parse(br#"{"a":1e10}"#, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::NonInteger);
}

#[test]
fn canonical_json_rejects_out_of_range_integers() {
    let err = canonical_json::parse(br#"{"a":9007199254740992}"#, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::IntegerOutOfRange("9007199254740992".to_string()));
}

#[test]
fn canonical_json_rejects_lone_surrogates() {
    let err = canonical_json::parse(br#"{"a":"\ud800"}"#, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::LoneSurrogate);
}

#[test]
fn canonical_json_accepts_surrogate_pairs() {
    // U+1F600 GRINNING FACE, as a UTF-16 surrogate pair escape.
    let input = r#"{"a":"😀"}"#;
    let v = canonical_json::parse(input.as_bytes(), 16).unwrap();
    assert_eq!(v.as_object().unwrap().get("a").unwrap().as_str(), Some("\u{1F600}"));
}

#[test]
fn canonical_json_decodes_u_escape_surrogate_pair() {
    // Same character as above, spelled as a \u-escaped UTF-16 surrogate pair.
    let input: &[u8] = b"{\"a\":\"\\ud83d\\ude00\"}";
    let v = canonical_json::parse(input, 16).unwrap();
    assert_eq!(v.as_object().unwrap().get("a").unwrap().as_str(), Some("\u{1F600}"));
}

#[test]
fn canonical_json_rejects_unescaped_control_chars() {
    let raw = b"{\"a\":\"x\ny\"}";
    let err = canonical_json::parse(raw, 16).unwrap_err();
    assert_eq!(err, canonical_json::Error::UnescapedControl);
}

#[test]
fn canonical_json_enforces_depth_limit() {
    let nested = "[".repeat(5) + &"]".repeat(5);
    assert!(canonical_json::parse(nested.as_bytes(), 5).is_ok());
    assert_eq!(canonical_json::parse(nested.as_bytes(), 4).unwrap_err(), canonical_json::Error::TooDeep(4));
}

#[test]
fn canonical_json_does_not_escape_non_ascii_or_slash() {
    let v = Value::String("a/b caf\u{e9}".to_string());
    assert_eq!(v.encode(), "\"a/b caf\u{e9}\"");
}

#[test]
fn canonical_json_object_rejects_duplicate_builder_keys() {
    let err = Value::object(vec![("a".to_string(), Value::Null), ("a".to_string(), Value::Null)]).unwrap_err();
    assert_eq!(err, canonical_json::Error::DuplicateKey("a".to_string()));
}

// -- signing -----------------------------------------------------------------

#[test]
fn key_id_validation() {
    assert!(signing::validate_key_id("ed25519:prod-1").is_ok());
    assert_eq!(signing::validate_key_id("rsa:prod-1"), Err(signing::KeyIdError::MissingPrefix));
    assert_eq!(signing::validate_key_id("ed25519:"), Err(signing::KeyIdError::BadLabelLength));
    assert_eq!(signing::validate_key_id("ed25519:Prod"), Err(signing::KeyIdError::BadLabelChar));
    let too_long = format!("ed25519:{}", "a".repeat(33));
    assert_eq!(signing::validate_key_id(&too_long), Err(signing::KeyIdError::BadLabelLength));
}

#[test]
fn sign_and_verify_round_trip() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = obj(vec![("hello", Value::String("world".to_string()))]);
    let sig = signing::sign(&key_pair, &body, "ed25519:prod-1");
    assert!(signing::verify(&public_key, &body, "ed25519:prod-1", &sig).is_ok());
}

#[test]
fn verify_rejects_tampered_body() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = obj(vec![("hello", Value::String("world".to_string()))]);
    let sig = signing::sign(&key_pair, &body, "ed25519:prod-1");
    let tampered = obj(vec![("hello", Value::String("mallory".to_string()))]);
    assert!(signing::verify(&public_key, &tampered, "ed25519:prod-1", &sig).is_err());
}

#[test]
fn verify_rejects_wrong_key_id() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = obj(vec![("hello", Value::String("world".to_string()))]);
    let sig = signing::sign(&key_pair, &body, "ed25519:prod-1");
    assert!(signing::verify(&public_key, &body, "ed25519:prod-2", &sig).is_err());
}

#[test]
fn preimage_prefix_ends_with_single_lf() {
    let body = Value::Object(BTreeMap::new());
    let msg = signing::preimage(&body, "ed25519:prod-1");
    assert!(msg.starts_with(b"mdma.federation.catalog.v1\n"));
    assert!(!msg.starts_with(b"mdma.federation.catalog.v1\\n"));
}

// -- catalog -------------------------------------------------------------

fn sample_descriptor() -> ModelDescriptor {
    ModelDescriptor {
        model: "llama-70b".to_string(),
        kind: Kind::Gguf,
        artifact_id: format!("sha256:{}", "a".repeat(64)),
        profile_id: format!("sha256:{}", "b".repeat(64)),
        architecture: "llama".to_string(),
        backend: "cuda".to_string(),
        ready: true,
        max_input_bytes: 4096,
        class: crate::registry::Class::Decision,
        capabilities: vec![crate::registry::Capability::Reasoning, crate::registry::Capability::Vision],
        resident: true,
        limits: Limits::Synthesize { n_ctx: 8192, max_output_tokens: 2048 },
    }
}

fn sample_body() -> CatalogBody {
    CatalogBody {
        origin: "alice.example.net".to_string(),
        destination: "bob.example.net".to_string(),
        nonce: vec![7u8; 32],
        issued_at_ms: 1_000_000,
        expires_at_ms: 1_030_000,
        server_state: ServerState::Serving,
        models: vec![sample_descriptor()],
        host: crate::grpc::Host {
            name: "alice".into(),
            os: "freebsd".into(),
            os_name: "FreeBSD 15.1-RELEASE".into(),
            arch: "amd64".into(),
            cpu: "AMD EPYC 9124".into(),
            memory_bytes: 64 << 30,
            device: "CPU".into(),
            memory_available_bytes: 41 << 30,
            device_memory_bytes: 0,
            device_available_bytes: 0,
            disk_bytes: 2 << 40,
            disk_available_bytes: 1 << 40,
        },
    }
}

#[test]
fn descriptor_validation_accepts_sample() {
    assert!(sample_descriptor().validate().is_ok());
}

#[test]
fn descriptor_validation_rejects_output_over_context() {
    let mut d = sample_descriptor();
    d.limits = Limits::Synthesize { n_ctx: 100, max_output_tokens: 200 };
    assert_eq!(d.validate(), Err(catalog::Error::InvalidField("limits")));
}

#[test]
fn descriptor_validation_rejects_bad_digest() {
    let mut d = sample_descriptor();
    d.artifact_id = "sha256:not-hex".to_string();
    assert_eq!(d.validate(), Err(catalog::Error::InvalidField("artifact_id")));
}

#[test]
fn descriptor_round_trips_through_canonical() {
    let d = sample_descriptor();
    let v = d.to_canonical();
    let decoded = ModelDescriptor::from_canonical(&v).unwrap();
    assert_eq!(d, decoded);
}

#[test]
fn residency_is_not_part_of_the_selection() {
    let loaded = sample_descriptor();
    let on_disk = ModelDescriptor { resident: false, ..loaded.clone() };
    assert_ne!(loaded, on_disk);
    assert!(loaded.same_selection(&on_disk) && on_disk.same_selection(&loaded));
    let swapped = ModelDescriptor { artifact_id: format!("sha256:{}", "c".repeat(64)), ..on_disk };
    assert!(!loaded.same_selection(&swapped));
    let Value::Object(mut obj) = loaded.to_canonical() else { unreachable!() };
    obj.remove("resident");
    assert_eq!(ModelDescriptor::from_canonical(&Value::Object(obj)).unwrap_err(), catalog::Error::MissingField("resident"));
}

#[test]
fn class_capabilities_and_host_are_closed_and_bounded() {
    let mut d = sample_descriptor();
    d.capabilities.reverse();
    assert_eq!(d.validate(), Err(catalog::Error::InvalidField("capabilities")));
    let mut body = sample_body();
    body.host.cpu = "x".repeat(crate::host::MAX_FIELD_BYTES + 1);
    assert_eq!(body.validate(), Err(catalog::Error::InvalidField("host")));

    let descriptor = |key: &str, value: Value| {
        let Value::Object(mut obj) = sample_descriptor().to_canonical() else { unreachable!() };
        obj.insert(key.into(), value);
        ModelDescriptor::from_canonical(&Value::Object(obj)).unwrap_err()
    };
    assert_eq!(descriptor("class", Value::String("llm".into())), catalog::Error::InvalidField("class"));
    assert_eq!(descriptor("capabilities", Value::Array(vec![Value::String("telepathy".into())])), catalog::Error::InvalidField("capabilities"));
    let Value::Object(mut body) = sample_body().to_canonical() else { unreachable!() };
    let Some(Value::Object(host)) = body.get_mut("host") else { unreachable!() };
    host.insert("serial".into(), Value::String("C02X".into()));
    assert_eq!(CatalogBody::from_canonical(&Value::Object(body)).unwrap_err(), catalog::Error::UnknownField("serial".into()));
}

#[test]
fn body_validation_rejects_duplicate_model_names() {
    let mut body = sample_body();
    body.models.push(sample_descriptor());
    assert_eq!(body.validate(), Err(catalog::Error::DuplicateModel("llama-70b".to_string())));
}

#[test]
fn body_validation_rejects_ready_model_when_draining() {
    let mut body = sample_body();
    body.server_state = ServerState::Draining;
    assert_eq!(body.validate(), Err(catalog::Error::ReadyWhileNotAccepting));
}

#[test]
fn body_validation_rejects_lifetime_over_60s() {
    let mut body = sample_body();
    body.expires_at_ms = body.issued_at_ms + 60_001;
    assert_eq!(body.validate(), Err(catalog::Error::BadLifetime));
}

#[test]
fn body_validation_rejects_bad_server_id() {
    let mut body = sample_body();
    body.origin = "localhost".to_string();
    assert_eq!(body.validate(), Err(catalog::Error::ServerId(server_id::Error::TooFewLabels)));
}

#[test]
fn body_round_trips_through_canonical() {
    let body = sample_body();
    let v = body.to_canonical();
    let decoded = CatalogBody::from_canonical(&v).unwrap();
    assert_eq!(body, decoded);
}

#[test]
fn from_canonical_rejects_unknown_field() {
    let body = sample_body();
    let Value::Object(mut obj_map) = body.to_canonical() else {
        unreachable!()
    };
    obj_map.insert("extra".to_string(), Value::Bool(true));
    let err = CatalogBody::from_canonical(&Value::Object(obj_map)).unwrap_err();
    assert_eq!(err, catalog::Error::UnknownField("extra".to_string()));
}

#[test]
fn from_canonical_rejects_wrong_type() {
    let body = sample_body();
    let Value::Object(mut obj_map) = body.to_canonical() else {
        unreachable!()
    };
    obj_map.insert("type".to_string(), Value::String("mdma.catalog.v2".to_string()));
    let err = CatalogBody::from_canonical(&Value::Object(obj_map)).unwrap_err();
    assert_eq!(err, catalog::Error::WrongType);
}

const SAMPLE_NONCE: [u8; 32] = [7u8; 32];

fn verify_ctx(now_ms: u64) -> VerifyContext<'static> {
    VerifyContext {
        now_ms,
        expected_origin: "alice.example.net",
        expected_destination: "bob.example.net",
        expected_nonce: &SAMPLE_NONCE,
        key_valid_from_ms: 0,
        key_valid_until_ms: u64::MAX,
    }
}

#[test]
fn sign_and_verify_catalog_round_trip() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let ctx = verify_ctx(1_000_000);
    let verified = catalog::verify_signed_catalog(&envelope, &public_key, &ctx).unwrap();
    assert_eq!(verified, body);
}

#[test]
fn verify_catalog_rejects_tampered_body_json() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let mut envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    // Flip a byte inside the still-valid-JSON nonce string so the parsed
    // value differs from what was signed, without breaking JSON syntax.
    let text = String::from_utf8(envelope.body_json.clone()).unwrap();
    let tampered = text.replacen("bob.example.net", "mallory.example.net", 1);
    envelope.body_json = tampered.into_bytes();
    let ctx = verify_ctx(1_000_000);
    assert!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx).is_err());
}

#[test]
fn verify_catalog_rejects_wrong_nonce() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let mut ctx = verify_ctx(1_000_000);
    ctx.expected_nonce = &[9u8; 32];
    assert_eq!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx), Err(catalog::Error::NonceMismatch));
}

#[test]
fn verify_catalog_rejects_expired() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let ctx = verify_ctx(body.expires_at_ms);
    assert_eq!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx), Err(catalog::Error::Expired));
}

#[test]
fn verify_catalog_rejects_issued_too_far_in_future() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let ctx = verify_ctx(body.issued_at_ms - catalog::MAX_ISSUED_AT_SKEW_MS - 1);
    assert_eq!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx), Err(catalog::Error::IssuedInFuture));
}

#[test]
fn verify_catalog_rejects_key_outside_validity_window() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let mut ctx = verify_ctx(1_000_000);
    ctx.key_valid_until_ms = 999_999; // expired before `now_ms`
    assert_eq!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx), Err(catalog::Error::KeyNotValid));
}

#[test]
fn verify_catalog_rejects_wrong_origin() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    let mut ctx = verify_ctx(1_000_000);
    ctx.expected_origin = "mallory.example.net";
    let err = catalog::verify_signed_catalog(&envelope, &public_key, &ctx).unwrap_err();
    assert_eq!(err, catalog::Error::OriginMismatch { claimed: "alice.example.net".to_string(), authenticated: "mallory.example.net".to_string() });
}

#[test]
fn verify_catalog_rejects_oversized_body() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let public_key = key_pair.public_key_bytes();
    let body = sample_body();
    let mut envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair).unwrap();
    envelope.body_json = vec![b' '; catalog::MAX_BODY_JSON_BYTES + 1];
    let ctx = verify_ctx(1_000_000);
    assert_eq!(catalog::verify_signed_catalog(&envelope, &public_key, &ctx), Err(catalog::Error::BodyTooLarge(catalog::MAX_BODY_JSON_BYTES + 1)));
}

#[test]
fn catalog_signer_refuses_oversized_inventory_before_issuance() {
    let key_pair = KeyPairHandle::generate().unwrap();
    let mut body = sample_body();
    body.models[0].architecture = "a".repeat(catalog::MAX_BODY_JSON_BYTES);
    assert_matches!(
        catalog::sign_catalog(&body, "ed25519:prod-1", &key_pair),
        Err(catalog::Error::BodyTooLarge(_))
    );
}

#[test]
fn effective_expiry_is_capped_by_key_validity() {
    let body = sample_body();
    let mut ctx = verify_ctx(1_000_000);
    ctx.key_valid_until_ms = 1_010_000;
    assert_eq!(catalog::effective_expiry_ms(&body, &ctx), 1_010_000);
}

#[test]
fn clock_skew_does_not_expand_signing_key_window_at_issuance() {
    let key = KeyPairHandle::generate().unwrap();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key).unwrap();
    let mut ctx = verify_ctx(body.issued_at_ms - 1);
    ctx.key_valid_until_ms = body.issued_at_ms;
    assert_eq!(catalog::verify_signed_catalog(&envelope, &key.public_key_bytes(), &ctx),
        Err(catalog::Error::KeyNotValid));
}

#[test]
fn clock_skew_does_not_make_a_future_signing_key_current() {
    let key = KeyPairHandle::generate().unwrap();
    let body = sample_body();
    let envelope = catalog::sign_catalog(&body, "ed25519:prod-1", &key).unwrap();
    let mut ctx = verify_ctx(body.issued_at_ms - 1);
    ctx.key_valid_from_ms = body.issued_at_ms;
    assert_eq!(catalog::verify_signed_catalog(&envelope, &key.public_key_bytes(), &ctx),
        Err(catalog::Error::KeyNotValid));
}
