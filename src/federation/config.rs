//! Local, explicitly configured trust. Parsing never enrolls a remote peer.
//! Paths are resolved by the operator; no URL, DNS or key discovery occurs.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rama::net::uri::Uri;
use rustls_pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{server_id, signing};

pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const MAX_PEERS: usize = 128;
pub const MAX_CREDENTIALS: usize = 16;
pub const MAX_SAFE_TIMESTAMP: u64 = 9_007_199_254_740_991;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("federation config exceeds its size limit")]
    Size,
    #[error("invalid federation TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid federation policy: {0}")]
    Policy(String),
}

fn require(ok: bool, message: &str) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err(Error::Policy(message.into()))
    }
}

/// Half-open finite credential window. Clock tolerance never expands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
}

impl Window {
    pub fn validate(&self) -> Result<(), Error> {
        require(
            self.valid_from_ms < self.valid_until_ms && self.valid_until_ms <= MAX_SAFE_TIMESTAMP,
            "credential windows must be finite, nonempty safe timestamps",
        )
    }

    pub fn contains(&self, now_ms: u64) -> bool {
        self.valid_from_ms <= now_ms && now_ms < self.valid_until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsRole {
    ClientAuth,
    ServerAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsPin {
    pub role: TlsRole,
    pub spki_sha256: String,
    #[serde(flatten)]
    pub window: Window,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SigningKey {
    pub key_id: String,
    /// Canonical URL-safe unpadded Base64 of exactly 32 Ed25519 bytes.
    pub public_key: String,
    #[serde(flatten)]
    pub window: Window,
}

/// Pin the complete root DER as well as its location. Replacing a PEM file
/// cannot silently publish a new issuer without a reviewed policy update.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaRoot {
    pub path: PathBuf,
    pub certificate_sha256: String,
}

impl SigningKey {
    pub fn bytes(&self) -> Result<[u8; 32], Error> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.public_key)
            .map_err(|_| Error::Policy("invalid signing public key encoding".into()))?;
        require(
            URL_SAFE_NO_PAD.encode(&bytes) == self.public_key,
            "noncanonical signing public key encoding",
        )?;
        bytes
            .try_into()
            .map_err(|_| Error::Policy("signing public key must be 32 bytes".into()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlPermissions {
    pub identify: bool,
    pub scan_capabilities: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Synthesize,
    Decide,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteExport {
    pub model: String,
    pub operation: Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteImport {
    pub model: String,
    pub artifact_id: String,
    pub profile_id: String,
    pub operation: Operation,
}

/// Independent control and workload budgets; none confer permissions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub control_per_minute: u32,
    pub requests_per_minute: u32,
    pub concurrency: u32,
    pub ledger_entries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            control_per_minute: 60,
            requests_per_minute: 60,
            concurrency: 1,
            ledger_entries: 1024,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), Error> {
        require(
            (1..=3600).contains(&self.control_per_minute)
                && (1..=60_000).contains(&self.requests_per_minute)
                && (1..=1024).contains(&self.concurrency)
                && (1..=65_536).contains(&self.ledger_entries)
                && self.concurrency <= self.ledger_entries,
            "peer limits outside bounded ranges",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub server_id: String,
    pub endpoint: String,
    pub tls_name: String,
    pub ca_roots: Vec<CaRoot>,
    #[serde(default)]
    pub tls_pins: Vec<TlsPin>,
    #[serde(default)]
    pub signing_keys: Vec<SigningKey>,
    #[serde(default)]
    pub control_permissions: ControlPermissions,
    #[serde(default)]
    pub catalog_exports: Vec<String>,
    #[serde(default)]
    pub execute_exports: Vec<ExecuteExport>,
    #[serde(default)]
    pub route_imports: Vec<RouteImport>,
    #[serde(default)]
    pub limits: Limits,
}

impl Peer {
    pub fn validate(&self) -> Result<(), Error> {
        server_id::validate(&self.server_id).map_err(|e| Error::Policy(e.to_string()))?;
        validate_endpoint(&self.endpoint)?;
        require(
            self.tls_name.len() <= 253 && !self.tls_name.ends_with('.'),
            "invalid tls_name",
        )?;
        ServerName::try_from(self.tls_name.as_str())
            .map_err(|_| Error::Policy("tls_name must be a DNS name or IP address".into()))?;
        require(
            !self.ca_roots.is_empty() && self.ca_roots.len() <= MAX_CREDENTIALS,
            "peer requires 1..16 CA root files",
        )?;
        let mut roots = BTreeSet::new();
        for root in &self.ca_roots {
            validate_digest(&root.certificate_sha256)?;
            require(
                !root.path.as_os_str().is_empty() && roots.insert(&root.certificate_sha256),
                "empty or duplicate CA root path",
            )?;
        }
        require(
            self.tls_pins.len() <= MAX_CREDENTIALS && self.signing_keys.len() <= MAX_CREDENTIALS,
            "too many peer credentials",
        )?;
        let mut pins = BTreeSet::new();
        for pin in &self.tls_pins {
            validate_digest(&pin.spki_sha256)?;
            pin.window.validate()?;
            require(
                pins.insert((pin.role as u8, &pin.spki_sha256)),
                "duplicate role/SPKI pin",
            )?;
        }
        let mut keys = BTreeSet::new();
        for key in &self.signing_keys {
            signing::validate_key_id(&key.key_id).map_err(|e| Error::Policy(e.to_string()))?;
            key.bytes()?;
            key.window.validate()?;
            require(keys.insert(&key.key_id), "duplicate signing key ID")?;
        }
        require(
            self.catalog_exports.len() <= 256
                && self.execute_exports.len() <= 256
                && self.route_imports.len() <= 256,
            "too many model grants",
        )?;
        let mut models = BTreeSet::new();
        for model in &self.catalog_exports {
            validate_model(model)?;
            require(models.insert(model), "duplicate catalog export")?;
        }
        models.clear();
        for export in &self.execute_exports {
            validate_model(&export.model)?;
            require(models.insert(&export.model), "duplicate execution export")?;
        }
        models.clear();
        for import in &self.route_imports {
            validate_model(&import.model)?;
            validate_digest(&import.artifact_id)?;
            validate_digest(&import.profile_id)?;
            require(models.insert(&import.model), "duplicate route import")?;
        }
        self.limits.validate()
    }

    #[must_use]
    pub fn without_grants(&self) -> Self {
        let mut peer = self.clone();
        peer.control_permissions = ControlPermissions::default();
        peer.catalog_exports.clear();
        peer.execute_exports.clear();
        peer.route_imports.clear();
        peer
    }
}

/// An unvalidated configuration. It cannot authenticate a peer by itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub server_id: String,
    pub trust_state: PathBuf,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl File {
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.len() > MAX_CONFIG_BYTES {
            return Err(Error::Size);
        }
        let file: Self = toml::from_str(input)?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<(), Error> {
        server_id::validate(&self.server_id).map_err(|e| Error::Policy(e.to_string()))?;
        require(
            !self.trust_state.as_os_str().is_empty(),
            "trust_state path is required",
        )?;
        require(self.peers.len() <= MAX_PEERS, "too many peers")?;
        let mut names = BTreeSet::new();
        for peer in &self.peers {
            peer.validate()?;
            require(
                peer.server_id != self.server_id,
                "peer identity equals local identity",
            )?;
            require(names.insert(&peer.server_id), "duplicate peer identity")?;
        }
        Ok(())
    }
}

pub fn validate_digest(value: &str) -> Result<(), Error> {
    let hex = value.strip_prefix("sha256:").unwrap_or("");
    require(
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "digest must be sha256: followed by 64 lowercase hexadecimal digits",
    )
}

pub fn validate_model(value: &str) -> Result<(), Error> {
    require(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid model name",
    )
}

fn validate_endpoint(value: &str) -> Result<(), Error> {
    require(
        value.len() <= 1024 && !value.contains(['?', '#', '@', '%', '\\']) && value.is_ascii(),
        "invalid endpoint origin",
    )?;
    let uri: Uri = value
        .parse()
        .map_err(|_| Error::Policy("invalid HTTPS origin".into()))?;
    require(
        uri.scheme_str() == Some("https")
            && uri.authority().is_some()
            && uri.port_u16().is_some_and(|p| p != 0)
            && uri.path_or_root() == "/",
        "endpoint must be an HTTPS origin with explicit nonzero port",
    )?;
    let host = uri
        .host_str()
        .ok_or_else(|| Error::Policy("endpoint requires hostname".into()))?;
    ServerName::try_from(host.trim_start_matches('[').trim_end_matches(']'))
        .map_err(|_| Error::Policy("invalid endpoint hostname".into()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
