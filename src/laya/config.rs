// Laya agent configuration and checkpoint validation.
use super::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub use crate::model::arch::modernbert::{EncoderConfig, LayerKind, RopeParams};

fn d_head_layers() -> usize {
    2
}
fn d_max_len() -> usize {
    512
}
fn d_head_max_len() -> usize {
    192
}
fn d_temperature() -> [f32; 3] {
    [1.0; 3]
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub encoder: String,
    #[serde(default = "d_head_layers")]
    pub head_layers: usize,
    #[serde(default = "d_max_len")]
    pub max_len: usize,
    #[serde(default = "d_head_max_len")]
    pub head_max_len: usize,
    #[serde(default)]
    pub act_costs: HashMap<String, f64>,
    #[serde(default = "d_temperature")]
    pub temperature: [f32; 3],
    #[serde(default)]
    pub temperature_by_options: HashMap<String, f32>,
}

impl AgentConfig {
    pub fn from_json(text: &str) -> Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(text)?;
        if raw.get("encoder").is_none() || raw.get("head_layers").is_none() {
            return Err(Error::Config(
                "Laya config must specify encoder and head_layers".into(),
            ));
        }
        let cfg: Self = serde_json::from_value(raw)?;
        let temps = cfg
            .temperature
            .iter()
            .chain(cfg.temperature_by_options.values());
        if temps.into_iter().any(|t| !t.is_finite() || *t <= 0.0) {
            return Err(Error::Config(
                "calibration temperatures must be finite and positive".into(),
            ));
        }
        Ok(cfg)
    }

    pub fn n_act(&self) -> usize {
        self.act_costs.len() + 1
    }
}

pub struct Checkpoint {
    pub encoder: EncoderConfig,
    pub agent: AgentConfig,
}

impl Checkpoint {
    /// Reads and cross-validates both configs of a checkpoint directory.
    pub fn open(dir: &Path) -> Result<Self> {
        for name in [
            "model.safetensors",
            "rl_agent_config.json",
            "encoder/config.json",
        ] {
            if !dir.join(name).is_file() {
                return Err(Error::Checkpoint(format!(
                    "not a complete Laya checkpoint: {} is missing",
                    dir.join(name).display()
                )));
            }
        }
        Self::from_sources(
            &std::fs::read_to_string(dir.join("rl_agent_config.json"))?,
            &std::fs::read_to_string(dir.join("encoder/config.json"))?,
        )
    }

    pub(super) fn from_sources(agent: &str, encoder: &str) -> Result<Self> {
        let agent = AgentConfig::from_json(agent)?;
        let encoder = EncoderConfig::from_json(encoder)?;
        if !(4 < agent.head_max_len
            && agent.head_max_len < agent.max_len
            && agent.max_len <= encoder.max_position_embeddings)
        {
            return Err(Error::Config(
                "expected 4 < head_max_len < max_len <= max_position_embeddings".into(),
            ));
        }
        Ok(Self { encoder, agent })
    }
}
