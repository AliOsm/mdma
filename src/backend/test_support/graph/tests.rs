use super::*;
use std::assert_matches;
use crate::backend::Error;
use crate::backend::cpu::CpuBackend;
use crate::model::decoder::EagerDecoder;

// Exercises the forwarding macro on this host without changing CpuBackend's
// production Backend implementation or its normal eager decoder.
trait TestDelegate: Backend + Sized {
    fn decoder<'a>(
        &'a self,
        model: &'a Model<Self>,
        kv: &'a mut KvCache<Self>,
        ws: &'a mut Workspace<Self>,
    ) -> Result<Box<dyn Decoder + 'a>>;
}

impl TestDelegate for CpuBackend {
    crate::backend::delegate_decoder!(eager_factory);
}

fn eager_factory<'a>(
    backend: &'a CpuBackend,
    model: &'a Model<CpuBackend>,
    kv: &'a mut KvCache<CpuBackend>,
    ws: &'a mut Workspace<CpuBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    if kv.capacity() == 0 {
        return Err(Error::Backend("test factory failure".into()));
    }
    Ok(Box::new(EagerDecoder { backend, model, kv, ws }))
}

// Explicit test double: exercises the shared concurrent scenario, NOT capture
// or replay on a GPU. The native suites must still run with real graph decoders.
struct MockGraph<'a>(EagerDecoder<'a, CpuBackend>);

impl Decoder for MockGraph<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> Result<Vec<f32>> {
        self.0.forward(token, pos)
    }

    fn is_graph(&self) -> bool {
        true
    }
}

#[expect(clippy::unnecessary_wraps, reason = "matches the fallible capture callback")]
fn mock_capture<'a>(
    backend: &'a CpuBackend,
    model: &'a Model<CpuBackend>,
    kv: &'a mut KvCache<CpuBackend>,
    ws: &'a mut Workspace<CpuBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    Ok(Box::new(MockGraph(EagerDecoder { backend, model, kv, ws })))
}

fn expected_tokens(backend: &CpuBackend, model: &Model<CpuBackend>, prompt: &[u32]) -> Vec<u32> {
    let mut kv = model.new_kv_cache(backend, 32).unwrap();
    let mut ws = model.alloc_workspace(backend, prompt.len()).unwrap();
    let logits = model.prefill(backend, &mut kv, &mut ws, prompt, 0).unwrap();
    let mut ws = model.alloc_workspace(backend, 1).unwrap();
    crate::backend::test_support::generation::greedy_continuation(
        backend,
        model,
        &mut kv,
        &mut ws,
        logits,
        prompt.len() as u32,
        3,
    )
}

#[test]
fn replay_scenario_and_delegation_preserve_positions_and_borrows() {
    let backend = CpuBackend;
    let model = crate::model::tests::tiny_model(&backend);
    let prompt = [1, 7, 13];
    let expected = expected_tokens(&backend, &model, &prompt);
    replay_matches_eager(&backend, &model, TestDelegate::decoder, &prompt, &expected, 12, 32);
    let mut kv = model.new_kv_cache(&backend, 0).unwrap();
    let mut ws = model.alloc_workspace(&backend, 1).unwrap();
    assert_matches!(
        TestDelegate::decoder(&backend, &model, &mut kv, &mut ws).map(|_| ()),
        Err(Error::Backend(message)) if message == "test factory failure"
    );
}

#[test]
fn concurrent_scenario_checks_tokens_and_rejects_eager_fallback() {
    let backend = CpuBackend;
    let model = crate::model::tests::tiny_model(&backend);
    let prompt = [1, 7, 13];
    let expected = expected_tokens(&backend, &model, &prompt);
    concurrent_prefill_replay(&backend, &model, mock_capture, 32, &prompt, &expected);
    assert!(
        std::panic::catch_unwind(|| {
            concurrent_prefill_replay(&backend, &model, eager_factory, 32, &prompt, &expected);
        })
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(|| {
            concurrent_prefill_replay(&backend, &model, mock_capture, 32, &prompt, &[u32::MAX]);
        })
        .is_err()
    );
}
