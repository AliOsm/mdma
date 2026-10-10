//! Checkpoint tokenizer with padding and truncation handled by the prompt builder.
use super::{Error, Result};
use serde_json::Value;
use std::path::Path;
use tokenizers::pipeline::{EncodeOptions, Override, PipelineTokenizer};

pub struct Tokenizer {
    inner: PipelineTokenizer,
    pub cls_id: u32,
    pub sep_id: u32,
    pub pad_id: u32,
    pub mask_id: u32,
    pub mask_token: String,
}

#[cfg(test)]
pub(crate) mod tests;

fn special(config: &Value, name: &str) -> Option<String> {
    match config.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("content")?.as_str().map(str::to_string),
        _ => None,
    }
}

impl Tokenizer {
    pub fn open(dir: &Path) -> Result<Self> {
        Self::from_sources(
            &std::fs::read_to_string(dir.join("tokenizer.json"))?,
            &std::fs::read_to_string(dir.join("tokenizer_config.json"))?,
        )
    }

    pub(super) fn from_sources(tokenizer: &str, config: &str) -> Result<Self> {
        // v1 reads canonical 2.0 configs. Convert legacy checkpoints in memory,
        // leaving their files untouched and keeping token IDs unchanged.
        let canonical = tokenizers::canonicalize_str(tokenizer)
            .map_err(|e| Error::Tokenizer(e.to_string()))?;
        let inner =
            tokenizers::from_json(&canonical).map_err(|e| Error::Tokenizer(e.to_string()))?;
        let tokenizer_config: Value = serde_json::from_str(&canonical)?;
        let config: Value = serde_json::from_str(config)?;
        let id_of = |name: &str| -> Result<(String, u32)> {
            let token = special(&config, name)
                .ok_or_else(|| Error::Tokenizer(format!("missing a valid {name}")))?;
            let vocab = &tokenizer_config["model"]["vocab"];
            let id = inner
                .get_added_vocabulary()
                .token_to_id(&token)
                .or_else(|| {
                    vocab
                        .get(&token)
                        .and_then(Value::as_u64)
                        .and_then(|id| u32::try_from(id).ok())
                })
                .or_else(|| {
                    vocab
                        .as_array()?
                        .iter()
                        .position(|entry| entry[0].as_str() == Some(&token))
                        .and_then(|id| u32::try_from(id).ok())
                })
                .ok_or_else(|| Error::Tokenizer(format!("missing a valid {name}")))?;
            Ok((token, id))
        };
        let cls_id = id_of("cls_token")?.1;
        let sep_id = id_of("sep_token")?.1;
        let pad_id = id_of("pad_token")?.1;
        let (mask_token, mask_id) = id_of("mask_token")?;
        Ok(Self {
            inner,
            cls_id,
            sep_id,
            pad_id,
            mask_id,
            mask_token,
        })
    }

    /// Token ids without special tokens.
    pub fn encode(&self, text: &str) -> Result<Vec<u32>> {
        let options = EncodeOptions {
            padding: Override::Off,
            truncation: Override::Off,
            ..EncodeOptions::no_specials()
        };
        let mut ids = Vec::new();
        self.inner
            .encode_into(text, &options, &mut ids)
            .map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(ids.into_iter().map(u32::from).collect())
    }
}
