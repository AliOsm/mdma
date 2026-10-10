//! Hugging Face ModernBERT encoder configuration.
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ModernBERT config: {0}")]
    Config(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RopeParams {
    pub rope_theta: Option<f64>,
    pub rope_type: Option<String>,
}

fn d_model_type() -> String { "modernbert".into() }
fn d_gelu() -> String { "gelu".into() }
fn d_norm_eps() -> f32 { 1e-5 }
fn d_local_attention() -> usize { 128 }
fn d_global_every() -> usize { 3 }
fn d_global_theta() -> f64 { 160000.0 }
fn d_local_theta() -> f64 { 10000.0 }
fn d_max_pos() -> usize { 8192 }

#[derive(Debug, Clone, Deserialize)]
pub struct EncoderConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    #[serde(default = "d_model_type")]
    pub model_type: String,
    #[serde(default = "d_norm_eps")]
    pub norm_eps: f32,
    #[serde(default)]
    pub norm_bias: bool,
    #[serde(default)]
    pub attention_bias: bool,
    #[serde(default)]
    pub mlp_bias: bool,
    #[serde(default = "d_gelu")]
    pub hidden_activation: String,
    #[serde(default = "d_local_attention")]
    pub local_attention: usize,
    #[serde(default = "d_global_every")]
    pub global_attn_every_n_layers: usize,
    #[serde(default = "d_global_theta")]
    pub global_rope_theta: f64,
    #[serde(default = "d_local_theta")]
    pub local_rope_theta: f64,
    #[serde(default = "d_max_pos")]
    pub max_position_embeddings: usize,
    #[serde(default)]
    pub layer_types: Option<Vec<String>>,
    #[serde(default)]
    pub rope_parameters: Option<HashMap<String, RopeParams>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Full,
    Sliding,
}

impl EncoderConfig {
    pub fn from_json(text: &str) -> Result<Self, Error> {
        let cfg: Self = serde_json::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub(super) fn validate(&self) -> Result<(), Error> {
        if self.model_type != "modernbert" {
            return Err(Error::Config(format!("unsupported encoder {:?}; expected modernbert", self.model_type)));
        }
        if self.hidden_activation != "gelu" {
            return Err(Error::Config(format!("unsupported encoder activation {:?}", self.hidden_activation)));
        }
        if self.num_attention_heads == 0 || !self.hidden_size.is_multiple_of(self.num_attention_heads) || !self.head_dim().is_multiple_of(2) {
            return Err(Error::Config("ModernBERT requires an even, integral attention head dimension".into()));
        }
        if let Some(types) = &self.layer_types
            && (types.len() != self.num_hidden_layers || types.iter().any(|t| t != "full_attention" && t != "sliding_attention"))
        {
            return Err(Error::Config("invalid ModernBERT layer_types".into()));
        }
        for (kind, params) in self.rope_parameters.iter().flatten() {
            if params.rope_type.as_deref().unwrap_or("default") != "default" {
                return Err(Error::Config(format!("only default (unscaled) ModernBERT RoPE is supported, {kind} is not")));
            }
        }
        Ok(())
    }

    pub fn head_dim(&self) -> usize { self.hidden_size / self.num_attention_heads }

    pub fn layer_kind(&self, index: usize) -> LayerKind {
        match &self.layer_types {
            Some(types) if types[index] == "sliding_attention" => LayerKind::Sliding,
            Some(_) => LayerKind::Full,
            None if index.is_multiple_of(self.global_attn_every_n_layers) => LayerKind::Full,
            None => LayerKind::Sliding,
        }
    }

    pub fn rope_theta(&self, kind: LayerKind) -> f32 {
        let (key, fallback) = match kind {
            LayerKind::Full => ("full_attention", self.global_rope_theta),
            LayerKind::Sliding => ("sliding_attention", self.local_rope_theta),
        };
        self.rope_parameters.as_ref().and_then(|p| p.get(key)).and_then(|p| p.rope_theta).unwrap_or(fallback) as f32
    }
}
