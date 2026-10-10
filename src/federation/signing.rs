//! Federation catalog signing: Ed25519 over a fixed preimage, independent
//! of TLS key material. See
//! `doc/federation/identity-and-catalog-v1.md#2-negotiation-and-key-lifecycle`
//! and `#3-catalog-exchange`.

use crate::federation::canonical_json::Value;
use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use std::collections::BTreeMap;

/// The exact byte prefix of the catalog signature preimage, ending in one
/// LF byte (not a backslash followed by `n`).
pub const PREIMAGE_PREFIX: &str = "mdma.federation.catalog.v1\n";
pub const MAX_LABEL_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyIdError {
    #[error("key id must start with \"ed25519:\"")]
    MissingPrefix,
    #[error("key id label must be 1..=32 bytes")]
    BadLabelLength,
    #[error("key id label must be lowercase letters, digits or hyphens")]
    BadLabelChar,
}

/// Validates the `ed25519:<label>` key ID form: a 1..32 byte label of
/// lowercase letters, digits or hyphens.
pub fn validate_key_id(id: &str) -> Result<(), KeyIdError> {
    let label = id
        .strip_prefix("ed25519:")
        .ok_or(KeyIdError::MissingPrefix)?;
    if label.is_empty() || label.len() > MAX_LABEL_LEN {
        return Err(KeyIdError::BadLabelLength);
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(KeyIdError::BadLabelChar);
    }
    Ok(())
}

/// Builds the exact signature preimage:
/// `UTF8("mdma.federation.catalog.v1\n") || CanonicalJSON({"body": B, "key_id": K})`.
/// The signature itself is not part of this preimage.
pub fn preimage(body: &Value, key_id: &str) -> Vec<u8> {
    let mut wrapper = BTreeMap::new();
    wrapper.insert("body".to_string(), body.clone());
    wrapper.insert("key_id".to_string(), Value::String(key_id.to_string()));
    let encoded = Value::Object(wrapper).encode();
    let mut out = Vec::with_capacity(PREIMAGE_PREFIX.len() + encoded.len());
    out.extend_from_slice(PREIMAGE_PREFIX.as_bytes());
    out.extend_from_slice(encoded.as_bytes());
    out
}

/// An owned Ed25519 key pair used to sign outgoing catalogs. Wraps
/// `aws-lc-rs` so callers never need that crate's types directly; this is
/// deliberately not the TLS key pair.
pub struct KeyPairHandle(Ed25519KeyPair);

impl KeyPairHandle {
    /// Load the explicitly selected local catalog key, never TLS key material.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self, &'static str> {
        Ed25519KeyPair::from_pkcs8(bytes)
            .map(Self)
            .map_err(|_| "invalid Ed25519 PKCS#8 key")
    }

    /// Export for private operator storage; these bytes must never be enrolled
    /// with peers or included in a public enrollment bundle.
    pub fn to_pkcs8(&self) -> Result<Vec<u8>, &'static str> {
        self.0
            .to_pkcs8()
            .map(|document| document.as_ref().to_vec())
            .map_err(|_| "Ed25519 key export failed")
    }

    pub fn generate() -> Result<Self, &'static str> {
        Ed25519KeyPair::generate()
            .map(Self)
            .map_err(|_| "key generation failed")
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.0.public_key().as_ref());
        out
    }
}

/// Signs `body` under `key_id`, returning the 64-byte Ed25519 signature.
pub fn sign(key_pair: &KeyPairHandle, body: &Value, key_id: &str) -> [u8; 64] {
    let msg = preimage(body, key_id);
    let sig = key_pair.0.sign(&msg);
    let mut out = [0u8; 64];
    out.copy_from_slice(sig.as_ref());
    out
}

/// Verifies a 64-byte Ed25519 `signature` of `body` under `key_id` against
/// `public_key`. Callers are responsible for checking that `public_key` is
/// the pinned key for the claimed signer and that its validity window
/// covers the relevant timestamps; this function only checks the
/// cryptographic signature.
pub fn verify(
    public_key: &[u8; 32],
    body: &Value,
    key_id: &str,
    signature: &[u8; 64],
) -> Result<(), &'static str> {
    let msg = preimage(body, key_id);
    UnparsedPublicKey::new(&ED25519, public_key.as_slice())
        .verify(&msg, signature.as_slice())
        .map_err(|_| "signature verification failed")
}
