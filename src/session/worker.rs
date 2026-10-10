//! Shared native generation loop and pre-launch memory reservation.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinishReason {
    Eos,
    MaxTokens,
    ContextLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completion {
    pub emitted: usize,
    pub reason: FinishReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerEnd {
    Completed(Completion),
    Cancelled,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Resolved admission tuple is consumed by the upcoming RPC adapter.
pub(crate) struct Admission {
    pub prompt_tokens: usize,
    pub max_tokens: usize,
    pub n_ctx: usize,
    pub sampler: SamplerConfig,
}

pub(crate) struct Prepared<B: Backend> {
    backend: Arc<B>,
    model: Arc<Model<B>>,
    tokenizer: Arc<Tokenizer>,
    prompt_ids: Vec<u32>,
    admission: Admission,
    context_limited: bool,
    capacity: usize,
    lease: PromptLease<B>,
}

impl<B> Session<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    pub(super) fn prepare(
        &self,
        prompt: &str,
        requested: usize,
        reserve: bool,
        prefix: Option<SharedPrefix<B>>,
    ) -> BResult<Prepared<B>> {
        let prompt_ids = self.tokenizer.try_encode(prompt)?;
        let n_ctx = self.n_ctx.min(self.model.config().n_ctx);
        if prompt_ids.is_empty() {
            return Err(crate::backend::Error::ShapeMismatch(
                "prompt must encode to at least one token".into(),
            ));
        }
        if prompt_ids.len() > n_ctx || (prompt_ids.len() == n_ctx && requested > 0) {
            return Err(crate::backend::Error::ContextLimit {
                prompt_tokens: prompt_ids.len(),
                n_ctx,
            });
        }
        let remaining = n_ctx - prompt_ids.len();
        let max_tokens = requested.min(remaining);
        let capacity = prompt_ids.len() + max_tokens.saturating_sub(1);
        let mut lease = PromptLease::reserve(prefix);
        if reserve {
            lease.admit(&*self.backend, &self.model, capacity, prompt_ids.len())?;
        }
        Ok(Prepared {
            backend: self.backend.clone(),
            model: self.model.clone(),
            tokenizer: self.tokenizer.clone(),
            admission: Admission {
                prompt_tokens: prompt_ids.len(),
                max_tokens,
                n_ctx,
                sampler: self.sampler_config.clone(),
            },
            prompt_ids,
            context_limited: requested > remaining,
            capacity,
            lease,
        })
    }

    /// Native-only primitive, not permission to execute. Ignores any opted-in
    /// local prefix slot: federated requests always reserve isolated KV.
    #[allow(dead_code)] // Workload RPC integration is not enabled yet.
    pub(crate) fn prepare_federated(&self, prompt: &str, requested: usize) -> BResult<Prepared<B>> {
        let s = &self.sampler_config;
        if requested == 0
            || prompt.len() > 65_536
            || !s.temperature.is_finite()
            || s.temperature < 0.0
            || !s.top_p.is_finite()
            || s.top_p <= 0.0
            || s.top_p > 1.0
            || !s.min_p.is_finite()
            || !(0.0..=1.0).contains(&s.min_p)
            || !s.repetition_penalty.is_finite()
            || s.repetition_penalty <= 0.0
            || s.repetition_window > u32::MAX as usize
        {
            return Err(crate::backend::Error::ShapeMismatch(
                "invalid federated generation parameters".into(),
            ));
        }
        self.prepare(prompt, requested, true, None)
    }
}

impl<B> Prepared<B>
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    #[allow(dead_code)]
    pub(crate) fn admission(&self) -> &Admission {
        &self.admission
    }

    /// FnOnce consumes all compute resources before its terminal value returns.
    /// Token decode is delegated to the sink so it can reserve buffer capacity
    /// BEFORE constructing decoded text.
    pub(crate) fn run(
        mut self,
        cancelled: impl Fn() -> bool,
        emit: impl FnMut(u32, &mut dyn FnMut(u32) -> BResult<String>) -> BResult<bool>,
    ) -> BResult<WorkerEnd> {
        struct Join<'a, B: Backend>(&'a B, bool);
        impl<B: Backend> Drop for Join<'_, B> {
            fn drop(&mut self) {
                if !self.1 {
                    // Also runs on panic before Prepared (and its KV lease)
                    // is dropped. GPU backends quarantine failed submissions.
                    let _ = self.0.join_execution();
                }
            }
        }
        let backend = self.backend.clone();
        let mut join = Join(&*backend, false);
        let fsm = SessionFsm::new(())
            .start_generating()
            .map_err(|_| crate::backend::Error::Backend("invalid generation lifecycle".into()))?;
        let generated = self.generate(&cancelled, emit);
        let joined = backend.join_execution();
        join.1 = true;
        let result = joined.and(generated);
        let result = match result {
            Ok(_) if cancelled() => Ok(WorkerEnd::Cancelled),
            result => result,
        };
        // Terminal lifecycle bookkeeping: `result` is already decided and is what the caller sees.
        match &result {
            Err(_) => {
                self.lease.retain.set(false);
                let _ = fsm.fail();
            }
            Ok(WorkerEnd::Cancelled) => {
                let _ = fsm.cancel();
            }
            Ok(WorkerEnd::Completed(_)) => {
                let _ = fsm.finish();
            }
        }
        result
    }

    fn generate(
        &mut self,
        cancelled: &impl Fn() -> bool,
        mut emit: impl FnMut(u32, &mut dyn FnMut(u32) -> BResult<String>) -> BResult<bool>,
    ) -> BResult<WorkerEnd> {
        enum Step {
            Token(u32),
            Eos,
            Cancelled,
        }
        if cancelled() {
            return Ok(WorkerEnd::Cancelled);
        }
        self.lease
            .allocate(&*self.backend, &self.model, self.capacity)?;
        if cancelled() {
            return Ok(WorkerEnd::Cancelled);
        }
        let state = self.lease.state.as_mut().expect("allocate populated the prompt state");
        let (logits, _) = state.prefill_reserved(
            &*self.backend,
            &self.model,
            &self.prompt_ids,
            self.lease.memory.scratch.as_ref(),
        )?;
        if cancelled() {
            return Ok(WorkerEnd::Cancelled);
        }
        let reason = if self.context_limited {
            FinishReason::ContextLimit
        } else {
            FinishReason::MaxTokens
        };
        if self.admission.max_tokens == 0 {
            return Ok(WorkerEnd::Completed(Completion { emitted: 0, reason }));
        }
        let mut history = std::mem::take(&mut self.prompt_ids);
        let sampler = Sampler::new(self.admission.sampler.clone());
        let mut rng = rand::rng();
        let mut decode = self.tokenizer.stream_decoder();
        let mut next = |logits: &[f32]| -> BResult<Step> {
            if cancelled() {
                return Ok(Step::Cancelled);
            }
            let token = sampler.sample(logits, &history, &mut rng);
            if self.tokenizer.is_eos(token) {
                return Ok(Step::Eos);
            }
            if !emit(token, &mut decode)? {
                return Ok(Step::Cancelled);
            }
            history.push(token);
            Ok(Step::Token(token))
        };
        let mut token = match next(&logits)? {
            Step::Token(token) => token,
            Step::Eos => {
                return Ok(WorkerEnd::Completed(Completion {
                    emitted: 0,
                    reason: FinishReason::Eos,
                }));
            }
            Step::Cancelled => return Ok(WorkerEnd::Cancelled),
        };
        if self.admission.max_tokens == 1 {
            return Ok(WorkerEnd::Completed(Completion { emitted: 1, reason }));
        }
        if cancelled() {
            return Ok(WorkerEnd::Cancelled);
        }
        let mut ws = self.model.alloc_workspace_reserved(
            &self.backend,
            1,
            self.lease.memory.scratch.as_ref(),
        )?;
        let mut decoder = self.backend.decoder(&self.model, &mut state.kv, &mut ws)?;
        let mut emitted = 1;
        for step in 1..self.admission.max_tokens {
            if cancelled() {
                return Ok(WorkerEnd::Cancelled);
            }
            let logits =
                decoder.forward(token, (self.admission.prompt_tokens + step - 1) as u32)?;
            match next(&logits)? {
                Step::Token(next) => {
                    token = next;
                    emitted += 1;
                }
                Step::Eos => {
                    return Ok(WorkerEnd::Completed(Completion {
                        emitted,
                        reason: FinishReason::Eos,
                    }));
                }
                Step::Cancelled => return Ok(WorkerEnd::Cancelled),
            }
        }
        Ok(WorkerEnd::Completed(Completion { emitted, reason }))
    }
}
