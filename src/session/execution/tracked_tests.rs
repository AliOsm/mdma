use super::*;
use crate::session::worker::{Completion, FinishReason, WorkerEnd};
use std::time::Duration;

#[test]
fn admission_is_resolved_reserved_and_unlaunched_until_consumed() {
    let peak = request_bytes(3, 4);
    let (session, budget, weights) = budget_session(peak);
    let prepared = session.prepare_federated("abc", 4).unwrap();
    let info = prepared.admission();
    assert_eq!(
        (info.prompt_tokens, info.max_tokens, info.n_ctx),
        (3, 4, 64)
    );
    assert_eq!(info.sampler.temperature, 0.0);
    assert_eq!(budget.used(), weights + peak);
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert!(
        session.prepare_federated("abc", 4).is_err(),
        "shared memory is charged during validation"
    );
    drop(prepared);
    assert_eq!(budget.used(), weights);
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
}

#[test]
fn federated_bounds_reject_before_allocation_and_keep_local_zero_output_compatibility() {
    let session = tiny_session(64);
    assert!(session.prepare_federated("abc", 0).is_err());
    assert!(session.prepare_federated("", 1).is_err());
    assert!(session.prepare_federated(&"x".repeat(65_537), 1).is_err());
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    for field in 0..5 {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut session = tiny_session(64);
            match field {
                0 => session.sampler_config.temperature = value,
                1 => session.sampler_config.top_p = value,
                2 => session.sampler_config.min_p = value,
                3 => session.sampler_config.repetition_penalty = value,
                _ => session.sampler_config.temperature = -1.0,
            }
            assert!(session.prepare_federated("abc", 4).is_err());
            assert_eq!(session.backend.allocations.load(SeqCst), 0);
        }
    }
    for (top_p, min_p, penalty) in [
        (0.0, 0.0, 1.0),
        (1.1, 0.0, 1.0),
        (1.0, -0.1, 1.0),
        (1.0, 1.1, 1.0),
        (1.0, 0.0, 0.0),
    ] {
        let mut session = tiny_session(64);
        session.sampler_config.top_p = top_p;
        session.sampler_config.min_p = min_p;
        session.sampler_config.repetition_penalty = penalty;
        assert!(session.prepare_federated("abc", 1).is_err());
    }
}

async fn tracked(
    session: &Session<CountingBackend>,
    prompt: &str,
    output: usize,
) -> (Vec<u32>, WorkerEnd) {
    let running = session
        .prepare_federated(prompt, output)
        .unwrap()
        .start(CancellationToken::new(), None);
    let ids = running
        .tokens
        .map(|token| token.token().token_id)
        .collect()
        .await;
    let end = running.worker.await.unwrap().unwrap();
    (ids, end)
}

#[tokio::test]
async fn joined_completion_counts_match_ordinary_output_and_context_caps() {
    let session = tiny_session(64);
    let (ids, end) = tracked(&session, "abc", 4).await;
    assert_eq!(ids, generate(&session, "abc", 4).await);
    assert_eq!(
        end,
        WorkerEnd::Completed(Completion {
            emitted: 4,
            reason: FinishReason::MaxTokens
        })
    );
    let session = tiny_session(6);
    let prepared = session.prepare_federated("abcd", 40).unwrap();
    assert_eq!(
        (
            prepared.admission().prompt_tokens,
            prepared.admission().max_tokens
        ),
        (4, 2)
    );
    let running = prepared.start(CancellationToken::new(), None);
    assert_eq!(running.tokens.collect::<Vec<_>>().await.len(), 2);
    assert_eq!(
        running.worker.await.unwrap().unwrap(),
        WorkerEnd::Completed(Completion {
            emitted: 2,
            reason: FinishReason::ContextLimit
        })
    );
    let (_, end) = tracked(&session, "abcd", 2).await;
    assert_eq!(
        end,
        WorkerEnd::Completed(Completion {
            emitted: 2,
            reason: FinishReason::MaxTokens
        }),
        "equal output/context caps prefer the requested limit"
    );
}

#[tokio::test]
async fn device_join_precedes_memory_release_and_cannot_turn_failure_into_success() {
    let peak = request_bytes(3, 4);
    let (session, budget, weights) = budget_session(peak);
    let observed = budget.clone();
    *session.backend.on_join.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(observed.used(), weights + peak, "request reservation survives device completion");
    }));
    let (_, end) = tracked(&session, "abc", 4).await;
    assert!(matches!(end, WorkerEnd::Completed(_)));
    assert_eq!(session.backend.joins.load(SeqCst), 1);
    assert_eq!(budget.used(), weights);
    session.backend.fail_join.store(true, SeqCst);
    let running = session.prepare_federated("abc", 4).unwrap().start(CancellationToken::new(), None);
    let _ = running.tokens.collect::<Vec<_>>().await;
    assert!(running.worker.await.unwrap().is_err(), "token EOF cannot hide a failed device join");
    assert_eq!(session.backend.joins.load(SeqCst), 2);
}

#[tokio::test]
async fn panic_and_precancelled_jobs_still_join_the_device_before_finishing() {
    let session = tiny_session(64);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let running = session.prepare_federated("abc", 4).unwrap().start(cancel, None);
    assert!(running.tokens.collect::<Vec<_>>().await.is_empty());
    assert_eq!(running.worker.await.unwrap().unwrap(), WorkerEnd::Cancelled);
    assert_eq!(session.backend.joins.load(SeqCst), 1);
    let prepared = session.prepare_federated("abc", 4).unwrap();
    let backend = session.backend.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        prepared.run(|| false, |_, _| panic!("injected sink panic"))
    }));
    assert!(result.is_err());
    assert_eq!(backend.joins.load(SeqCst), 2);
}
#[tokio::test]
async fn eos_completion_does_not_construct_decoder_and_normal_eof_is_not_cancel() {
    let mut session = tiny_session(64);
    let eos = generate(&session, "abc", 1).await[0];
    session.tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer_with_eos(Some(eos)));
    session.backend.reset();
    session.backend.fail_decoder.store(true, SeqCst);
    let cancel = CancellationToken::new();
    let running = session
        .prepare_federated("abc", 4)
        .unwrap()
        .start(cancel.clone(), None);
    assert!(running.tokens.collect::<Vec<_>>().await.is_empty());
    assert!(
        !cancel.is_cancelled(),
        "normal stream EOF cannot cancel its completion adapter"
    );
    assert_eq!(
        running.worker.await.unwrap().unwrap(),
        WorkerEnd::Completed(Completion {
            emitted: 0,
            reason: FinishReason::Eos
        })
    );
    assert_eq!(session.backend.decoders.load(SeqCst), 0);
}

#[tokio::test]
async fn federated_generation_never_consumes_or_populates_the_local_prefix_slot() {
    let session = tiny_session(64).with_prefix_reuse();
    let expected = generate(&session, "abc", 4).await;
    session.backend.reset();
    let (actual, _) = tracked(&session, "abc", 4).await;
    assert_eq!(actual, expected);
    assert_eq!(session.backend.kv_bytes.load(SeqCst), kv_bytes(&session, 6));
    assert_eq!(session.backend.embeds.lock().unwrap()[0], vec![97, 98, 99]);
    session.backend.reset();
    assert_eq!(generate(&session, "abc", 4).await, expected);
    assert_eq!(
        session.backend.kv_bytes.load(SeqCst),
        0,
        "old local prefix survived, not replaced"
    );
    let session = tiny_session(64).with_prefix_reuse();
    tracked(&session, "abc", 4).await;
    let lease = crate::session::prefix::PromptLease::reserve(session.prefix.clone());
    assert!(
        lease.state.is_none(),
        "federated KV cannot remain in an opted-in local slot"
    );
}

#[tokio::test]
async fn failures_cannot_turn_empty_or_partial_streams_into_success() {
    for allocation in [1, 16, 0] {
        let (session, budget, weights) = budget_session(request_bytes(3, 4));
        session
            .backend
            .fail_nth_allocation
            .store(allocation, SeqCst);
        session.backend.fail_decoder.store(allocation == 0, SeqCst);
        let running = session
            .prepare_federated("abc", 4)
            .unwrap()
            .start(CancellationToken::new(), None);
        let tokens = running.tokens.collect::<Vec<_>>().await;
        assert_eq!(tokens.len(), usize::from(allocation != 1));
        assert!(
            running.worker.await.unwrap().is_err(),
            "EOF alone is not completion"
        );
        assert_eq!(budget.used(), weights);
    }
}

#[tokio::test]
async fn precancelled_tracked_worker_releases_reserved_memory_without_allocating() {
    let (session, budget, weights) = budget_session(request_bytes(3, 4));
    let prepared = session.prepare_federated("abc", 4).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let running = prepared.start(cancel, None);
    assert!(running.tokens.collect::<Vec<_>>().await.is_empty());
    assert_eq!(running.worker.await.unwrap().unwrap(), WorkerEnd::Cancelled);
    assert_eq!(session.backend.allocations.load(SeqCst), 0);
    assert_eq!(budget.used(), weights);
}

#[tokio::test]
async fn cancellation_unblocks_a_full_handoff_even_with_receiver_retained() {
    let (session, budget, weights) = budget_session(request_bytes(3, 40));
    let cancel = CancellationToken::new();
    let running = session
        .prepare_federated("abc", 40)
        .unwrap()
        .start(cancel.clone(), None);
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.backend.embeds.lock().unwrap().len() < crate::session::tracked::SLOTS + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let count = session.backend.embeds.lock().unwrap().len();
    assert_eq!(
        count, crate::session::tracked::SLOTS + 1,
        "charged tokens then one next-step logits boundary"
    );
    assert!(!running.worker.is_finished());
    cancel.cancel();
    let end = tokio::time::timeout(Duration::from_secs(2), running.worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(end, WorkerEnd::Cancelled);
    assert_eq!(budget.used(), weights);
    assert!(running.tokens.collect::<Vec<_>>().await.len() <= crate::session::tracked::SLOTS);
}

#[tokio::test]
async fn consumer_held_tokens_keep_slots_charged_before_next_decode() {
    let session = tiny_session(64);
    let cancel = CancellationToken::new();
    let mut running = session
        .prepare_federated("abc", 40)
        .unwrap()
        .start(cancel.clone(), None);
    let mut held = Vec::new();
    for _ in 0..crate::session::tracked::SLOTS {
        held.push(
            tokio::time::timeout(Duration::from_secs(2), running.tokens.next())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), running.tokens.next())
            .await
            .is_err(),
        "reading tokens without dropping permits cannot create more capacity"
    );
    drop(held.pop());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), running.tokens.next())
            .await
            .unwrap()
            .is_some()
    );
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), running.worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        WorkerEnd::Cancelled
    );
}

#[tokio::test]
async fn joined_completion_waits_for_decoder_teardown_and_budget_release() {
    let (session, budget, weights) = budget_session(request_bytes(3, 4));
    let (started, teardown) = tokio::sync::oneshot::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    *session.backend.on_decoder_drop.lock().unwrap() = Some(Box::new(move || {
        started.send(()).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    }));
    let mut running = session
        .prepare_federated("abc", 4)
        .unwrap()
        .start(CancellationToken::new(), None);
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(running.tokens.next().await.unwrap().token().token_id);
    }
    tokio::time::timeout(Duration::from_secs(2), teardown)
        .await
        .unwrap()
        .unwrap();
    assert!(!running.worker.is_finished());
    assert!(budget.used() > weights);
    assert!(session.prepare_federated("abc", 1).is_err());
    release.send(()).unwrap();
    assert_eq!(
        running.worker.await.unwrap().unwrap(),
        WorkerEnd::Completed(Completion {
            emitted: 4,
            reason: FinishReason::MaxTokens
        })
    );
    assert_eq!(budget.used(), weights);
    assert!(running.tokens.next().await.is_none());
    assert_eq!(ids, generate(&session, "abc", 4).await);
}

#[tokio::test]
async fn oversized_decoded_event_is_failed_not_queued_or_completed() {
    let mut session = tiny_session(64);
    let token = generate(&session, "abc", 1).await[0];
    session.tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer_with_decode_piece(
        token,
        "x".repeat(60 * 1024 + 1),
    ));
    let running = session
        .prepare_federated("abc", 4)
        .unwrap()
        .start(CancellationToken::new(), None);
    assert!(running.tokens.collect::<Vec<_>>().await.is_empty());
    assert_matches!(running.worker.await.unwrap(), Err(Error::ShapeMismatch(_)));
}

#[tokio::test]
async fn decoder_teardown_panic_cannot_masquerade_as_successful_eof() {
    let (session, budget, weights) = budget_session(request_bytes(3, 4));
    *session.backend.on_decoder_drop.lock().unwrap() =
        Some(Box::new(|| panic!("injected teardown panic")));
    let running = session
        .prepare_federated("abc", 4)
        .unwrap()
        .start(CancellationToken::new(), None);
    assert_eq!(running.tokens.collect::<Vec<_>>().await.len(), 4);
    assert!(running.worker.await.unwrap_err().is_panic());
    assert_eq!(
        budget.used(),
        weights,
        "unwind drops actual compute reservations"
    );
}

#[tokio::test]
async fn cancellation_during_prefill_holds_memory_until_safe_compute_boundary() {
    let peak = request_bytes(3, 4);
    let (session, budget, weights) = budget_session(peak);
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    *session.backend.on_embed.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    }));
    let cancel = CancellationToken::new();
    let running = session
        .prepare_federated("abc", 4)
        .unwrap()
        .start(cancel.clone(), None);
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    assert!(!running.worker.is_finished());
    assert_eq!(budget.used(), weights + peak);
    assert!(session.prepare_federated("abc", 4).is_err());
    release.send(()).unwrap();
    assert_eq!(running.worker.await.unwrap().unwrap(), WorkerEnd::Cancelled);
    assert!(running.tokens.collect::<Vec<_>>().await.is_empty());
    assert_eq!(budget.used(), weights);
}

#[test]
fn queued_disconnect_holds_reservation_until_cleanup_but_never_allocates() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    rt.block_on(async {
        let peak = request_bytes(3, 4);
        let (session, budget, weights) = budget_session(peak);
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        let running = session
            .prepare_federated("abc", 4)
            .unwrap()
            .start(CancellationToken::new(), None);
        drop(running.tokens);
        assert_eq!(budget.used(), weights + peak);
        assert_eq!(session.backend.allocations.load(SeqCst), 0);
        assert!(!running.worker.is_finished());
        release.send(()).unwrap();
        blocker.await.unwrap();
        assert_eq!(running.worker.await.unwrap().unwrap(), WorkerEnd::Cancelled);
        assert_eq!(budget.used(), weights);
        assert_eq!(session.backend.allocations.load(SeqCst), 0);
    });
}
