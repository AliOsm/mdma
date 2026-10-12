//! `server_id` validation: an operator-selected, lowercase ASCII DNS-form
//! name, per `doc/federation/identity-and-catalog-v1.md#1-server-identity-and-peer-configuration`.
//!
//! This is the stable identifier a peer configures for itself and for
//! every peer it trusts. It is deliberately not proof of DNS ownership;
//! validation here only enforces syntax.

pub const MAX_LENGTH: usize = 253;
pub const MAX_LABEL_LENGTH: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("server_id must not be empty")]
    Empty,
    #[error("server_id must be at most 253 bytes, got {0}")]
    TooLong(usize),
    #[error("server_id must be ASCII")]
    NotAscii,
    #[error("server_id must be lowercase")]
    NotLowercase,
    #[error("server_id must not be an IP literal")]
    IpLiteral,
    #[error("server_id must not contain a scheme")]
    HasScheme,
    #[error("server_id must not have a trailing dot")]
    TrailingDot,
    #[error("server_id must not contain a port")]
    HasPort,
    #[error("server_id must not contain a path, query or fragment")]
    InvalidChar(String),
    #[error("server_id must have at least two labels")]
    TooFewLabels,
    #[error("server_id label {0:?} is empty")]
    EmptyLabel(String),
    #[error("server_id label {0:?} is longer than 63 bytes")]
    LabelTooLong(String),
    #[error("server_id label {0:?} must start and end with a letter or digit")]
    LabelBoundary(String),
    #[error(
        "server_id label {0:?} contains a character other than ASCII letters, digits or hyphens"
    )]
    InvalidLabelChar(String),
}

/// Validates `id` against the v1 `server_id` grammar. Does not perform any
/// network lookup; this is purely syntactic.
pub fn validate(id: &str) -> Result<(), Error> {
    if id.is_empty() {
        return Err(Error::Empty);
    }
    if id.len() > MAX_LENGTH {
        return Err(Error::TooLong(id.len()));
    }
    if !id.is_ascii() {
        return Err(Error::NotAscii);
    }
    if id.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(Error::NotLowercase);
    }
    // IP literals (v4 or v6) are rejected outright, before the scheme/port
    // checks below, since IPv6 literals legitimately contain ':'.
    if id.parse::<std::net::IpAddr>().is_ok() {
        return Err(Error::IpLiteral);
    }
    if id.contains("://") {
        return Err(Error::HasScheme);
    }
    if id.ends_with('.') {
        return Err(Error::TrailingDot);
    }
    if id.contains(':') {
        return Err(Error::HasPort);
    }
    if id.contains('/') || id.contains('?') || id.contains('#') {
        return Err(Error::InvalidChar(id.to_string()));
    }
    let labels: Vec<&str> = id.split('.').collect();
    if labels.len() < 2 {
        return Err(Error::TooFewLabels);
    }
    for label in labels {
        validate_label(label)?;
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), Error> {
    if label.is_empty() {
        return Err(Error::EmptyLabel(label.to_string()));
    }
    if label.len() > MAX_LABEL_LENGTH {
        return Err(Error::LabelTooLong(label.to_string()));
    }
    let bytes = label.as_bytes();
    let is_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_alnum(bytes[0]) || !is_alnum(*bytes.last().expect("checked nonempty")) {
        return Err(Error::LabelBoundary(label.to_string()));
    }
    if !bytes.iter().all(|&b| is_alnum(b) || b == b'-') {
        return Err(Error::InvalidLabelChar(label.to_string()));
    }
    Ok(())
}
