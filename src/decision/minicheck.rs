//! Bespoke-MiniCheck's grounded fact-checking, the way its reference
//! implementation (Liyan06/MiniCheck, `inference.py`) runs it: one ChatML
//! prompt per claim, answered by the probability of `yes` as the first token.
use crate::backend::{Backend, Error, Result as BResult};
use crate::federation::catalog::MAX_DECIDE_QUESTIONS;
use crate::session::Session;
use crate::tokenizer::{ChatFormat, Tokenizer};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Verbatim from MiniCheck's `utils.py`.
pub const SYSTEM_PROMPT: &str = "Determine whether the provided claim is consistent with the corresponding document. Consistency in this context implies that all information presented in the claim is substantiated by the document. If not, it should be considered inconsistent. Please assess the claim's consistency with the document by responding with either \"Yes\" or \"No\".";

/// The `model` field of every result.
pub const NAME: &str = "minicheck";

/// A document and the claims to check against it, in request order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub document: String,
    /// (question id, claim)
    pub claims: Vec<(String, String)>,
}

impl Request {
    /// `state_json` is `{"document": ...}`, `questions_json` maps each id to
    /// `{"claim": ...}`: 1 to 128 claims, nothing empty, no other fields.
    pub fn parse(state_json: &str, questions_json: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct State {
            document: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Question {
            claim: String,
        }
        let State { document } =
            serde_json::from_str(state_json).map_err(|e| format!("state_json: {e}"))?;
        if document.trim().is_empty() {
            return Err("state_json: the document is empty".into());
        }
        let questions: Map<String, Value> =
            serde_json::from_str(questions_json).map_err(|e| format!("questions_json: {e}"))?;
        if questions.is_empty() || questions.len() > MAX_DECIDE_QUESTIONS as usize {
            return Err(format!(
                "questions_json: 1 to {MAX_DECIDE_QUESTIONS} claims, got {}",
                questions.len()
            ));
        }
        let claims = questions
            .into_iter()
            .map(|(id, question)| {
                let Question { claim } = serde_json::from_value(question)
                    .map_err(|e| format!("questions_json: {id:?}: {e}"))?;
                if claim.trim().is_empty() {
                    return Err(format!("questions_json: claim {id:?} is empty"));
                }
                Ok((id, claim))
            })
            .collect::<Result<_, String>>()?;
        Ok(Self { document, claims })
    }

    /// One prompt per claim, in claim order.
    pub fn prompts(&self) -> Vec<String> {
        self.claims
            .iter()
            .map(|(_, claim)| prompt(&self.document, claim))
            .collect()
    }
}

/// The system prompt and one user turn, ending where the answer begins.
pub fn prompt(document: &str, claim: &str) -> String {
    ChatFormat::ChatMl.conversation(
        Some(SYSTEM_PROMPT),
        &format!("Document: {document}\nClaim: {claim}"),
    )
}

/// The pieces that read `yes` in any case (`Yes`, `yes`; not `▁Yes`).
pub fn yes_ids(tokenizer: &Tokenizer) -> Vec<u32> {
    tokenizer.ids_where(|piece| piece.eq_ignore_ascii_case("yes"))
}

/// The probability mass on `yes` after the prompt; `None` for empty or
/// non-finite logits.
pub fn support(logits: &[f32], yes: &[u32]) -> Option<f32> {
    if logits.is_empty() || logits.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let weight = |logit: f32| f64::from(logit - max).exp();
    let total: f64 = logits.iter().copied().map(weight).sum();
    let yes: f64 = yes
        .iter()
        .filter_map(|&id| logits.get(id as usize))
        .copied()
        .map(weight)
        .sum();
    Some((yes / total) as f32)
}

/// Each claim's probability, in claim order, and the prompt tokens evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub probabilities: Vec<f32>,
    pub input_tokens: usize,
}

/// Prefills every prompt on `session` (which should reuse prefixes) and
/// reads its support. Longest first, so the first KV allocation holds the
/// rest and the shared system and document prefix stays cached. On memory
/// exhaustion `evict` frees room and says whether to retry. `None` when
/// cancelled.
pub(crate) fn score<B>(
    session: &Session<B>,
    prompts: &[String],
    yes: &[u32],
    cancelled: &dyn Fn() -> bool,
    mut evict: impl FnMut() -> BResult<bool>,
) -> BResult<Option<Scored>>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    let mut order: Vec<usize> = (0..prompts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(prompts[i].len()));
    let mut probabilities = vec![0.0; prompts.len()];
    let mut input_tokens = 0;
    for i in order {
        let prepared = loop {
            match session.prepare_scoring(&prompts[i]) {
                Err(error) if error.is_memory_exhausted() && evict()? => {}
                result => break result?,
            }
        };
        input_tokens += prepared.admission().prompt_tokens;
        let Some(probability) = prepared.score(cancelled, |logits| support(logits, yes))? else {
            return Ok(None);
        };
        probabilities[i] =
            probability.ok_or_else(|| Error::Backend("empty or nonfinite logits".into()))?;
    }
    Ok(Some(Scored {
        probabilities,
        input_tokens,
    }))
}

/// `{"model": "minicheck", "answers": {id: {"supported", "probability"}},
/// "usage": {"input_tokens", "output_tokens": 0}}`; supported above 0.5.
pub fn answer(request: &Request, scored: &Scored) -> Value {
    let answers: Map<String, Value> = request
        .claims
        .iter()
        .zip(&scored.probabilities)
        .map(|((id, _), &p)| {
            (
                id.clone(),
                json!({ "supported": p > 0.5, "probability": f64::from(p) }),
            )
        })
        .collect();
    json!({
        "model": NAME,
        "answers": answers,
        "usage": { "input_tokens": scored.input_tokens, "output_tokens": 0 },
    })
}
