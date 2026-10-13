// Playground runs.
use super::deadline;
use super::pages;
use super::sse::{Event, append, element, signals};
use super::upstream::Upstream;
use mdma::grpc::{
    DecideRequest, ModelStatus, SynthesizeRequest, SynthesizeResponse, SynthesizeSamplerConfig,
};
use rama::futures::StreamExt;
use rama::futures::async_stream::stream_fn;
use rama::http::StatusCode;
use rama::http::protocols::html::*;
use rama::http::service::web::extract::datastar::ReadSignals;
use rama::http::service::web::extract::{Query, State};
use rama::http::service::web::response::{IntoResponse, Sse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_SYSTEM_BYTES: usize = 8 * 1024;
const MAX_TOKENS: u32 = 8192;
/// As many claims as one `Decide` takes.
const MAX_CLAIMS: usize = 128;

/// A text signal; absent means empty.
fn text(signals: &Value, name: &str) -> Result<String, String> {
    match &signals[name] {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        other => Err(format!("{name} must be text, got {other}")),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RunRequest {
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
    pub sampler: Option<SynthesizeSamplerConfig>,
    pub chat: bool,
    pub system: String,
}

impl RunRequest {
    /// Empty inputs mean the server default.
    pub fn parse(signals: &Value) -> Result<Self, String> {
        let text = |name: &str| text(signals, name);
        let number = |name: &str| -> Result<Option<f64>, String> {
            match &signals[name] {
                Value::Null => Ok(None),
                Value::String(s) if s.trim().is_empty() => Ok(None),
                Value::String(s) => s
                    .trim()
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{name} must be a number")),
                Value::Number(n) => Ok(n.as_f64()),
                other => Err(format!("{name} must be a number, got {other}")),
            }
        };
        let ranged = |name: &str, max: f64| -> Result<Option<f32>, String> {
            match number(name)? {
                None => Ok(None),
                Some(v) if v.is_finite() && (0.0..=max).contains(&v) => Ok(Some(v as f32)),
                Some(v) => Err(format!("{name} must be between 0 and {max}, got {v}")),
            }
        };

        let prompt = text("prompt")?;
        if prompt.trim().is_empty() {
            return Err("prompt is empty".into());
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "prompt is {} bytes; the playground accepts up to {MAX_PROMPT_BYTES}",
                prompt.len()
            ));
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a checked whole number in 1..=MAX_TOKENS"
        )]
        let max_tokens = match number("max_tokens")? {
            Some(n) if n.fract() == 0.0 && (1.0..=f64::from(MAX_TOKENS)).contains(&n) => n as u32,
            _ => {
                return Err(format!(
                    "max tokens must be a whole number between 1 and {MAX_TOKENS}"
                ));
            }
        };
        let sampler = SynthesizeSamplerConfig {
            temperature: ranged("temperature", 10.0)?,
            top_p: ranged("top_p", 1.0)?,
            min_p: ranged("min_p", 1.0)?,
            repetition_penalty: ranged("repetition_penalty", 10.0)?,
            repetition_window: None,
        };
        let chat = match &signals["chat"] {
            Value::Null => false,
            Value::Bool(chat) => *chat,
            other => return Err(format!("chat must be true or false, got {other}")),
        };
        let system = if chat {
            text("system")?.trim().to_owned()
        } else {
            String::new()
        };
        if system.len() > MAX_SYSTEM_BYTES {
            return Err(format!(
                "system message is {} bytes; the playground accepts up to {MAX_SYSTEM_BYTES}",
                system.len()
            ));
        }
        let unset = SynthesizeSamplerConfig::default();
        Ok(Self {
            model: text("model")?,
            prompt,
            max_tokens,
            sampler: (sampler != unset).then_some(sampler),
            chat,
            system,
        })
    }

    fn into_proto(self) -> SynthesizeRequest {
        SynthesizeRequest {
            prompt: self.prompt,
            max_tokens: self.max_tokens,
            sampler: self.sampler,
            model: self.model,
            emit_events: true,
            chat: self.chat,
            system: self.system,
        }
    }
}

/// A document and the claims to check against it, one per non-empty line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckRequest {
    pub model: String,
    pub document: String,
    pub claims: Vec<String>,
}

impl CheckRequest {
    pub fn parse(signals: &Value) -> Result<Self, String> {
        let document = text(signals, "document")?;
        if document.trim().is_empty() {
            return Err("document is empty".into());
        }
        let claims: Vec<String> = text(signals, "claims")?
            .lines()
            .map(str::trim)
            .filter(|claim| !claim.is_empty())
            .map(str::to_owned)
            .collect();
        if claims.is_empty() || claims.len() > MAX_CLAIMS {
            return Err(format!(
                "write 1 to {MAX_CLAIMS} claims, one per line; got {}",
                claims.len()
            ));
        }
        let bytes = document.len() + claims.iter().map(String::len).sum::<usize>();
        if bytes > MAX_PROMPT_BYTES {
            return Err(format!(
                "document and claims are {bytes} bytes; the playground accepts up to {MAX_PROMPT_BYTES}"
            ));
        }
        Ok(Self {
            model: text(signals, "model")?,
            document,
            claims,
        })
    }

    /// Claim ids are line numbers among the claims, from 1.
    fn to_proto(&self) -> DecideRequest {
        let questions: serde_json::Map<String, Value> = (1..)
            .zip(&self.claims)
            .map(|(id, claim): (usize, _)| (id.to_string(), json!({ "claim": claim })))
            .collect();
        DecideRequest {
            model: self.model.clone(),
            state_json: json!({ "document": self.document }).to_string(),
            questions_json: Value::Object(questions).to_string(),
        }
    }

    /// Each claim's verdict from a MiniCheck result, and the input tokens.
    fn verdicts(&self, result_json: &str) -> Result<(Vec<pages::Verdict>, u64), String> {
        let result: Value =
            serde_json::from_str(result_json).map_err(|e| format!("unreadable result: {e}"))?;
        let verdicts = (1..)
            .zip(&self.claims)
            .map(|(id, claim): (usize, _)| {
                let answer = &result["answers"][id.to_string()];
                match (
                    answer["supported"].as_bool(),
                    answer["probability"].as_f64(),
                ) {
                    (Some(supported), Some(probability)) => Ok(pages::Verdict {
                        claim: claim.clone(),
                        supported,
                        probability,
                    }),
                    _ => Err(format!("no verdict for claim {id} in {result_json}")),
                }
            })
            .collect::<Result<_, _>>()?;
        Ok((
            verdicts,
            result["usage"]["input_tokens"].as_u64().unwrap_or(0),
        ))
    }
}

fn progress(fields: &Value) -> Event {
    signals(&json!({ "_gen": fields }))
}

/// A run starting on `model` (`name` when it is not listed). The server
/// loads a model on first use, so it starts `loading` unless resident.
fn starting(id: u64, name: &str, model: Option<&ModelStatus>, max_tokens: u32) -> Event {
    let phase = if model.is_some_and(|m| m.residency != "resident") {
        "loading"
    } else {
        "prefill"
    };
    progress(&json!({
        "id": id, "phase": phase, "error": "",
        "model": model.map_or(name, |m| m.name.as_str()),
        "n_ctx": model.map_or(0, |m| m.n_ctx), "max_tokens": max_tokens,
        "kv_bytes_per_token": model.and_then(|m| m.geometry.as_ref()).map_or(0, |g| g.kv_bytes_per_token),
        "emitted": 0, "tok_s": 0, "ttft_ms": 0, "dt_ms": 0,
        "prompt_tokens": 0, "finish_reason": "", "prefill_ms": 0, "decode_ms": 0,
    }))
}

fn failed(message: impl Into<String>) -> Vec<Event> {
    let message = message.into();
    vec![
        element(pages::run_error(&message)),
        progress(&json!({ "phase": "error", "error": message })),
    ]
}

pub(super) async fn run(
    State(upstream): State<Arc<Upstream>>,
    ReadSignals(signals): ReadSignals<Value>,
) -> impl IntoResponse {
    Sse::new(stream_fn(move |mut yielder| async move {
        let status = upstream.snapshot();
        let model = status.model(signals["model"].as_str().unwrap_or_default());
        if pages::checks_claims(model) {
            let request = match CheckRequest::parse(&signals) {
                Ok(request) => request,
                Err(message) => {
                    for event in failed(message) {
                        yielder.yield_item(event).await;
                    }
                    return;
                }
            };
            let Some(run) = upstream.begin() else {
                for event in failed("all playground slots are busy (MDMA_WEB_MAX_STREAMS)") {
                    yielder.yield_item(event).await;
                }
                return;
            };
            yielder.yield_item(element(pages::run_error(""))).await;
            yielder.yield_item(element(pages::output(model))).await;
            yielder
                .yield_item(starting(run.id, &request.model, model, 0))
                .await;
            let started = Instant::now();
            let reply = match deadline::request(request.to_proto(), upstream.rpc_timeout) {
                Ok(call) => tokio::select! {
                    () = run.cancel.cancelled() => {
                        yielder.yield_item(progress(&json!({ "phase": "stopped" }))).await;
                        return;
                    }
                    reply = upstream.client.decide(call) => reply
                        .map(|reply| reply.into_inner().result_json)
                        .map_err(|status| format!("{:?}: {}", status.code(), status.message())),
                },
                Err(error) => Err(error.to_string()),
            };
            match reply.and_then(|result| request.verdicts(&result)) {
                Ok((verdicts, input_tokens)) => {
                    yielder
                        .yield_item(element(pages::verdicts(&verdicts)))
                        .await;
                    yielder
                        .yield_item(progress(&json!({
                            "phase": "done", "prompt_tokens": input_tokens,
                            "dt_ms": started.elapsed().as_millis(),
                        })))
                        .await;
                }
                Err(message) => {
                    for event in failed(message) {
                        yielder.yield_item(event).await;
                    }
                }
            }
            return;
        }
        let request = match RunRequest::parse(&signals) {
            Ok(request) => request,
            Err(message) => {
                for event in failed(message) {
                    yielder.yield_item(event).await;
                }
                return;
            }
        };
        let Some(run) = upstream.begin() else {
            for event in failed("all playground slots are busy (MDMA_WEB_MAX_STREAMS)") {
                yielder.yield_item(event).await;
            }
            return;
        };
        if request.chat
            && let Some(m) = model.filter(|m| m.chat_format.is_empty())
        {
            for event in failed(format!("{} has no known chat template", m.name)) {
                yielder.yield_item(event).await;
            }
            return;
        }
        yielder.yield_item(element(pages::run_error(""))).await;
        yielder.yield_item(element(pages::output(model))).await;
        yielder
            .yield_item(starting(run.id, &request.model, model, request.max_tokens))
            .await;

        let started = Instant::now();
        let call = match deadline::request(request.into_proto(), upstream.rpc_timeout) {
            Ok(call) => call,
            Err(error) => {
                for event in failed(error.to_string()) {
                    yielder.yield_item(event).await;
                }
                return;
            }
        };
        let mut tokens = tokio::select! {
            () = run.cancel.cancelled() => {
                yielder.yield_item(progress(&json!({ "phase": "stopped" }))).await;
                return;
            }
            response = upstream.client.synthesize(call) => match response {
                Ok(response) => response.into_inner(),
                Err(status) => {
                    for event in failed(format!("{:?}: {}", status.code(), status.message())) { yielder.yield_item(event).await; }
                    return;
                }
            },
        };

        let (mut emitted, mut first, mut last) = (0_u32, None::<Instant>, started);
        loop {
            let item = tokio::select! {
                biased;
                () = run.cancel.cancelled() => {
                    yielder.yield_item(progress(&json!({ "phase": "stopped" }))).await;
                    return;
                }
                item = tokens.next() => item,
            };
            match item {
                None => break,
                Some(Err(status)) => {
                    for event in failed(format!("{:?}: {}", status.code(), status.message())) {
                        yielder.yield_item(event).await;
                    }
                    return;
                }
                Some(Ok(SynthesizeResponse {
                    accepted: Some(accepted),
                    ..
                })) => {
                    yielder.yield_item(progress(&json!({
                        "phase": "prefill",
                        "prompt_tokens": accepted.prompt_tokens, "max_tokens": accepted.max_tokens, "n_ctx": accepted.n_ctx,
                    }))).await;
                }
                Some(Ok(SynthesizeResponse {
                    completed: Some(completed),
                    ..
                })) => {
                    yielder
                        .yield_item(progress(&json!({
                            "finish_reason": completed.finish_reason,
                            "prefill_ms": completed.prefill_ms, "decode_ms": completed.decode_ms,
                        })))
                        .await;
                }
                Some(Ok(token)) => {
                    let now = Instant::now();
                    emitted += 1;
                    let first = *first.get_or_insert(now);
                    let decode = now.duration_since(first).as_secs_f64();
                    let tok_s = if emitted > 1 && decode > 0.0 {
                        f64::from(emitted - 1) / decode
                    } else {
                        0.0
                    };
                    if !token.text.is_empty() {
                        yielder
                            .yield_item(append("#output", span!(token.text)))
                            .await;
                    }
                    yielder
                        .yield_item(progress(&json!({
                            "phase": "decode", "emitted": emitted, "tok_s": tok_s,
                            "ttft_ms": first.duration_since(started).as_millis(),
                            "dt_ms": now.duration_since(last).as_secs_f64() * 1000.0,
                        })))
                        .await;
                    last = now;
                }
            }
        }
        yielder
            .yield_item(progress(&json!({ "phase": "done" })))
            .await;
    }))
}

/// What a page shows after choosing `name`: the inputs that model takes, an
/// idle run sized to it, an empty output, and chat on only when it has a
/// template.
pub(super) fn selection(status: &super::upstream::Status, name: &str) -> Vec<Event> {
    let model = status.model(name);
    let chat = pages::chat_template(model).is_ok();
    vec![
        signals(&json!({
            "chat": chat,
            "_gen": {
                "id": 0, "phase": "idle", "error": "",
                "model": model.map_or(name, |m| m.name.as_str()),
                "n_ctx": model.map_or(0, |m| m.n_ctx),
                "kv_bytes_per_token": model.and_then(|m| m.geometry.as_ref()).map_or(0, |g| g.kv_bytes_per_token),
                "max_tokens": 0, "emitted": 0, "tok_s": 0, "ttft_ms": 0, "dt_ms": 0,
                "prompt_tokens": 0, "finish_reason": "", "prefill_ms": 0, "decode_ms": 0,
            },
        })),
        element(pages::inputs(model)),
        element(pages::stats(model)),
        element(pages::output(model)),
        element(pages::run_error("")),
    ]
}

pub(super) async fn select(
    State(upstream): State<Arc<Upstream>>,
    ReadSignals(signals): ReadSignals<Value>,
) -> impl IntoResponse {
    let name = signals["model"].as_str().unwrap_or_default().to_owned();
    let events = selection(&upstream.snapshot(), &name);
    Sse::new(stream_fn(move |mut yielder| async move {
        for event in events {
            yielder.yield_item(event).await;
        }
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct StopQuery {
    id: u64,
}

pub(super) async fn stop(
    State(upstream): State<Arc<Upstream>>,
    Query(query): Query<StopQuery>,
) -> impl IntoResponse {
    if upstream.stop(query.id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

#[cfg(test)]
#[path = "playground/tests.rs"]
mod tests;
