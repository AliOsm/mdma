//! Graph test choreography only: factories own actual capture, backend
//! configuration, and fallback policy. No GPU graph implementation is shared.
use crate::backend::{Backend, Result};
use crate::kv_cache::KvCache;
use crate::model::decoder::Decoder;
use crate::model::{Model, Workspace};
use crate::sampler::greedy;

pub(crate) type Capture<B> = for<'a> fn(
    &'a B,
    &'a Model<B>,
    &'a mut KvCache<B>,
    &'a mut Workspace<B>,
) -> Result<Box<dyn Decoder + 'a>>;

pub(crate) fn replay_matches_eager<B: Backend>(
    backend: &B,
    model: &Model<B>,
    capture: Capture<B>,
    prompt: &[u32],
    expected: &[u32],
    steps: usize,
    capacity: usize,
) {
    let mut a = model.new_kv_cache(backend, capacity).unwrap();
    let mut b = model.new_kv_cache(backend, capacity).unwrap();
    let mut reference = model.new_kv_cache(backend, capacity).unwrap();
    let mut aw = model.alloc_workspace(backend, 1).unwrap();
    let mut bw = model.alloc_workspace(backend, 1).unwrap();
    let mut rw = model.alloc_workspace(backend, 1).unwrap();
    {
        // Direct capture factories, not the production fallback factories.
        let mut da = capture(backend, model, &mut a, &mut aw).unwrap();
        let mut db = capture(backend, model, &mut b, &mut bw).unwrap();
        let mut logits = Vec::new();
        for pos in 0..steps {
            let token = prompt.get(pos).copied().unwrap_or_else(|| greedy(&logits));
            let want = model
                .forward(backend, &mut reference, &mut rw, token, pos as u32)
                .unwrap();
            logits = da.forward(token, pos as u32).unwrap();
            assert_eq!(logits, want, "graph logits at position {pos}");
            db.forward((pos + 17) as u32, pos as u32).unwrap();
            if (prompt.len()..prompt.len() + expected.len()).contains(&pos) {
                assert_eq!(token, expected[pos - prompt.len()]);
            }
        }
        assert!(da.forward(u32::MAX, steps as u32).is_err());
        assert!(da.forward(1, (steps + 1) as u32).is_err());
        assert!(da.forward(1, capacity as u32).is_err());
    }
    assert_eq!(a.len(), steps);
    assert_eq!(b.len(), steps);
    // Drop releases the exclusive borrows. Prefill may grow eager scratch;
    // recapture must pick up the new KV position.
    let mut pw = model.alloc_workspace(backend, 3).unwrap();
    model
        .prefill(backend, &mut a, &mut pw, &[31, 47, 59], steps as u32)
        .unwrap();
    model
        .prefill(
            backend,
            &mut reference,
            &mut pw,
            &[31, 47, 59],
            steps as u32,
        )
        .unwrap();
    let pos = (steps + 3) as u32;
    let want = model
        .forward(backend, &mut reference, &mut rw, 99, pos)
        .unwrap();
    let mut da = capture(backend, model, &mut a, &mut aw).unwrap();
    assert_eq!(da.forward(99, pos).unwrap(), want);
}

pub(crate) fn concurrent_prefill_replay<B: Backend + Sync>(
    backend: &B,
    model: &Model<B>,
    capture: Capture<B>,
    capacity: usize,
    prompt: &[u32],
    expected: &[u32],
) where
    B::Buf: Sync,
{
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut kv = model.new_kv_cache(backend, capacity).unwrap();
                    let mut pw = model.alloc_workspace(backend, prompt.len()).unwrap();
                    let mut ws = model.alloc_workspace(backend, 1).unwrap();
                    let mut logits = model.prefill(backend, &mut kv, &mut pw, prompt, 0).unwrap();
                    drop(pw);
                    barrier.wait();
                    let mut decoder = capture(backend, model, &mut kv, &mut ws).unwrap();
                    assert!(decoder.is_graph(), "concurrent capture fell back to eager");
                    for (i, &expected) in expected.iter().enumerate() {
                        let next = greedy(&logits);
                        assert_eq!(next, expected);
                        logits = decoder.forward(next, (prompt.len() + i) as u32).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    });
}

#[cfg(test)]
mod tests;
