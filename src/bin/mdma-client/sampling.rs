use mdma::grpc::SynthesizeSamplerConfig;
use rama::error::BoxError;
use std::env::VarError;

pub(super) fn config(env: impl FnOnce(&str) -> Result<String, VarError>) -> Result<Option<SynthesizeSamplerConfig>, BoxError> {
    match env("MDMA_GREEDY") {
        Err(VarError::NotPresent) => Ok(None),
        Ok(value) if value == "0" => Ok(None),
        Ok(value) if value == "1" => Ok(Some(SynthesizeSamplerConfig {
            temperature: Some(0.0),
            top_p: Some(1.0),
            min_p: Some(0.0),
            repetition_penalty: Some(1.0),
            repetition_window: Some(0),
        })),
        Ok(_) => Err("MDMA_GREEDY must be 0 or 1".into()),
        Err(error) => Err(format!("MDMA_GREEDY: {error}").into()),
    }
}

#[cfg(test)]
#[path = "sampling/tests.rs"]
mod tests;
