// Playground runs.
use super::deadline;
use super::pages;
use super::sse::{Event, append, element, signals};
use super::upstream::Upstream;
use mdma::grpc::{SynthesizeRequest, SynthesizeResponse, SynthesizeSamplerConfig};
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
        let text = |name: &str| match &signals[name] {
            Value::Null => Ok(String::new()),
            Value::String(s) => Ok(s.clone()),
            other => Err(format!("{name} must be text, got {other}")),
        };
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

fn progress(fields: &Value) -> Event {
    signals(&json!({ "_gen": fields }))
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
        let status = upstream.snapshot();
        let model = status.model(&request.model);
        if request.chat
            && let Some(m) = model.filter(|m| m.chat_format.is_empty())
        {
            for event in failed(format!("{} has no known chat template", m.name)) {
                yielder.yield_item(event).await;
            }
            return;
        }
        yielder.yield_item(element(pages::run_error(""))).await;
        yielder.yield_item(element(pages::output())).await;
        // The server loads a model on first use; Accepted follows the load.
        let phase = if model.is_some_and(|m| m.residency != "resident") {
            "loading"
        } else {
            "prefill"
        };
        yielder.yield_item(progress(&json!({
            "id": run.id, "phase": phase, "error": "",
            "model": model.map_or(request.model.as_str(), |m| m.name.as_str()),
            "n_ctx": model.map_or(0, |m| m.n_ctx), "max_tokens": request.max_tokens,
            "kv_bytes_per_token": model.and_then(|m| m.geometry.as_ref()).map_or(0, |g| g.kv_bytes_per_token),
            "emitted": 0, "tok_s": 0, "ttft_ms": 0, "dt_ms": 0,
            "prompt_tokens": 0, "finish_reason": "", "prefill_ms": 0, "decode_ms": 0,
        }))).await;

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
