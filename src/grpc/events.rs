//! `emit_events` Synthesize: accepted, tokens, completed.
use super::proto::{SynthesizeAccepted, SynthesizeCompleted, SynthesizeResponse};
use super::{TokenStream, native_status};
use crate::session::tracked::Running;
use crate::session::worker::{Admission, WorkerEnd};
use rama::futures::async_stream::stream_fn;
use rama::http::grpc::Status;
use std::time::{Duration, Instant};
use tokio_stream::StreamExt;

fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

fn saturating(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `started` is when the worker was launched.
pub(super) fn stream(admission: &Admission, running: Running, started: Instant) -> TokenStream {
    let accepted = SynthesizeAccepted {
        prompt_tokens: saturating(admission.prompt_tokens),
        max_tokens: saturating(admission.max_tokens),
        n_ctx: saturating(admission.n_ctx),
    };
    Box::pin(stream_fn(move |mut yielder| async move {
        yielder
            .yield_item(Ok(SynthesizeResponse {
                accepted: Some(accepted),
                ..SynthesizeResponse::default()
            }))
            .await;
        let Running { mut tokens, worker } = running;
        let (mut first, mut last, mut emitted) = (None::<Instant>, started, 0_u32);
        while let Some(token) = tokens.next().await {
            let now = Instant::now();
            first.get_or_insert(now);
            last = now;
            emitted += 1;
            let token = token.token();
            yielder
                .yield_item(Ok(SynthesizeResponse {
                    token_id: token.token_id,
                    text: token.text.clone(),
                    ..SynthesizeResponse::default()
                }))
                .await;
        }
        let end = match worker.await {
            Ok(Ok(end)) => end,
            Ok(Err(error)) => return yielder.yield_item(Err(native_status(error))).await,
            Err(_) => {
                return yielder
                    .yield_item(Err(Status::internal("generation worker failed")))
                    .await;
            }
        };
        let completion = match end {
            WorkerEnd::Completed(completion) => completion,
            WorkerEnd::Cancelled => {
                return yielder
                    .yield_item(Err(Status::cancelled("generation cancelled")))
                    .await;
            }
        };
        if completion.emitted != emitted as usize {
            return yielder
                .yield_item(Err(Status::internal(
                    "token stream and worker disagree on emitted tokens",
                )))
                .await;
        }
        let first = first.unwrap_or_else(Instant::now);
        yielder
            .yield_item(Ok(SynthesizeResponse {
                completed: Some(SynthesizeCompleted {
                    emitted_tokens: emitted,
                    finish_reason: completion.reason.as_str().into(),
                    prefill_ms: millis(first.duration_since(started)),
                    decode_ms: millis(last.saturating_duration_since(first)),
                }),
                ..SynthesizeResponse::default()
            }))
            .await;
    }))
}
