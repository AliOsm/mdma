//! Compatibility policy, not authentication: all identities are self-reported.
use super::proto;
use rama::http::grpc::{Code, Status};
use serde_json::{Value, json};

pub(super) const PROTOCOL: u32 = 2;
pub(super) const TXT_PROTOCOL: &str = "resources-2";

pub(super) fn local() -> proto::NodeIdentity {
    proto::NodeIdentity {
        mdma_version: env!("CARGO_PKG_VERSION").into(),
        build_id: env!("MDMA_BUILD_ID").into(),
        protocol_version: PROTOCOL,
    }
}

pub(super) fn json_identity(identity: &proto::NodeIdentity) -> Value {
    json!({"mdma_version": identity.mdma_version, "build_id": identity.build_id,
        "protocol_version": identity.protocol_version})
}

#[derive(Clone)]
pub(super) struct Policy {
    minimum: semver::Version,
    required_build: Option<String>,
}

impl Policy {
    pub(super) fn new(minimum: Option<&str>, required_build: Option<&str>) -> Result<Self, String> {
        let minimum = semver::Version::parse(minimum.unwrap_or(env!("CARGO_PKG_VERSION")))
            .map_err(|error| format!("invalid --min-peer-version: {error}"))?;
        if required_build.is_some_and(|build| build.is_empty() || build.len() > 128
            || !build.bytes().all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))) {
            return Err("invalid --require-peer-build token".into());
        }
        Ok(Self { minimum, required_build: required_build.map(str::to_owned) })
    }

    pub(super) fn check(&self, identity: Option<&proto::NodeIdentity>) -> Result<(), String> {
        let identity = identity.ok_or("missing peer identity (legacy node)")?;
        if identity.protocol_version != PROTOCOL {
            return Err(format!("protocol {} rejected; require {}", identity.protocol_version, PROTOCOL));
        }
        let version = semver::Version::parse(&identity.mdma_version)
            .map_err(|_| "missing or invalid MDMA version".to_owned())?;
        if version.cmp_precedence(&self.minimum).is_lt() {
            return Err(format!("MDMA {version} outdated; require >= {}", self.minimum));
        }
        if identity.build_id.is_empty() || identity.build_id.len() > 128
            || !identity.build_id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c)) {
            return Err("missing or invalid build ID".into());
        }
        if let Some(build) = self.required_build.as_ref().filter(|build| *build != &identity.build_id) {
            return Err(format!("build {} rejected; require {build}", identity.build_id));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum QueryError {
    Unreachable(String),
    Rejected { reason: String, identity: Option<Value> },
}

impl From<String> for QueryError {
    fn from(error: String) -> Self { Self::Unreachable(error) }
}
impl From<&str> for QueryError {
    fn from(error: &str) -> Self { Self::Unreachable(error.into()) }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) => fmt.write_str(error),
            Self::Rejected { reason, .. } => fmt.write_str(reason),
        }
    }
}

impl QueryError {
    pub(super) fn status(&self) -> &'static str {
        match self { Self::Unreachable(_) => "unreachable", Self::Rejected { .. } => "rejected" }
    }
    #[expect(clippy::needless_pass_by_value, reason = "used as a `map_err` adapter")]
    pub(super) fn from_status(status: Status) -> Self {
        match status.code() {
            Code::FailedPrecondition | Code::Unimplemented => Self::Rejected {
                reason: format!("identity/compatibility rejected: {status}"), identity: None,
            },
            _ => Self::Unreachable(status.to_string()),
        }
    }
}

#[cfg(test)]
#[path = "version/tests.rs"]
mod tests;
