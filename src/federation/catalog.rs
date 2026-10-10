//! Federation v1 catalog data model: the signed inventory snapshot a peer
//! may poll via `ScanCapabilities`. See
//! `doc/federation/identity-and-catalog-v1.md` sections 3-5.
//!
//! This module covers data shape, validation and envelope verification
//! only; it has no network, TLS or peer-trust-store code. Callers supply
//! the already-authenticated mTLS principal, the outstanding poll nonce
//! and the pinned signing key's validity window, all of which live outside
//! this module (in the not-yet-implemented peer trust store).

use crate::federation::canonical_json::{self, Value};
use crate::federation::server_id;
use crate::federation::signing::{self, KeyPairHandle};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::collections::{BTreeMap, HashSet};

pub const CATALOG_TYPE: &str = "mdma.catalog.v1";
/// A catalog's lifetime (`expires_at_ms - issued_at_ms`) must be positive
/// and at most this many milliseconds.
pub const MAX_CATALOG_LIFETIME_MS: u64 = 60_000;
/// Reject `issued_at_ms` more than this far in the future.
pub const MAX_ISSUED_AT_SKEW_MS: u64 = 30_000;
pub const MAX_DESCRIPTORS: usize = 256;
pub const MAX_BODY_JSON_BYTES: usize = 256 * 1024;
pub const MAX_JSON_DEPTH: usize = 16;
pub const MAX_MODEL_NAME_BYTES: usize = 128;
pub const MAX_MAX_INPUT_BYTES: u32 = 65536;
pub const MAX_DECIDE_QUESTIONS: u32 = 128;
pub const MAX_DECIDE_RESULT_BYTES: u32 = 524288;
pub const NONCE_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Serving,
    Degraded,
    Critical,
    Draining,
}

impl ServerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Degraded => "degraded",
            Self::Critical => "critical",
            Self::Draining => "draining",
        }
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "serving" => Ok(Self::Serving),
            "degraded" => Ok(Self::Degraded),
            "critical" => Ok(Self::Critical),
            "draining" => Ok(Self::Draining),
            _ => Err(Error::InvalidField("server_state")),
        }
    }

    /// `critical` and `draining` catalogs MUST mark every model unready.
    pub fn forces_unready(&self) -> bool {
        matches!(self, Self::Critical | Self::Draining)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gguf,
    Laya,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gguf => "gguf",
            Self::Laya => "laya",
        }
    }

    /// `synthesize` for GGUF, `decide` for Laya; never `upstream`.
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Gguf => "synthesize",
            Self::Laya => "decide",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limits {
    Synthesize { n_ctx: u32, max_output_tokens: u32 },
    Decide { max_questions: u32, max_result_bytes: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("missing field {0}")]
    MissingField(&'static str),
    #[error("unexpected field {0:?}")]
    UnknownField(String),
    #[error("invalid value for field {0}")]
    InvalidField(&'static str),
    #[error("invalid server_id: {0}")]
    ServerId(server_id::Error),
    #[error("invalid key id: {0}")]
    KeyId(signing::KeyIdError),
    #[error("duplicate exported model name {0:?}")]
    DuplicateModel(String),
    #[error("too many model descriptors: {0} (max 256)")]
    TooManyModels(usize),
    #[error("canonical JSON error: {0}")]
    Json(canonical_json::Error),
    #[error("body_json is {0} bytes (max 262144)")]
    BodyTooLarge(usize),
    #[error("body_json is not the canonical encoding of its parsed value")]
    NotCanonical,
    #[error("signature verification failed")]
    BadSignature,
    #[error("catalog type must be \"mdma.catalog.v1\"")]
    WrongType,
    #[error("origin {claimed:?} does not match authenticated peer {authenticated:?}")]
    OriginMismatch { claimed: String, authenticated: String },
    #[error("destination {0:?} does not match local identity")]
    DestinationMismatch(String),
    #[error("nonce does not match the outstanding poll challenge")]
    NonceMismatch,
    #[error("issued_at_ms is too far in the future")]
    IssuedInFuture,
    #[error("catalog lifetime must be positive and at most 60000ms")]
    BadLifetime,
    #[error("catalog is already expired")]
    Expired,
    #[error("signing key is not valid for this catalog's issued_at/now")]
    KeyNotValid,
    #[error("critical/draining catalogs must mark every model unready")]
    ReadyWhileNotAccepting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDescriptor {
    pub model: String,
    pub kind: Kind,
    pub artifact_id: String,
    pub profile_id: String,
    pub architecture: String,
    pub backend: String,
    pub ready: bool,
    pub max_input_bytes: u32,
    pub limits: Limits,
}

impl ModelDescriptor {
    pub fn validate(&self) -> Result<(), Error> {
        if self.model.is_empty() || self.model.len() > MAX_MODEL_NAME_BYTES {
            return Err(Error::InvalidField("model"));
        }
        if !self.model.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
            return Err(Error::InvalidField("model"));
        }
        validate_digest(&self.artifact_id).map_err(|()| Error::InvalidField("artifact_id"))?;
        validate_digest(&self.profile_id).map_err(|()| Error::InvalidField("profile_id"))?;
        if self.architecture.is_empty() {
            return Err(Error::InvalidField("architecture"));
        }
        if self.backend.is_empty() {
            return Err(Error::InvalidField("backend"));
        }
        if self.max_input_bytes == 0 || self.max_input_bytes > MAX_MAX_INPUT_BYTES {
            return Err(Error::InvalidField("max_input_bytes"));
        }
        match (self.kind, &self.limits) {
            (Kind::Gguf, Limits::Synthesize { n_ctx, max_output_tokens }) => {
                if *n_ctx == 0 || *max_output_tokens == 0 || *max_output_tokens > *n_ctx {
                    return Err(Error::InvalidField("limits"));
                }
            }
            (Kind::Laya, Limits::Decide { max_questions, max_result_bytes }) => {
                if *max_questions == 0 || *max_questions > MAX_DECIDE_QUESTIONS {
                    return Err(Error::InvalidField("limits"));
                }
                if *max_result_bytes == 0 || *max_result_bytes > MAX_DECIDE_RESULT_BYTES {
                    return Err(Error::InvalidField("limits"));
                }
            }
            _ => return Err(Error::InvalidField("limits")),
        }
        Ok(())
    }

    pub(crate) fn to_canonical(&self) -> Value {
        let limits = match self.limits {
            Limits::Synthesize { n_ctx, max_output_tokens } => BTreeMap::from([
                ("n_ctx".to_string(), Value::Integer(i64::from(n_ctx))),
                ("max_output_tokens".to_string(), Value::Integer(i64::from(max_output_tokens))),
            ]),
            Limits::Decide { max_questions, max_result_bytes } => BTreeMap::from([
                ("max_questions".to_string(), Value::Integer(i64::from(max_questions))),
                ("max_result_bytes".to_string(), Value::Integer(i64::from(max_result_bytes))),
            ]),
        };
        let fields: Vec<(String, Value)> = vec![
            ("model".into(), Value::String(self.model.clone())),
            ("kind".into(), Value::String(self.kind.as_str().into())),
            ("artifact_id".into(), Value::String(self.artifact_id.clone())),
            ("profile_id".into(), Value::String(self.profile_id.clone())),
            ("architecture".into(), Value::String(self.architecture.clone())),
            ("backend".into(), Value::String(self.backend.clone())),
            ("operations".into(), Value::Array(vec![Value::String(self.kind.operation().into())])),
            ("ready".into(), Value::Bool(self.ready)),
            ("max_input_bytes".into(), Value::Integer(i64::from(self.max_input_bytes))),
            ("limits".into(), Value::Object(limits)),
        ];
        Value::object(fields).expect("static field set has no duplicate keys")
    }

    pub(crate) fn from_canonical(value: &Value) -> Result<Self, Error> {
        let obj = value.as_object().ok_or(Error::InvalidField("model descriptor"))?;
        reject_unknown_fields(
            obj,
            &[
                "model",
                "kind",
                "artifact_id",
                "profile_id",
                "architecture",
                "backend",
                "operations",
                "ready",
                "max_input_bytes",
                "limits",
            ],
        )?;
        let model = field_str(obj, "model")?;
        let kind = match field_str(obj, "kind")?.as_str() {
            "gguf" => Kind::Gguf,
            "laya" => Kind::Laya,
            _ => return Err(Error::InvalidField("kind")),
        };
        let artifact_id = field_str(obj, "artifact_id")?;
        let profile_id = field_str(obj, "profile_id")?;
        let architecture = field_str(obj, "architecture")?;
        let backend = field_str(obj, "backend")?;
        let operations = obj.get("operations").and_then(Value::as_array).ok_or(Error::MissingField("operations"))?;
        if operations.len() != 1 || operations[0].as_str() != Some(kind.operation()) {
            return Err(Error::InvalidField("operations"));
        }
        let ready = obj.get("ready").and_then(Value::as_bool).ok_or(Error::MissingField("ready"))?;
        let max_input_bytes = field_u32(obj, "max_input_bytes")?;
        let limits_obj = obj.get("limits").and_then(Value::as_object).ok_or(Error::MissingField("limits"))?;
        let limits = match kind {
            Kind::Gguf => {
                reject_unknown_fields(limits_obj, &["n_ctx", "max_output_tokens"])?;
                Limits::Synthesize { n_ctx: field_u32(limits_obj, "n_ctx")?, max_output_tokens: field_u32(limits_obj, "max_output_tokens")? }
            }
            Kind::Laya => {
                reject_unknown_fields(limits_obj, &["max_questions", "max_result_bytes"])?;
                Limits::Decide {
                    max_questions: field_u32(limits_obj, "max_questions")?,
                    max_result_bytes: field_u32(limits_obj, "max_result_bytes")?,
                }
            }
        };
        let descriptor = Self { model, kind, artifact_id, profile_id, architecture, backend, ready, max_input_bytes, limits };
        descriptor.validate()?;
        Ok(descriptor)
    }
}

fn validate_digest(s: &str) -> Result<(), ()> {
    let hex = s.strip_prefix("sha256:").ok_or(())?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(());
    }
    Ok(())
}

fn reject_unknown_fields(obj: &BTreeMap<String, Value>, expected: &[&str]) -> Result<(), Error> {
    for key in obj.keys() {
        if !expected.contains(&key.as_str()) {
            return Err(Error::UnknownField(key.clone()));
        }
    }
    Ok(())
}

fn field_str(obj: &BTreeMap<String, Value>, name: &'static str) -> Result<String, Error> {
    obj.get(name).and_then(Value::as_str).map(str::to_string).ok_or(Error::MissingField(name))
}

fn field_u32(obj: &BTreeMap<String, Value>, name: &'static str) -> Result<u32, Error> {
    let n = obj.get(name).and_then(Value::as_integer).ok_or(Error::MissingField(name))?;
    u32::try_from(n).map_err(|_| Error::InvalidField(name))
}

fn field_u64(obj: &BTreeMap<String, Value>, name: &'static str) -> Result<u64, Error> {
    let n = obj.get(name).and_then(Value::as_integer).ok_or(Error::MissingField(name))?;
    u64::try_from(n).map_err(|_| Error::InvalidField(name))
}

/// The signed catalog body: "complete visible local export set for this
/// recipient". See `identity-and-catalog-v1.md#3-catalog-exchange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogBody {
    pub origin: String,
    pub destination: String,
    /// The raw 32-byte poll challenge (wire form is URL-safe unpadded
    /// Base64, handled by `to_canonical`/`from_canonical`).
    pub nonce: Vec<u8>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub server_state: ServerState,
    pub models: Vec<ModelDescriptor>,
}

impl CatalogBody {
    pub fn validate(&self) -> Result<(), Error> {
        server_id::validate(&self.origin).map_err(Error::ServerId)?;
        server_id::validate(&self.destination).map_err(Error::ServerId)?;
        if self.nonce.len() != NONCE_BYTES {
            return Err(Error::InvalidField("nonce"));
        }
        if self.expires_at_ms <= self.issued_at_ms || self.expires_at_ms - self.issued_at_ms > MAX_CATALOG_LIFETIME_MS {
            return Err(Error::BadLifetime);
        }
        if self.models.len() > MAX_DESCRIPTORS {
            return Err(Error::TooManyModels(self.models.len()));
        }
        let mut seen = HashSet::new();
        for model in &self.models {
            model.validate()?;
            if !seen.insert(model.model.clone()) {
                return Err(Error::DuplicateModel(model.model.clone()));
            }
            if self.server_state.forces_unready() && model.ready {
                return Err(Error::ReadyWhileNotAccepting);
            }
        }
        Ok(())
    }

    pub fn to_canonical(&self) -> Value {
        let fields: Vec<(String, Value)> = vec![
            ("type".into(), Value::String(CATALOG_TYPE.into())),
            ("origin".into(), Value::String(self.origin.clone())),
            ("destination".into(), Value::String(self.destination.clone())),
            ("nonce".into(), Value::String(URL_SAFE_NO_PAD.encode(&self.nonce))),
            ("issued_at_ms".into(), Value::Integer(self.issued_at_ms.cast_signed())),
            ("expires_at_ms".into(), Value::Integer(self.expires_at_ms.cast_signed())),
            ("server_state".into(), Value::String(self.server_state.as_str().into())),
            ("models".into(), Value::Array(self.models.iter().map(ModelDescriptor::to_canonical).collect())),
        ];
        Value::object(fields).expect("static field set has no duplicate keys")
    }

    pub fn from_canonical(value: &Value) -> Result<Self, Error> {
        let obj = value.as_object().ok_or(Error::InvalidField("body"))?;
        reject_unknown_fields(obj, &["type", "origin", "destination", "nonce", "issued_at_ms", "expires_at_ms", "server_state", "models"])?;
        if field_str(obj, "type")? != CATALOG_TYPE {
            return Err(Error::WrongType);
        }
        let origin = field_str(obj, "origin")?;
        let destination = field_str(obj, "destination")?;
        let nonce = URL_SAFE_NO_PAD.decode(field_str(obj, "nonce")?).map_err(|_| Error::InvalidField("nonce"))?;
        let issued_at_ms = field_u64(obj, "issued_at_ms")?;
        let expires_at_ms = field_u64(obj, "expires_at_ms")?;
        let server_state = ServerState::parse(&field_str(obj, "server_state")?)?;
        let models_v = obj.get("models").and_then(Value::as_array).ok_or(Error::MissingField("models"))?;
        let models = models_v.iter().map(ModelDescriptor::from_canonical).collect::<Result<Vec<_>, _>>()?;
        let body = Self { origin, destination, nonce, issued_at_ms, expires_at_ms, server_state, models };
        body.validate()?;
        Ok(body)
    }
}

/// The wire envelope: a pinned key ID, the canonical body bytes and a
/// 64-byte Ed25519 signature over them.
#[derive(Debug, Clone)]
pub struct SignedCatalogEnvelope {
    pub key_id: String,
    pub body_json: Vec<u8>,
    pub signature: [u8; 64],
}

/// Everything a receiver must already know to verify one catalog response:
/// the authenticated mTLS principal, local identity, the outstanding poll
/// nonce, the current time and the pinned key's local validity window.
/// None of this is derived by this module; runtime callers obtain it from
/// the authenticated TLS binding, current trust store and outstanding poll.
pub struct VerifyContext<'a> {
    pub now_ms: u64,
    pub expected_origin: &'a str,
    pub expected_destination: &'a str,
    pub expected_nonce: &'a [u8],
    pub key_valid_from_ms: u64,
    pub key_valid_until_ms: u64,
}

/// Signs `body` under `key_id`, producing the envelope a `ScanCapabilities`
/// response carries.
pub fn sign_catalog(body: &CatalogBody, key_id: &str, key_pair: &KeyPairHandle) -> Result<SignedCatalogEnvelope, Error> {
    body.validate()?;
    signing::validate_key_id(key_id).map_err(Error::KeyId)?;
    let value = body.to_canonical();
    let body_json = value.encode().into_bytes();
    if body_json.len() > MAX_BODY_JSON_BYTES {
        return Err(Error::BodyTooLarge(body_json.len()));
    }
    let signature = signing::sign(key_pair, &value, key_id);
    Ok(SignedCatalogEnvelope { key_id: key_id.to_string(), body_json, signature })
}

/// Verifies `envelope` end to end: size bound, canonical re-encoding,
/// signature, origin/destination/nonce match, timestamp and key-validity
/// windows, and body/descriptor schema. Returns the verified body on
/// success.
pub fn verify_signed_catalog(envelope: &SignedCatalogEnvelope, public_key: &[u8; 32], ctx: &VerifyContext) -> Result<CatalogBody, Error> {
    if envelope.body_json.len() > MAX_BODY_JSON_BYTES {
        return Err(Error::BodyTooLarge(envelope.body_json.len()));
    }
    signing::validate_key_id(&envelope.key_id).map_err(Error::KeyId)?;
    let value = canonical_json::parse(&envelope.body_json, MAX_JSON_DEPTH).map_err(Error::Json)?;
    if value.encode().into_bytes() != envelope.body_json {
        return Err(Error::NotCanonical);
    }
    signing::verify(public_key, &value, &envelope.key_id, &envelope.signature).map_err(|_| Error::BadSignature)?;
    let body = CatalogBody::from_canonical(&value)?;
    if body.origin != ctx.expected_origin {
        return Err(Error::OriginMismatch { claimed: body.origin, authenticated: ctx.expected_origin.to_string() });
    }
    if body.destination != ctx.expected_destination {
        return Err(Error::DestinationMismatch(body.destination));
    }
    if body.nonce != ctx.expected_nonce {
        return Err(Error::NonceMismatch);
    }
    if body.issued_at_ms > ctx.now_ms.saturating_add(MAX_ISSUED_AT_SKEW_MS) {
        return Err(Error::IssuedInFuture);
    }
    if body.expires_at_ms <= ctx.now_ms {
        return Err(Error::Expired);
    }
    if ctx.now_ms < ctx.key_valid_from_ms
        || ctx.now_ms >= ctx.key_valid_until_ms
        || body.issued_at_ms < ctx.key_valid_from_ms
        || body.issued_at_ms >= ctx.key_valid_until_ms
    {
        return Err(Error::KeyNotValid);
    }
    Ok(body)
}

/// The capped effective expiry: the body's own `expires_at_ms`, further
/// capped by the 60-second local monotonic lifetime and by the signing
/// key's local validity window. Subsequent wall-clock adjustments cannot
/// extend it.
pub fn effective_expiry_ms(body: &CatalogBody, ctx: &VerifyContext) -> u64 {
    body.expires_at_ms.min(ctx.now_ms.saturating_add(MAX_CATALOG_LIFETIME_MS)).min(ctx.key_valid_until_ms)
}
