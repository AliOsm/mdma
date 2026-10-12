use rama::error::BoxError;
use std::env::VarError;

pub(super) fn required(env: &impl Fn(&str) -> Result<String, VarError>, name: &str) -> Result<String, BoxError> {
    let value = env(name).map_err(|e| format!("{name}: {e}"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}
