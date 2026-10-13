//! Isolated token handoff with an explicit job join. EOF alone is not success.
use super::*;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use worker::{Prepared, WorkerEnd};

// Reserve worst-case wire space before decoding. Eight 64KiB slots bound
// native handoff to 512KiB, leaving room for the RPC encoder/HTTP2 handoff.
pub(crate) const SLOTS: usize = 8;
const MAX_TEXT: usize = 60 * 1024; // Leave room within 64KiB for protobuf fields.

pub(crate) struct Token {
    token: GeneratedToken,
    _slot: OwnedSemaphorePermit,
}

impl Token {
    pub(crate) fn token(&self) -> &GeneratedToken {
        &self.token
    }
}

pub(crate) struct Tokens {
    inner: ReceiverStream<Token>,
    cancel: CancellationToken,
    eof: bool,
}

impl Stream for Tokens {
    type Item = Token;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Token>> {
        let result = Pin::new(&mut self.inner).poll_next(cx);
        if matches!(result, Poll::Ready(None)) {
            self.eof = true;
        }
        result
    }
}

impl Drop for Tokens {
    fn drop(&mut self) {
        // Normal EOF still requires the job join, but it must not manufacture
        // caller cancellation before the adapter can emit Completed.
        if !self.eof {
            self.cancel.cancel();
        }
    }
}
pub(crate) type Job = Box<dyn FnOnce() -> BResult<WorkerEnd> + Send>;
pub(crate) type Boundary = Arc<dyn Fn() -> bool + Send + Sync>;

pub(crate) struct Running {
    pub tokens: Tokens,
    pub worker: JoinHandle<BResult<WorkerEnd>>,
}

/// Prefill and decode time of one run, as `completed` events report them.
pub(crate) struct Timing {
    started: Instant,
    first: Option<Instant>,
    last: Instant,
}

impl Timing {
    /// `started` is when the worker was launched.
    pub(crate) const fn new(started: Instant) -> Self {
        Self {
            started,
            first: None,
            last: started,
        }
    }

    /// Records a token handed to the response now.
    pub(crate) fn token(&mut self) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = now;
    }

    /// Milliseconds to the first token, and from it to the last one.
    pub(crate) fn millis(&self) -> (u32, u32) {
        let millis = |d: Duration| u32::try_from(d.as_millis()).unwrap_or(u32::MAX);
        let first = self.first.unwrap_or_else(Instant::now);
        (
            millis(first.duration_since(self.started)),
            millis(self.last.saturating_duration_since(first)),
        )
    }
}

impl<B> Session<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    /// `generate_guarded`, also returning the admission and the worker handle.
    pub(crate) fn start_tracked(
        &self,
        prompt: &str,
        max_tokens: usize,
        cancel: CancellationToken,
        guard: Option<ShutdownGuard>,
    ) -> BResult<(worker::Admission, Running)> {
        let prepared = self.prepare(
            prompt,
            max_tokens,
            !cancel.is_cancelled(),
            self.prefix.clone(),
        )?;
        let admission = prepared.admission().clone();
        Ok((admission, prepared.start(cancel, guard)))
    }
}

impl<B> Prepared<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    /// Revalidate identity/grants/deadline/power before invoking this. Memory
    /// is already reserved, but backend allocation can still fail in the job.
    pub(crate) fn into_job(
        self,
        cancel: CancellationToken,
        guard: Option<ShutdownGuard>,
    ) -> (Tokens, Job) {
        self.into_job_checked(cancel, guard, Arc::new(|| true))
    }

    pub(crate) fn into_job_checked(
        self,
        cancel: CancellationToken,
        guard: Option<ShutdownGuard>,
        boundary: Boundary,
    ) -> (Tokens, Job) {
        let (tx, rx) = mpsc::channel(SLOTS);
        let slots = Arc::new(Semaphore::new(SLOTS));
        let runtime = tokio::runtime::Handle::current();
        let loop_cancel = cancel.clone();
        let job = Box::new(move || {
            let result = self.run(
                || loop_cancel.is_cancelled() || tx.is_closed() || !boundary(),
                |token_id, decode| {
                    let Some(slot) = runtime.block_on(async {
                        tokio::select! {
                            biased;
                            () = loop_cancel.cancelled() => None,
                            slot = slots.clone().acquire_owned() => slot.ok(),
                        }
                    }) else {
                        return Ok(false);
                    };
                    if loop_cancel.is_cancelled() || tx.is_closed() || !boundary() {
                        return Ok(false);
                    }
                    let text = decode(token_id)?;
                    if text.len() > MAX_TEXT {
                        return Err(crate::backend::Error::ShapeMismatch(
                            "federated token event too large".into(),
                        ));
                    }
                    let token = Token {
                        token: GeneratedToken { token_id, text },
                        _slot: slot,
                    };
                    Ok(runtime.block_on(async {
                        tokio::select! {
                            biased;
                            () = loop_cancel.cancelled() => false,
                            sent = tx.send(token) => sent.is_ok(),
                        }
                    }))
                },
            );
            drop(guard);
            result
        });
        (
            Tokens {
                inner: ReceiverStream::new(rx),
                cancel,
                eof: false,
            },
            job,
        )
    }

    pub(crate) fn start(self, cancel: CancellationToken, guard: Option<ShutdownGuard>) -> Running {
        let (tokens, job) = self.into_job(cancel, guard);
        Running {
            tokens,
            worker: tokio::task::spawn_blocking(job),
        }
    }
}
