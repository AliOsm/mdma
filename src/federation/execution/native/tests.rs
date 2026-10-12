use super::super::ledger::{Ledger, RequestId};
use super::*;
use crate::backend::cpu::CpuBackend;
use crate::federation::config::Limits;
use crate::gguf::GgufModel;
use crate::memory::NativeMemoryBudget;
use crate::model::Model;
use crate::sampler::SamplerConfig;
use crate::session::{
    Session,
    worker::{Completion, FinishReason},
};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;

fn session() -> (Session<CpuBackend>, NativeMemoryBudget, usize) {
    let file = crate::gguf::tests::Fixture::new(1, 8)
        .with_tokenizer()
        .write();
    let mut gguf = GgufModel::open(file.path()).unwrap();
    let weights = crate::model::memory::weight_bytes(&gguf).unwrap();
    let budget = NativeMemoryBudget::new(weights + 32768);
    let backend = Arc::new(CpuBackend);
    let model = Arc::new(Model::load_with_budget(&*backend, &mut gguf, &budget).unwrap());
    let tokenizer = Arc::new(crate::tokenizer::tests::tiny_tokenizer());
    let sampler = SamplerConfig {
        temperature: 0.0,
        repetition_penalty: 1.0,
        ..Default::default()
    };
    (
        Session::new(backend, model, tokenizer, sampler, 64),
        budget,
        weights,
    )
}

fn claim(ledger: &Arc<Ledger>, id: u8) -> super::super::ledger::Claim {
    ledger
        .claim(
            "origin.mdma.test",
            RequestId::parse(&[id; 16]).unwrap(),
            &Limits::default(),
        )
        .unwrap()
}

#[tokio::test]
async fn native_join_releases_memory_and_claim_before_success_and_preserves_id() {
    let (session, budget, weights) = session();
    let ledger = Ledger::new();
    let prepared = session.prepare_federated("abc", 4).unwrap();
    let (tokens, completion) = launch(prepared, claim(&ledger, 1), CancellationToken::new(), None);
    assert_eq!(tokens.collect::<Vec<_>>().await.len(), 4);
    assert_eq!(
        completion.finish().await.unwrap(),
        WorkerEnd::Completed(Completion {
            emitted: 4,
            reason: FinishReason::MaxTokens
        })
    );
    assert_eq!(budget.used(), weights);
    assert!(matches!(
        ledger.claim(
            "origin.mdma.test",
            RequestId::parse(&[1; 16]).unwrap(),
            &Limits::default()
        ),
        Err(super::super::ledger::Error::Duplicate)
    ));
    drop(claim(&ledger, 2));
}

#[tokio::test]
async fn native_cancelled_backpressure_keeps_actual_job_charged_until_join() {
    let (session, budget, weights) = session();
    let ledger = Ledger::new();
    let cancel = CancellationToken::new();
    let prepared = session.prepare_federated("abc", 40).unwrap();
    let (mut tokens, completion) = launch(prepared, claim(&ledger, 1), cancel.clone(), None);
    let mut held = Vec::new();
    for _ in 0..crate::session::tracked::SLOTS {
        held.push(tokens.next().await.unwrap());
    }
    assert!(budget.used() > weights);
    assert!(matches!(
        ledger.claim(
            "origin.mdma.test",
            RequestId::parse(&[2; 16]).unwrap(),
            &Limits::default()
        ),
        Err(super::super::ledger::Error::Concurrency)
    ));
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), completion.finish())
            .await
            .unwrap()
            .unwrap(),
        WorkerEnd::Cancelled
    );
    assert_eq!(budget.used(), weights);
    drop(claim(&ledger, 3));
    drop(held);
}

#[tokio::test]
async fn dropping_receiver_cancels_without_premature_success_or_id_release() {
    let (session, budget, weights) = session();
    let ledger = Ledger::new();
    let prepared = session.prepare_federated("abc", 40).unwrap();
    let (tokens, completion) = launch(prepared, claim(&ledger, 1), CancellationToken::new(), None);
    drop(tokens);
    assert_eq!(completion.finish().await.unwrap(), WorkerEnd::Cancelled);
    assert_eq!(budget.used(), weights);
    assert!(matches!(
        ledger.claim(
            "origin.mdma.test",
            RequestId::parse(&[1; 16]).unwrap(),
            &Limits::default()
        ),
        Err(super::super::ledger::Error::Duplicate)
    ));
}

fn launch<B>(
    prepared: Prepared<B>,
    claim: Claim,
    cancel: CancellationToken,
    guard: Option<ShutdownGuard>,
) -> (Tokens, CompletionTask)
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    launch_inner(
        prepared,
        claim,
        cancel,
        guard,
        (),
        std::sync::Arc::new(|| true),
    )
}
