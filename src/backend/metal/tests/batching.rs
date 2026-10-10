use super::*;
use crate::{gguf::GgufModel, kv_cache::KvCache, model::{Model, Workspace, decoder::Decoder}};

fn capture<'a>(
    backend: &'a MetalBackend,
    model: &'a Model<MetalBackend>,
    kv: &'a mut KvCache<MetalBackend>,
    ws: &'a mut Workspace<MetalBackend>,
) -> Result<Box<dyn Decoder + 'a>> {
    // Exercise batching independently of the process-wide fallback setting.
    Ok(Box::new(batch::BatchedDecoder::new(backend, model, kv, ws)?))
}

#[test]
fn batched_decoders_match_eager_with_interleaved_requests_and_errors() {
    let tiny = crate::model::tests::tiny_model(gpu());
    crate::backend::test_support::graph::replay_matches_eager(
        gpu(), &tiny, capture, &[1, 2, 3], &[], 6, 16);
    let f = crate::model::compat_tests::decoder_fixture(true);
    let model = Model::load(gpu(), &mut GgufModel::open(&f.0).unwrap()).unwrap();
    compare_requests(&model, 3);
}

fn compare_requests(model: &Model<MetalBackend>, seed: u32) {
    let mut kv = model.new_kv_cache(gpu(), 16).unwrap();
    let mut reference = model.new_kv_cache(gpu(), 16).unwrap();
    let mut ws = model.alloc_workspace(gpu(), 1).unwrap();
    let mut rw = model.alloc_workspace(gpu(), 1).unwrap();
    let mut other = model.new_kv_cache(gpu(), 16).unwrap();
    let mut ow = model.alloc_workspace(gpu(), 1).unwrap();
    let mut other_reference = model.new_kv_cache(gpu(), 16).unwrap();
    let mut orw = model.alloc_workspace(gpu(), 1).unwrap();
    {
        let mut decoder = capture(gpu(), model, &mut kv, &mut ws).unwrap();
        let mut second = capture(gpu(), model, &mut other, &mut ow).unwrap();
        assert!(!decoder.is_graph(), "command batching is not graph replay");
        for pos in 0..8 {
            let want = model.forward(gpu(), &mut reference, &mut rw, seed + pos, pos).unwrap();
            assert_eq!(decoder.forward(seed + pos, pos).unwrap(), want);
            if pos % 2 == 0 {
                let p = pos / 2;
                let want = model.forward(gpu(), &mut other_reference, &mut orw, 20 + p, p).unwrap();
                assert_eq!(second.forward(20 + p, p).unwrap(), want);
            }
        }
        assert!(decoder.forward(u32::MAX, 8).is_err());
        assert!(decoder.forward(3, 9).is_err());
        assert!(decoder.forward(3, 16).is_err());
    }
    assert_eq!(kv.len(), 8);
    assert_eq!(other.len(), 4);
    let mut prefill = model.alloc_workspace(gpu(), 3).unwrap();
    model.prefill(gpu(), &mut kv, &mut prefill, &[3, 4, 5], 8).unwrap();
    model.prefill(gpu(), &mut reference, &mut prefill, &[3, 4, 5], 8).unwrap();
    let want = model.forward(gpu(), &mut reference, &mut rw, 6, 11).unwrap();
    assert_eq!(capture(gpu(), model, &mut kv, &mut ws).unwrap().forward(6, 11).unwrap(), want);
}

#[test]
fn concurrent_batches_keep_both_architectures_isolated() {
    for gemma in [false, true] {
        let f = crate::model::compat_tests::decoder_fixture(gemma);
        let model = Model::load(gpu(), &mut GgufModel::open(&f.0).unwrap()).unwrap();
        std::thread::scope(|scope| {
            let a = scope.spawn(|| compare_requests(&model, 3));
            let b = scope.spawn(|| compare_requests(&model, 17));
            a.join().unwrap();
            b.join().unwrap();
        });
    }
}

#[test]
fn host_transfers_join_referenced_work_and_retain_temporaries() {
    let backend = gpu().request_backend().unwrap();
    let mut a = fbuf(&backend, &[1.0; 67]);
    let b = fbuf(&backend, &[2.0; 67]);
    let mut y = fbuf(&backend, &[0.0; 67]);
    backend.add(&a, &b, &mut y).unwrap();
    // New allocations can be uploaded without splitting the pending batch.
    let unrelated = fbuf(&backend, &[9.0; 67]);
    assert!(backend.pending.as_ref().unwrap().lock().unwrap().is_some());
    backend.upload(&mut a, 0, bytemuck::cast_slice(&[4.0f32; 67])).unwrap();
    assert!(backend.pending.as_ref().unwrap().lock().unwrap().is_none());
    assert_eq!(read(&backend, &y, 67), vec![3.0; 67]);
    backend.add(&a, &unrelated, &mut y).unwrap();
    drop(a);
    drop(unrelated);
    assert_eq!(read(&backend, &y, 67), vec![13.0; 67]);

    let table = weight(&backend, GgmlDType::F32, 3, 67, &packed(GgmlDType::F32, 3, 67));
    let mut embedded = fbuf(&backend, &[0.0; 67 * 3]);
    backend.embed(&table, &[2, 0, 2], &mut embedded).unwrap();
    backend.scale(&mut embedded, 2.0).unwrap();
    // A second embedding allocates/uploads IDs while earlier work is pending.
    let mut second = fbuf(&backend, &[0.0; 67]);
    backend.embed(&table, &[1], &mut second).unwrap();
    assert!(backend.pending.as_ref().unwrap().lock().unwrap().is_some());
    let source = data(3 * 67, 7);
    let want: Vec<_> = [2, 0, 2].iter().flat_map(|&r| source[r * 67..(r + 1) * 67].iter().map(|v| v * 2.0)).collect();
    assert_eq!(read(&backend, &embedded, 3 * 67), want);
    assert_eq!(read(&backend, &second, 67), &source[67..134]);
}

#[test]
fn unsubmitted_batches_cancel_on_unwind_without_writing_outputs() {
    let a = fbuf(gpu(), &[1.0; 67]);
    let b = fbuf(gpu(), &[2.0; 67]);
    let mut y = fbuf(gpu(), &[123.0; 67]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let backend = gpu().request_backend().unwrap();
        backend.add(&a, &b, &mut y).unwrap();
        panic!("cancel request before submission");
    }));
    assert!(result.is_err());
    assert_eq!(read(gpu(), &y, 67), vec![123.0; 67]);
}
