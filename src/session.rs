//! Synchronous inference runs in a blocking task, with tokens streamed
//! asynchronously over a channel. The lifecycle FSM tracks request boundaries.
use crate::backend::{Backend, Result as BResult};
use crate::model::Model;
use crate::sampler::{Sampler, SamplerConfig};
use crate::tokenizer::Tokenizer;
use rama::graceful::ShutdownGuard;
use state_machines::state_machine;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

mod prefix;
use prefix::{PrefixSlot, PromptLease, SharedPrefix};
#[allow(dead_code)] // Native tracked path precedes authenticated workload RPC integration.
pub(crate) mod tracked;
pub(crate) mod worker;

const TOKEN_STREAM_CAPACITY: usize = 16;

/// Cancels inference when the response stream is dropped, including during
/// prefill before the first channel send can detect a disconnected client.
/// The inner stream is `Unpin`, so polling needs no unsafe pin projection.
pub struct CancelOnDrop<S> {
    inner: S,
    cancel: CancellationToken,
}

impl<S> CancelOnDrop<S> {
    pub(crate) const fn new(inner: S, cancel: CancellationToken) -> Self {
        Self { inner, cancel }
    }
}

impl<S> Drop for CancelOnDrop<S> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl<S: Stream + Unpin> Stream for CancelOnDrop<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    InvalidTransition,
}

state_machine! {
    name: SessionFsm,
    error: SessionError,
    initial: Idle,
    states: [Idle, Generating, Done, Cancelled, Failed],
    events {
        start_generating { transition: { from: Idle, to: Generating } }
        finish           { transition: { from: Generating, to: Done } }
        cancel            { transition: { from: [Idle, Generating], to: Cancelled } }
        fail              { transition: { from: [Idle, Generating], to: Failed } }
    }
}

pub struct GeneratedToken {
    pub token_id: u32,
    pub text: String,
}

pub struct Session<B: Backend> {
    backend: Arc<B>,
    model: Arc<Model<B>>,
    tokenizer: Arc<Tokenizer>,
    sampler_config: SamplerConfig,
    n_ctx: usize,
    prefix: Option<SharedPrefix<B>>,
}

impl<B> Session<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    /// `n_ctx` caps prompt plus output, bounded by the model's context.
    /// Fresh KV caches allocate only the rows this request can evaluate.
    pub fn new(
        backend: Arc<B>,
        model: Arc<Model<B>>,
        tokenizer: Arc<Tokenizer>,
        sampler_config: SamplerConfig,
        n_ctx: usize,
    ) -> Self {
        Self {
            backend,
            model,
            tokenizer,
            sampler_config,
            n_ctx,
            prefix: None,
        }
    }

    /// Retain one idle prompt KV cache for reuse within this Session only.
    /// Concurrent calls lease exclusive caches; they never share writable KV.
    /// This retains up to `n_ctx` tokens of KV after completion. Opt in only
    /// within a single trust boundary: prompt-hit timing can reveal reuse.
    #[must_use]
    pub fn with_prefix_reuse(mut self) -> Self {
        self.prefix = Some(Arc::new(Mutex::new(PrefixSlot::new())));
        self
    }

    /// Evict the idle prefix and prevent in-flight requests from repopulating
    /// this cache. Active requests themselves are not cancelled.
    pub fn clear_prefix_cache(&self) {
        if let Some(prefix) = &self.prefix {
            prefix.lock().unwrap().clear();
        }
    }

    /// Each call is an independent generation, not a continued conversation.
    /// KV starts fresh unless `with_prefix_reuse` was explicitly enabled.
    /// The prompt must encode to at least one token and fit the context.
    /// Output is capped to the remaining context; a full prompt with
    /// nonzero requested output is rejected. Zero output still prefills.
    /// Budgeted models reserve new KV plus peak workspace before enqueueing;
    /// insufficient native tensor capacity returns `Error::MemoryBudget`.
    pub fn generate(
        &self,
        prompt: &str,
        max_tokens: usize,
        cancel: CancellationToken,
    ) -> BResult<CancelOnDrop<ReceiverStream<BResult<GeneratedToken>>>> {
        self.generate_guarded(prompt, max_tokens, cancel, None)
    }

    pub(crate) fn generate_guarded(
        &self,
        prompt: &str,
        max_tokens: usize,
        cancel: CancellationToken,
        guard: Option<ShutdownGuard>,
    ) -> BResult<CancelOnDrop<ReceiverStream<BResult<GeneratedToken>>>> {
        let (tx, rx) = mpsc::channel(TOKEN_STREAM_CAPACITY);
        let prepared = self.prepare(
            prompt,
            max_tokens,
            !cancel.is_cancelled(),
            self.prefix.clone(),
        )?;
        let loop_cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let result = prepared.run(
                || loop_cancel.is_cancelled() || tx.is_closed(),
                |token_id, decode| {
                    let text = decode(token_id)?;
                    Ok(tx
                        .blocking_send(Ok(GeneratedToken { token_id, text }))
                        .is_ok())
                },
            );
            // A closed channel means the client already disconnected; there is no one to tell.
            if let Err(error) = result {
                let _ = tx.blocking_send(Err(error));
            }
            drop(guard);
        });

        Ok(CancelOnDrop {
            inner: ReceiverStream::new(rx),
            cancel,
        })
    }
}

#[cfg(test)]
mod tests;
