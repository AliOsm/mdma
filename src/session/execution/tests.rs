use super::*;
use crate::backend::{AttnSpec, Backend, Error, KvView, Pool, QWeight, Result, RopeSpec};
use crate::kv_cache::KvCache;
use crate::memory::NativeMemoryBudget;
use crate::model::Workspace;
use crate::model::decoder::{Decoder, EagerDecoder};
use std::assert_matches;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

#[path = "tracked_tests.rs"]
mod tracked;

#[derive(Default)]
struct CountingBackend {
    allocations: AtomicUsize,
    kv_bytes: AtomicUsize,
    decoders: AtomicUsize,
    embeds: Mutex<Vec<Vec<u32>>>,
    fail_allocation: AtomicBool,
    fail_nth_allocation: AtomicUsize,
    fail_decoder: AtomicBool,
    joins: AtomicUsize,
    fail_join: AtomicBool,
    on_join: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    on_decoder_drop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    on_embed: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    // Allocation-only test mode; compute still delegates to the f32 CPU.
    half_kv: bool,
}

struct DropDecoder<'a> {
    inner: EagerDecoder<'a, CountingBackend>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl Decoder for DropDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.inner.forward(token, pos)
    }
}

impl Drop for DropDecoder<'_> {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}

impl CountingBackend {
    fn reset(&self) {
        self.allocations.store(0, SeqCst);
        self.kv_bytes.store(0, SeqCst);
        self.decoders.store(0, SeqCst);
        self.embeds.lock().unwrap().clear();
    }
}

impl Backend for CountingBackend {
    type Buf = <CpuBackend as Backend>::Buf;

    fn join_execution(&self) -> Result<()> {
        self.joins.fetch_add(1, SeqCst);
        let hook = self.on_join.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        if self.fail_join.load(SeqCst) {
            return Err(Error::Backend("injected device join failure".into()));
        }
        Ok(())
    }

    fn kv_elem_bytes(&self) -> usize {
        if self.half_kv { 2 } else { 4 }
    }

    fn decoder<'a>(
        &'a self,
        model: &'a Model<Self>,
        kv: &'a mut KvCache<Self>,
        ws: &'a mut Workspace<Self>,
    ) -> Result<Box<dyn Decoder + 'a>> {
        self.decoders.fetch_add(1, SeqCst);
        if self.fail_decoder.load(SeqCst) {
            return Err(Error::Backend("injected decoder failure".into()));
        }
        Ok(Box::new(DropDecoder {
            inner: EagerDecoder {
                backend: self,
                model,
                kv,
                ws,
            },
            on_drop: self.on_decoder_drop.lock().unwrap().take(),
        }))
    }

    fn alloc(&self, bytes: usize, pool: Pool) -> Result<Self::Buf> {
        let count = self.allocations.fetch_add(1, SeqCst) + 1;
        if self.fail_allocation.load(SeqCst) || self.fail_nth_allocation.load(SeqCst) == count {
            return Err(Error::Backend("injected allocation failure".into()));
        }
        if matches!(pool, Pool::Kv) {
            self.kv_bytes.fetch_add(bytes, SeqCst);
        }
        CpuBackend.alloc(bytes, pool)
    }

    fn upload(&self, dst: &mut Self::Buf, offset: usize, src: &[u8]) -> Result<()> {
        CpuBackend.upload(dst, offset, src)
    }
    fn download(&self, src: &Self::Buf, offset: usize, dst: &mut [u8]) -> Result<()> {
        CpuBackend.download(src, offset, dst)
    }
    fn copy(
        &self,
        src: &Self::Buf,
        src_off: usize,
        dst: &mut Self::Buf,
        dst_off: usize,
        len: usize,
    ) -> Result<()> {
        CpuBackend.copy(src, src_off, dst, dst_off, len)
    }
    fn qmatmul(&self, w: &QWeight<Self::Buf>, x: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        CpuBackend.qmatmul(w, x, y)
    }
    fn embed(&self, table: &QWeight<Self::Buf>, tokens: &[u32], out: &mut Self::Buf) -> Result<()> {
        self.embeds.lock().unwrap().push(tokens.to_vec());
        let hook = self.on_embed.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        CpuBackend.embed(table, tokens, out)
    }
    fn rmsnorm(&self, w: &Self::Buf, x: &Self::Buf, y: &mut Self::Buf, eps: f32) -> Result<()> {
        CpuBackend.rmsnorm(w, x, y, eps)
    }
    fn add(&self, a: &Self::Buf, b: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        CpuBackend.add(a, b, y)
    }
    fn rope(
        &self,
        q: &mut Self::Buf,
        k: &mut Self::Buf,
        pos: u32,
        n_tokens: usize,
        spec: &RopeSpec,
    ) -> Result<()> {
        CpuBackend.rope(q, k, pos, n_tokens, spec)
    }
    fn attn(
        &self,
        q: &Self::Buf,
        kv: &KvView<Self::Buf>,
        spec: &AttnSpec,
        out: &mut Self::Buf,
    ) -> Result<()> {
        CpuBackend.attn(q, kv, spec, out)
    }
    fn silu_mul(&self, gate: &Self::Buf, up: &Self::Buf, y: &mut Self::Buf) -> Result<()> {
        CpuBackend.silu_mul(gate, up, y)
    }
}

fn tiny_session(n_ctx: usize) -> Session<CountingBackend> {
    let backend = Arc::new(CountingBackend::default());
    let model = Arc::new(crate::model::tests::tiny_model(&*backend));
    backend.reset();
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let config = SamplerConfig {
        temperature: 0.0,
        repetition_penalty: 1.0,
        ..SamplerConfig::default()
    };
    Session::new(backend, model, tokenizer, config, n_ctx)
}

fn kv_bytes(session: &Session<CountingBackend>, rows: usize) -> usize {
    let config = session.model.config();
    2 * config.n_layers * config.n_kv_heads * config.head_dim * 4 * rows
}

fn budget_session(extra: usize) -> (Session<CountingBackend>, NativeMemoryBudget, usize) {
    let file = crate::gguf::tests::Fixture::new(1, 8)
        .with_tokenizer()
        .write();
    let mut gguf = GgufModel::open(file.path()).unwrap();
    let weights = crate::model::memory::weight_bytes(&gguf).unwrap();
    let budget = NativeMemoryBudget::new(weights + extra);
    let backend = Arc::new(CountingBackend::default());
    let model = Arc::new(Model::load_with_budget(&*backend, &mut gguf, &budget).unwrap());
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    backend.reset();
    let config = SamplerConfig {
        temperature: 0.0,
        repetition_penalty: 1.0,
        ..SamplerConfig::default()
    };
    (
        Session::new(backend, model, tokenizer, config, 64),
        budget,
        weights,
    )
}

fn request_bytes(prompt: usize, output: usize) -> usize {
    // Tiny model: E=8, K=4, F=16, V=128, one layer, f32 KV.
    32 * (prompt + output.saturating_sub(1)) + 4 * (prompt * (6 * 8 + 2 * 4 + 3 * 16) + 8 + 128)
}

#[test]
fn budget_kv_sizing_uses_the_backends_element_width() {
    let backend = CountingBackend {
        half_kv: true,
        ..CountingBackend::default()
    };
    let model = crate::model::tests::tiny_model(&backend);
    let bytes = model.kv_bytes(&backend, 7).unwrap();
    let _kv = model.new_kv_cache(&backend, 7).unwrap();
    assert_eq!(bytes, 2 * 4 * 7 * 2);
    assert_eq!(backend.kv_bytes.load(SeqCst), bytes);
}

#[tokio::test]
async fn budget_stays_charged_until_cancelled_decoder_teardown_finishes() {
    use rama::graceful::Shutdown;
    use std::time::Duration;

    let peak = request_bytes(3, 40);
    let (session, budget, weights) = budget_session(peak);
    let trigger = CancellationToken::new();
    let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
    let (started, tearing_down) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    *session.backend.on_decoder_drop.lock().unwrap() = Some(Box::new(move || {
        started.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
    }));
    let mut stream = session
        .generate_guarded(
            "abc",
            40,
            CancellationToken::new(),
            Some(shutdown.guard_weak().upgrade()),
        )
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.backend.decoders.load(SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), tearing_down)
        .await
        .unwrap()
        .unwrap();
    let held = budget.used();
    let rejected = session
        .generate("abc", 4, CancellationToken::new())
        .map(|_| ());
    release.send(()).unwrap();
    trigger.cancel();
    shutdown
        .shutdown_with_limit(Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(held, weights + peak);
    assert_matches!(rejected, Err(Error::MemoryBudget { .. }));
    assert_eq!(budget.used(), weights);
}

#[tokio::test]
async fn budget_rejects_before_allocation_and_exact_peak_succeeds() {
    let peak = request_bytes(3, 4);
    let (session, budget, weights) = budget_session(peak - 1);
    assert_matches!(
        session
            .generate("abc", 4, CancellationToken::new())
            .map(|_| ()),
        Err(Error::MemoryBudget { .. })
    );
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert_eq!(budget.used(), weights);
    let (session, budget, weights) = budget_session(peak);
    assert_eq!(
        generate(&session, "abc", 4).await,
        generate(&tiny_session(64), "abc", 4).await
    );
    assert_eq!(session.backend.allocations.load(SeqCst), 2 + 13 + 13);
    assert_eq!(budget.used(), weights);
    drop(session);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn budget_refunds_partial_allocation_and_decoder_failures() {
    for allocation in [1, 2, 4, 16, 28, 0] {
        let (session, budget, weights) = budget_session(request_bytes(3, 4));
        let session = session.with_prefix_reuse();
        session
            .backend
            .fail_nth_allocation
            .store(allocation, SeqCst);
        session.backend.fail_decoder.store(allocation == 0, SeqCst);
        let items: Vec<_> = session
            .generate("abc", 4, CancellationToken::new())
            .unwrap()
            .collect()
            .await;
        assert!(items.last().unwrap().is_err(), "allocation {allocation}");
        assert_eq!(budget.used(), weights, "allocation {allocation}");
        let lease = crate::session::prefix::PromptLease::reserve(session.prefix.clone());
        assert!(lease.state.is_none());
        drop(lease);
        session.backend.fail_nth_allocation.store(0, SeqCst);
        session.backend.fail_decoder.store(false, SeqCst);
        assert_eq!(generate(&session, "abc", 4).await.len(), 4);
        session.clear_prefix_cache();
        assert_eq!(budget.used(), weights);
    }
}

#[tokio::test]
async fn budget_keeps_idle_prefix_kv_charged_but_not_workspace() {
    let (session, budget, weights) = budget_session(request_bytes(3, 4));
    let session = session.with_prefix_reuse();
    let expected = generate(&session, "abc", 4).await;
    assert_eq!(budget.used(), weights + kv_bytes(&session, 6));
    session.backend.reset();
    assert_eq!(generate(&session, "abc", 4).await, expected);
    assert_eq!(
        session.backend.kv_bytes.load(SeqCst),
        0,
        "KV lease must not be charged twice"
    );
    assert_eq!(
        session.backend.allocations.load(SeqCst),
        13,
        "exact prefix needs decode workspace only"
    );
    assert_eq!(budget.used(), weights + kv_bytes(&session, 6));
    // A failed larger admission returns the old prefix without freeing its charge.
    assert_matches!(
        session
            .generate("abcdefgh", 4, CancellationToken::new())
            .map(|_| ()),
        Err(Error::MemoryBudget { .. })
    );
    assert_eq!(budget.used(), weights + kv_bytes(&session, 6));
    session.clear_prefix_cache();
    assert_eq!(budget.used(), weights);
}

#[tokio::test]
async fn budget_precancelled_request_needs_no_spare_bytes() {
    let (session, budget, weights) = budget_session(0);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        session
            .generate("abc", 4, cancel)
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .is_empty()
    );
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert_eq!(budget.used(), weights);
}

#[test]
fn budget_is_shared_while_queued_and_refunded_after_disconnect_cleanup() {
    use rama::graceful::Shutdown;
    use std::time::Duration;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    rt.block_on(async {
        let peak = request_bytes(3, 4);
        let (session, budget, weights) = budget_session(peak);
        let other = Session::new(
            session.backend.clone(),
            session.model.clone(),
            session.tokenizer.clone(),
            session.sampler_config.clone(),
            64,
        );
        let trigger = CancellationToken::new();
        let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let stream = session
            .generate_guarded(
                "abc",
                4,
                CancellationToken::new(),
                Some(shutdown.guard_weak().upgrade()),
            )
            .unwrap();
        assert_eq!(budget.used(), weights + peak);
        assert_matches!(
            other
                .generate("abc", 4, CancellationToken::new())
                .map(|_| ()),
            Err(Error::MemoryBudget { .. })
        );
        assert_eq!(session.backend.allocations.load(SeqCst), 0);
        drop(stream);
        assert_eq!(
            budget.used(),
            weights + peak,
            "disconnect cannot refund memory still owned by a worker"
        );
        release.send(()).unwrap();
        blocker.await.unwrap();
        trigger.cancel();
        shutdown
            .shutdown_with_limit(Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(budget.used(), weights);
        assert_eq!(session.backend.allocations.load(SeqCst), 0);
        assert_eq!(generate(&other, "abc", 4).await.len(), 4);
        assert_eq!(budget.used(), weights);
    });
}

async fn generate(session: &Session<CountingBackend>, prompt: &str, max_tokens: usize) -> Vec<u32> {
    session
        .generate(prompt, max_tokens, CancellationToken::new())
        .unwrap()
        .map(|token| token.unwrap().token_id)
        .collect()
        .await
}

#[tokio::test]
async fn one_token_only_prefills_and_allocates_prompt_sized_kv() {
    let session = tiny_session(64);
    session.backend.fail_decoder.store(true, SeqCst);
    assert_eq!(generate(&session, "abc", 1).await.len(), 1);
    assert_eq!(session.backend.kv_bytes.load(SeqCst), kv_bytes(&session, 3));
    assert_eq!(session.backend.allocations.load(SeqCst), 2 + 13);
    assert_eq!(session.backend.decoders.load(SeqCst), 0);
    assert_eq!(*session.backend.embeds.lock().unwrap(), [vec![97, 98, 99]]);
}

#[tokio::test]
async fn generation_forwards_only_tokens_needed_for_the_next_sample() {
    let session = tiny_session(64);
    let ids = generate(&session, "abc", 4).await;
    assert_eq!(ids.len(), 4);
    assert_eq!(session.backend.kv_bytes.load(SeqCst), kv_bytes(&session, 6));
    assert_eq!(session.backend.decoders.load(SeqCst), 1);
    assert_eq!(session.backend.allocations.load(SeqCst), 2 + 13 + 13);
    let embeds = session.backend.embeds.lock().unwrap();
    assert_eq!(embeds[0], [97, 98, 99]);
    assert_eq!(
        embeds[1..],
        ids[..3].iter().map(|&id| vec![id]).collect::<Vec<_>>()
    );

    let cpu = CpuBackend;
    let model = crate::model::tests::tiny_model(&cpu);
    let mut kv = model.new_kv_cache(&cpu, 64).unwrap();
    let mut prefill = model.alloc_workspace(&cpu, 3).unwrap();
    let mut ws = model.alloc_workspace(&cpu, 1).unwrap();
    let mut logits = model
        .prefill(&cpu, &mut kv, &mut prefill, &[97, 98, 99], 0)
        .unwrap();
    for (step, &id) in ids.iter().enumerate() {
        assert_eq!(id, crate::sampler::greedy(&logits));
        logits = model
            .forward(&cpu, &mut kv, &mut ws, id, 3 + step as u32)
            .unwrap();
    }
}

#[tokio::test]
async fn zero_output_warms_prefix_without_a_decoder() {
    let session = tiny_session(64).with_prefix_reuse();
    assert!(generate(&session, "abc", 0).await.is_empty());
    assert_eq!(session.backend.kv_bytes.load(SeqCst), kv_bytes(&session, 3));
    assert_eq!(session.backend.decoders.load(SeqCst), 0);
    assert_eq!(*session.backend.embeds.lock().unwrap(), [vec![97, 98, 99]]);
    session.backend.reset();
    assert_eq!(generate(&session, "abc", 1).await.len(), 1);
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert!(session.backend.embeds.lock().unwrap().is_empty());
}

#[tokio::test]
async fn context_limits_reject_invalid_prompts_before_allocation_and_cap_output() {
    let session = tiny_session(6);
    for (prompt, output) in [("", 1), ("abcdefg", 0), ("abcdef", 1)] {
        assert!(
            session
                .generate(prompt, output, CancellationToken::new())
                .is_err()
        );
    }
    assert_matches!(
        session
            .generate("abcdefg", 0, CancellationToken::new())
            .map(|_| ()),
        Err(Error::ContextLimit {
            prompt_tokens: 7,
            n_ctx: 6
        })
    );
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert_eq!(generate(&session, "abcd", usize::MAX).await.len(), 2);
    assert_eq!(session.backend.kv_bytes.load(SeqCst), kv_bytes(&session, 5));
    assert!(generate(&session, "abcdef", 0).await.is_empty());

    let empty = tiny_session(0);
    assert!(empty.generate("a", 0, CancellationToken::new()).is_err());
    assert_eq!(empty.backend.allocations.load(SeqCst), 0);
}

#[tokio::test]
async fn model_context_bounds_large_session_limit_without_overflow() {
    let session = tiny_session(usize::MAX);
    assert_eq!(generate(&session, "abc", usize::MAX).await.len(), 61);
    assert_eq!(
        session.backend.kv_bytes.load(SeqCst),
        kv_bytes(&session, 63)
    );
}

#[tokio::test]
async fn immediate_eos_does_not_construct_a_decoder() {
    for extra_stop in [false, true] {
        let mut session = tiny_session(64);
        let eos = generate(&session, "abc", 1).await[0];
        session.tokenizer = Arc::new(if extra_stop {
            crate::tokenizer::tests::tiny_tokenizer_with_stop(eos)
        } else {
            crate::tokenizer::tests::tiny_tokenizer_with_eos(Some(eos))
        });
        session.backend.reset();
        session.backend.fail_decoder.store(true, SeqCst);
        assert!(generate(&session, "abc", 4).await.is_empty());
        assert_eq!(session.backend.decoders.load(SeqCst), 0);
        assert_eq!(session.backend.allocations.load(SeqCst), 2 + 13);
    }
}

#[tokio::test]
async fn precancelled_requests_allocate_nothing() {
    let session = tiny_session(64);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        session
            .generate("abc", 4, cancel)
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .is_empty()
    );
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
}

#[test]
fn allocation_waits_for_worker_and_queued_disconnect_skips_it() {
    use rama::graceful::Shutdown;
    use std::time::Duration;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    rt.block_on(async {
        for disconnect in [false, true] {
            let session = tiny_session(64);
            let trigger = CancellationToken::new();
            let shutdown = Shutdown::new(trigger.clone().cancelled_owned());
            let (release, wait) = std::sync::mpsc::channel();
            let (started, ready) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            ready.recv().unwrap();
            let mut stream = Some(session.generate_guarded(
                "abc",
                4,
                CancellationToken::new(),
                Some(shutdown.guard_weak().upgrade()),
            ));
            let before_release = session.backend.allocations.load(SeqCst);
            if disconnect {
                drop(stream.take());
            }
            release.send(()).unwrap();
            blocker.await.unwrap();
            if let Some(stream) = stream {
                assert_eq!(stream.unwrap().collect::<Vec<_>>().await.len(), 4);
            }
            trigger.cancel();
            shutdown
                .shutdown_with_limit(Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(before_release, 0, "no allocation on the async caller");
            assert_eq!(session.backend.allocations.load(SeqCst) == 0, disconnect);
        }
    });
}

#[tokio::test]
async fn worker_failures_are_streamed_and_do_not_retain_a_prefix() {
    let session = tiny_session(64).with_prefix_reuse();
    session.backend.fail_allocation.store(true, SeqCst);
    let items: Vec<_> = session
        .generate("abc", 4, CancellationToken::new())
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 1);
    assert_matches!(items[0].as_ref().map(|_| ()), Err(Error::Backend(message)) if message == "injected allocation failure");
    session.backend.fail_allocation.store(false, SeqCst);
    session.backend.fail_decoder.store(true, SeqCst);
    let items: Vec<_> = session
        .generate("abc", 4, CancellationToken::new())
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_ok());
    assert_matches!(items[1].as_ref().map(|_| ()), Err(Error::Backend(message)) if message == "injected decoder failure");
    let lease = crate::session::prefix::PromptLease::reserve(session.prefix.clone());
    assert!(lease.state.is_none());
}

#[tokio::test]
async fn concurrent_requests_keep_independent_kv_and_match_serial_output() {
    let session = tiny_session(64).with_prefix_reuse();
    let expected = generate(&session, "abc", 4).await;
    let (a, b, c, d) = tokio::join!(
        generate(&session, "abc", 4),
        generate(&session, "abc", 4),
        generate(&session, "abc", 4),
        generate(&session, "abc", 4),
    );
    for ids in <[_; 4]>::from((a, b, c, d)) {
        assert_eq!(ids, expected);
    }
}
