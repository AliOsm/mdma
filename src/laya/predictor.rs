//! Checkpoint loading and `{"model", "answers", "usage"}` predictions.
use super::calibrate::{answer, clamp_temperature, softmax, temp_bucket};
use super::config::Checkpoint;
use super::model::DecisionModel;
use super::prompt::{Item, Question, build_sequence, collate_iter};
use super::tokenizer::Tokenizer;
use super::weights::Weights;
use super::{Error, Result};
use crate::backend::Backend;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;

pub struct Predictor<B: Backend> {
    pub(super) backend: B,
    pub(super) model: DecisionModel<B>,
    tokenizer: Tokenizer,
    temperature: [f32; 3],
    temperature_by_options: HashMap<String, f32>,
    batch_size: usize,
}

impl<B: Backend> Predictor<B> {
    pub fn load(backend: B, dir: &Path) -> Result<Self> {
        let Checkpoint { encoder, agent } = Checkpoint::open(dir)?;
        let tokenizer = Tokenizer::open(&dir.join("tokenizer"))?;
        let bytes = std::fs::read(dir.join("model.safetensors"))?;
        Self::from_sources(backend, encoder, agent, tokenizer, &bytes)
    }

    pub(crate) fn load_frozen(
        backend: B,
        dir: &Path,
    ) -> Result<(Self, crate::artifact::ArtifactId, u64)> {
        let sources = super::sources::Sources::capture(dir)?;
        let Checkpoint { encoder, agent } =
            Checkpoint::from_sources(sources.agent()?, sources.encoder()?)?;
        let tokenizer = Tokenizer::from_sources(sources.tokenizer()?, sources.tokenizer_config()?)?;
        let model = Self::from_sources(backend, encoder, agent, tokenizer, sources.weights())?;
        Ok((model, sources.id(), sources.weights().len() as u64))
    }

    fn from_sources(
        backend: B,
        encoder: super::config::EncoderConfig,
        agent: super::config::AgentConfig,
        tokenizer: Tokenizer,
        bytes: &[u8],
    ) -> Result<Self> {
        let weights = Weights::parse(bytes)?;
        let temperature = agent.temperature.map(clamp_temperature);
        let temperature_by_options = agent
            .temperature_by_options
            .iter()
            .map(|(k, &v)| (k.clone(), clamp_temperature(v)))
            .collect();
        let model = DecisionModel::load(&backend, &weights, encoder, agent)?;
        Ok(Self {
            backend,
            model,
            tokenizer,
            temperature,
            temperature_by_options,
            batch_size: 16,
        })
    }

    /// The encoder's `model_type`, e.g. "modernbert".
    pub fn architecture(&self) -> &str {
        &self.model.encoder_config().model_type
    }

    /// Tokens per question after truncation.
    pub fn max_len(&self) -> usize {
        self.model.agent_config().max_len
    }

    /// Questions per forward pass; larger requests are chunked.
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    pub fn temperature_for(&self, kind: super::prompt::Kind, k: usize) -> f32 {
        self.temperature_by_options
            .get(&temp_bucket(kind, k))
            .copied()
            .unwrap_or_else(|| self.temperature[kind.index() as usize])
    }

    /// Token ids and option markers per question.
    pub fn prepare(
        &self,
        state: &Value,
        questions: &Map<String, Value>,
    ) -> Result<Vec<(Question, Item)>> {
        self.prepare_checked(state, questions, || true)
            .map(|prepared| prepared.expect("unconditional preparation"))
    }

    /// No device allocation. Check eligibility between bounded question
    /// tokenizations, not only after all questions have been processed.
    pub(crate) fn prepare_checked(
        &self,
        state: &Value,
        questions: &Map<String, Value>,
        eligible: impl Fn() -> bool,
    ) -> Result<Option<Vec<(Question, Item)>>> {
        let agent = self.model.agent_config();
        let mut out = Vec::with_capacity(questions.len());
        for (qid, def) in questions {
            if !eligible() {
                return Ok(None);
            }
            let q = Question::from_json(def)?;
            let seq = build_sequence(
                &self.tokenizer,
                state,
                &q,
                agent.max_len,
                agent.head_max_len,
            )?;
            if seq.markers.len() != q.options.len() {
                return Err(Error::Question(format!(
                    "question {qid:?} has too many options for the token budget"
                )));
            }
            let item = Item {
                ids: seq.ids,
                markers: seq.markers,
                kind: q.kind,
            };
            out.push((q, item));
        }
        if !eligible() {
            return Ok(None);
        }
        Ok(Some(out))
    }

    pub fn predict(&self, state: &Value, questions: &Map<String, Value>) -> Result<Value> {
        let prepared = self.prepare(state, questions)?;
        let ids: Vec<String> = questions.keys().cloned().collect();
        self.predict_prepared(&prepared, &ids, || true)
            .map(|value| value.expect("unconditional prediction"))
    }

    /// Peak logical tensor workspace. Weight, tokenizer/JSON and private or
    /// transient backend allocations are outside this reservation.
    pub(crate) fn prepared_workspace_bytes(&self, prepared: &[(Question, Item)]) -> Result<usize> {
        let mut peak = 0;
        for chunk in prepared.chunks(self.batch_size) {
            let seq_len = chunk
                .iter()
                .map(|(_, item)| item.ids.len())
                .max()
                .unwrap_or(0);
            let kmax = chunk
                .iter()
                .map(|(_, item)| item.markers.len())
                .max()
                .unwrap_or(0)
                .max(2);
            peak = peak.max(self.model.workspace_bytes(chunk.len(), seq_len, kmax)?);
        }
        Ok(peak)
    }

    pub(crate) fn predict_prepared(
        &self,
        prepared: &[(Question, Item)],
        ids: &[String],
        eligible: impl Fn() -> bool,
    ) -> Result<Option<Value>> {
        if ids.len() != prepared.len() {
            return Err(Error::Checkpoint(
                "prepared question identity count mismatch".into(),
            ));
        }
        let mut answers = Map::new();
        for (start, chunk) in prepared
            .chunks(self.batch_size)
            .enumerate()
            .map(|(i, c)| (i * self.batch_size, c))
        {
            if !eligible() {
                return Ok(None);
            }
            let batch = collate_iter(chunk.iter().map(|(_, it)| it), self.tokenizer.pad_id)?;
            let mut ws =
                self.model
                    .alloc_workspace(&self.backend, batch.n, batch.seq_len, batch.kmax)?;
            if !eligible() {
                return Ok(None);
            }
            let out = self.model.forward(&self.backend, &mut ws, &batch)?;
            if !eligible() {
                return Ok(None);
            }
            for (row, (q, item)) in chunk.iter().enumerate() {
                if out.logits[row]
                    .iter()
                    .chain(&out.act[row])
                    .any(|v| !v.is_finite())
                {
                    return Err(Error::Checkpoint("non-finite model outputs".into()));
                }
                let k = item.markers.len();
                let scale = self.temperature_for(q.kind, k);
                let z: Vec<f32> = out.logits[row][..k].iter().map(|v| v / scale).collect();
                let p = softmax(&z);
                let act = softmax(&out.act[row]);
                answers.insert(ids[start + row].clone(), answer(q, &p, act[0]));
            }
        }
        if !eligible() {
            return Ok(None);
        }
        let input_tokens: usize = prepared.iter().map(|(_, it)| it.ids.len()).sum();
        Ok(Some(json!({
            "model": "laya-rl-agent",
            "answers": answers,
            "usage": { "input_tokens": input_tokens, "output_tokens": 0 },
        })))
    }
}
