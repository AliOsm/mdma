use super::*;
use std::assert_matches;
use crate::backend::cpu::{CpuBackend, CpuBuf};
use crate::backend::{AttnSpec, KvView, Pool, QWeight, RopeSpec};
use crate::gguf::tests::Fixture;
use crate::memory::NativeMemoryBudget;
use crate::model::{Model, Workspace};
use crate::kv_cache::KvCache;
use std::cell::{Cell, RefCell};

#[derive(Default)]
struct CountingBackend {
    allocations: Cell<usize>,
    fail_alloc_at: Cell<Option<usize>>,
    batches: RefCell<Vec<usize>>,
    downloads: Cell<usize>,
    factories: RefCell<Vec<(usize, usize)>>,
    positions: RefCell<Vec<u32>>,
    fail_factory: Cell<bool>,
    fail_decode_at: Cell<Option<usize>>,
    fail_download_at: Cell<Option<usize>>,
    nonfinite_at: Cell<Option<usize>>,
}

struct CountingDecoder<'a> {
    backend: &'a CountingBackend,
    model: &'a Model<CountingBackend>,
    kv: &'a mut KvCache<CountingBackend>,
    ws: &'a mut Workspace<CountingBackend>,
}

impl crate::model::decoder::Decoder for CountingDecoder<'_> {
    fn forward(&mut self, token: u32, pos: u32) -> backend::Result<Vec<f32>> {
        self.backend.positions.borrow_mut().push(pos);
        if self.backend.fail_decode_at.get() == Some(self.backend.positions.borrow().len()) {
            return Err(backend::Error::Backend("injected decode failure".into()));
        }
        self.model.forward(self.backend, self.kv, self.ws, token, pos)
    }
}

impl Backend for CountingBackend {
    type Buf = CpuBuf;

    fn decoder<'a>(
        &'a self, model: &'a Model<Self>, kv: &'a mut KvCache<Self>, ws: &'a mut Workspace<Self>,
    ) -> backend::Result<Box<dyn crate::model::decoder::Decoder + 'a>> {
        self.factories.borrow_mut().push((kv.capacity(), kv.len()));
        if self.fail_factory.get() {
            return Err(backend::Error::Backend("injected factory failure".into()));
        }
        Ok(Box::new(CountingDecoder { backend: self, model, kv, ws }))
    }

    fn alloc(&self, bytes: usize, pool: Pool) -> backend::Result<CpuBuf> {
        self.allocations.set(self.allocations.get() + 1);
        if self.fail_alloc_at.get() == Some(self.allocations.get()) {
            return Err(backend::Error::OutOfMemory("injected warm-up allocation race".into()));
        }
        CpuBackend.alloc(bytes, pool)
    }
    fn upload(&self, dst: &mut CpuBuf, offset: usize, src: &[u8]) -> backend::Result<()> {
        CpuBackend.upload(dst, offset, src)
    }
    fn download(&self, src: &CpuBuf, offset: usize, dst: &mut [u8]) -> backend::Result<()> {
        let count = self.downloads.get() + 1;
        self.downloads.set(count);
        if self.fail_download_at.get() == Some(count) {
            return Err(backend::Error::Backend("injected readback failure".into()));
        }
        CpuBackend.download(src, offset, dst)?;
        if self.nonfinite_at.get() == Some(count) {
            bytemuck::cast_slice_mut::<u8, f32>(dst)[0] = f32::NAN;
        }
        Ok(())
    }
    fn copy(&self, src: &CpuBuf, a: usize, dst: &mut CpuBuf, b: usize, n: usize) -> backend::Result<()> {
        CpuBackend.copy(src, a, dst, b, n)
    }
    fn qmatmul(&self, w: &QWeight<CpuBuf>, x: &CpuBuf, y: &mut CpuBuf) -> backend::Result<()> {
        CpuBackend.qmatmul(w, x, y)
    }
    fn embed(&self, w: &QWeight<CpuBuf>, ids: &[u32], y: &mut CpuBuf) -> backend::Result<()> {
        self.batches.borrow_mut().push(ids.len());
        CpuBackend.embed(w, ids, y)
    }
    fn rmsnorm(&self, w: &CpuBuf, x: &CpuBuf, y: &mut CpuBuf, eps: f32) -> backend::Result<()> {
        CpuBackend.rmsnorm(w, x, y, eps)
    }
    fn add(&self, a: &CpuBuf, b: &CpuBuf, y: &mut CpuBuf) -> backend::Result<()> {
        CpuBackend.add(a, b, y)
    }
    fn rope(&self, q: &mut CpuBuf, k: &mut CpuBuf, pos: u32, n: usize, spec: &RopeSpec) -> backend::Result<()> {
        CpuBackend.rope(q, k, pos, n, spec)
    }
    fn attn(&self, q: &CpuBuf, kv: &KvView<CpuBuf>, spec: &AttnSpec, y: &mut CpuBuf) -> backend::Result<()> {
        CpuBackend.attn(q, kv, spec, y)
    }
    fn silu_mul(&self, a: &CpuBuf, b: &CpuBuf, y: &mut CpuBuf) -> backend::Result<()> {
        CpuBackend.silu_mul(a, b, y)
    }
}

fn registry(backend: &CountingBackend, n_ctx: usize, budget: Option<&NativeMemoryBudget>) -> Registry<CountingBackend> {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = super::super::ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models[0].name = "fixture".into();
    Registry::load_with_budget(backend, &config, n_ctx, budget, |_| {}).unwrap()
}

#[test]
fn bounded_rounds_join_prefill_and_use_the_request_decoder_factory() {
    let backend = CountingBackend::default();
    let registry = registry(&backend, 4096, None);
    assert!(registry.warmup(&backend, 2, || false).unwrap());
    assert_eq!(*backend.batches.borrow(), [16, 1, 1, 1, 1, 16, 1, 1, 1, 1]);
    assert_eq!(*backend.factories.borrow(), [(20, 16), (20, 16)]);
    assert_eq!(*backend.positions.borrow(), [16, 17, 18, 19, 16, 17, 18, 19]);
    assert_eq!(backend.downloads.get(), 10); // Every operation returns completed host logits.
}

#[test]
fn small_contexts_keep_one_prefill_position_and_never_overrun() {
    for capacity in [1, 2, 5, 19, 20, 64] {
        let backend = CountingBackend::default();
        let registry = registry(&backend, capacity, None);
        assert!(registry.warmup(&backend, 1, || false).unwrap());
        let cap = capacity.min(20);
        let steps = 4.min(cap - 1);
        assert_eq!(backend.batches.borrow()[0], cap - steps);
        assert_eq!(backend.downloads.get(), 1 + steps);
        assert_eq!(*backend.positions.borrow(), ((cap - steps)..cap).map(|p| p as u32).collect::<Vec<_>>());
        assert_eq!(backend.factories.borrow().len(), usize::from(steps > 0));
    }
}

#[test]
fn aliases_warm_once_using_the_largest_context_policy() {
    let backend = CountingBackend::default();
    let mut registry = registry(&backend, 1, None);
    let Entry::Decoder(first) = &registry.models[0].entry else { unreachable!() };
    let alias = Decoder {
        model: first.model.clone(), tokenizer: first.tokenizer.clone(),
        architecture: first.architecture.clone(), n_ctx: 64,
    };
    registry.models.push(super::super::Loaded {
        name: "alias".into(), kind: super::super::Kind::Gguf, weight_bytes: 0,
        entry: Entry::Decoder(alias),
    });
    assert!(registry.warmup(&backend, 2, || false).unwrap());
    assert_eq!(backend.downloads.get(), 10);
    assert_eq!(*backend.factories.borrow(), [(20, 16), (20, 16)]);
}

#[test]
fn disabled_and_precancelled_warmup_allocate_nothing() {
    let backend = CountingBackend::default();
    let registry = registry(&backend, 64, None);
    let before = backend.allocations.get();
    assert!(registry.warmup(&backend, 0, || false).unwrap());
    assert!(!registry.warmup(&backend, 2, || true).unwrap());
    assert!(!registry.warmup(&backend, 0, || true).unwrap());
    assert!(registry.warmup(&backend, MAX_WARMUP_ROUNDS + 1, || false).is_err());
    assert_eq!(backend.allocations.get(), before);
    assert!(backend.batches.borrow().is_empty());
}

#[test]
fn cancellation_between_prefill_and_decode_does_not_construct_a_decoder() {
    let backend = CountingBackend::default();
    let budget = NativeMemoryBudget::new(1_000_000);
    let registry = registry(&backend, 64, Some(&budget));
    let weights = budget.used();
    assert!(!registry.warmup(&backend, 2, || backend.downloads.get() > 0).unwrap());
    assert_eq!(backend.downloads.get(), 1);
    assert!(backend.factories.borrow().is_empty());
    assert_eq!(budget.used(), weights);
}

#[test]
fn cancellation_during_decode_stops_before_the_next_step_and_refunds_buffers() {
    let backend = CountingBackend::default();
    let budget = NativeMemoryBudget::new(1_000_000);
    let registry = registry(&backend, 64, Some(&budget));
    let weights = budget.used();
    assert!(!registry.warmup(&backend, 2, || !backend.positions.borrow().is_empty()).unwrap());
    assert_eq!(*backend.positions.borrow(), [16]);
    assert_eq!(backend.downloads.get(), 2);
    assert_eq!(budget.used(), weights);
}

#[test]
fn failures_are_named_and_temporary_reservations_are_refunded() {
    for failure in ["readback", "factory", "decode", "nonfinite_prefill", "nonfinite_decode"] {
        let backend = CountingBackend::default();
        let budget = NativeMemoryBudget::new(1_000_000);
        let registry = registry(&backend, 64, Some(&budget));
        let weights = budget.used();
        match failure {
            "readback" => backend.fail_download_at.set(Some(1)),
            "factory" => backend.fail_factory.set(true),
            "decode" => backend.fail_decode_at.set(Some(2)),
            "nonfinite_prefill" => backend.nonfinite_at.set(Some(1)),
            "nonfinite_decode" => backend.nonfinite_at.set(Some(2)),
            _ => unreachable!(),
        }
        let error = registry.warmup(&backend, 2, || false).unwrap_err().to_string();
        assert!(error.contains("model fixture"), "{error}");
        assert!(error.contains("failure") || error.contains("nonfinite"), "{error}");
        assert_eq!(budget.used(), weights, "{failure}");
    }
}

#[test]
fn allocation_failure_does_not_ignore_the_native_tensor_budget() {
    let backend = CountingBackend::default();
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let source = crate::gguf::GgufModel::open(file.path()).unwrap();
    let weights = crate::model::memory::weight_bytes(&source).unwrap();
    let budget = NativeMemoryBudget::new(weights + 1);
    let registry = registry(&backend, 64, Some(&budget));
    let error = registry.warmup(&backend, 2, || false).unwrap_err().to_string();
    assert!(error.contains("model fixture") && error.contains("memory budget exhausted"), "{error}");
    assert_eq!(budget.used(), weights);
    assert_eq!(backend.downloads.get(), 0);
}

#[test]
fn daemon_warmup_releases_all_aliases_on_memory_exhaustion() {
    let backend = CountingBackend::default();
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let source = crate::gguf::GgufModel::open(file.path()).unwrap();
    let weights = crate::model::memory::weight_bytes(&source).unwrap();
    let budget = NativeMemoryBudget::new(weights + 1);
    let mut config = super::super::ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models.push(config.models[0].clone());
    config.models[1].name = "alias".into();
    config.models[1].n_ctx = Some(4);
    let mut registry = Registry::load_available_with_budget(&backend, &config, 64, Some(&budget), |_| {}).unwrap();
    assert_eq!(budget.used(), weights);
    assert!(registry.warmup_available(&backend, 2, || false).unwrap());
    assert_eq!(budget.used(), 0, "no alias should keep rejected weights resident");
    assert_eq!(backend.downloads.get(), 0);
    for model in registry.iter() {
        let Entry::Unavailable(status) = &model.entry else { panic!("model must be skipped"); };
        assert!(status.reason.contains("warm-up: native tensor memory budget exhausted"));
    }
    let Entry::Unavailable(alias) = &registry.get("alias").unwrap().entry else { unreachable!() };
    assert_eq!(alias.n_ctx, 4);
    assert!(registry.warmup_available(&backend, 2, || false).unwrap(), "skipped entries allocate nothing");
    assert_eq!(budget.used(), 0);
}

#[test]
fn warmup_withdraws_frozen_identity_for_every_unavailable_alias() {
    let backend = CountingBackend::default();
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let source = crate::gguf::GgufModel::open(file.path()).unwrap();
    let weights = crate::model::memory::weight_bytes(&source).unwrap();
    let budget = NativeMemoryBudget::new(weights + 1);
    let mut config = super::super::ModelsFile::single_gguf(file.path().to_str().unwrap());
    config.models.push(config.models[0].clone());
    config.models[1].name = "alias".into();
    let mut registry = Registry::load_available_frozen_with_budget(&backend, &config, 64, Some(&budget), |_| {}).unwrap();
    assert!(registry.artifact_id("").is_some());
    assert_eq!(registry.artifact_id(""), registry.artifact_id("alias"));
    assert!(registry.warmup_available(&backend, 2, || false).unwrap());
    assert!(registry.artifact_id("").is_none());
    assert!(registry.artifact_id("alias").is_none());
    assert_eq!(budget.used(), 0);
}

#[test]
fn daemon_warmup_keeps_nonmemory_failures_and_cancellation_fatal_to_readiness() {
    let backend = CountingBackend::default();
    let mut registry = registry(&backend, 64, None);
    assert!(!registry.warmup_available(&backend, 2, || true).unwrap());
    backend.fail_factory.set(true);
    let error = registry.warmup_available(&backend, 2, || false).unwrap_err();
    assert!(error.to_string().contains("injected factory failure"));
    assert_matches!(&registry.get("").unwrap().entry, Entry::Decoder(_));
}

#[test]
fn daemon_warmup_continues_other_models_after_a_partial_allocation_oom() {
    let backend = CountingBackend::default();
    let a = Fixture::new(1, 8).with_tokenizer().write();
    let b = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = super::super::ModelsFile::single_gguf(a.path().to_str().unwrap());
    config.models[0].name = "a".into();
    config.models.push(config.models[0].clone());
    config.models[1].name = "alias".into();
    let mut second = config.models[0].clone();
    second.name = "b".into();
    second.gguf = Some(b.path().to_str().unwrap().into());
    config.models.push(second);
    let budget = NativeMemoryBudget::new(1_000_000);
    let mut registry = Registry::load_available_with_budget(&backend, &config, 64, Some(&budget), |_| {}).unwrap();
    let weights = budget.used() / 2;
    backend.fail_alloc_at.set(Some(backend.allocations.get() + 3));
    assert!(registry.warmup_available(&backend, 2, || false).unwrap());
    for name in ["a", "alias"] {
        assert_matches!(&registry.get(name).unwrap().entry, Entry::Unavailable(_));
    }
    assert_matches!(&registry.get("b").unwrap().entry, Entry::Decoder(_));
    assert_eq!(budget.used(), weights);
    assert_eq!(backend.downloads.get(), 10, "remaining model still completes both rounds");
    drop(registry);
    assert_eq!(budget.used(), 0);
}

fn continuation(backend: &CountingBackend, registry: &Registry<CountingBackend>) -> Vec<Vec<f32>> {
    let Entry::Decoder(loaded) = &registry.models[0].entry else { unreachable!() };
    let model = &loaded.model;
    let mut kv = model.new_kv_cache(backend, 16).unwrap();
    let mut prefill_ws = model.alloc_workspace(backend, 3).unwrap();
    let mut ws = model.alloc_workspace(backend, 1).unwrap();
    let mut logits = model.prefill(backend, &mut kv, &mut prefill_ws, &[1, 2, 3], 0).unwrap();
    let mut outputs = vec![logits.clone()];
    for pos in 3..8 {
        logits = model.forward(backend, &mut kv, &mut ws, greedy(&logits), pos).unwrap();
        outputs.push(logits.clone());
    }
    outputs
}

#[test]
fn warmup_keeps_weights_resident_without_seeding_user_kv_or_changing_logits() {
    let backend = CountingBackend::default();
    let budget = NativeMemoryBudget::new(1_000_000);
    let registry = registry(&backend, 64, Some(&budget));
    let weights = budget.used();
    let baseline = continuation(&backend, &registry);
    assert!(registry.warmup(&backend, 2, || false).unwrap());
    assert_eq!(budget.used(), weights);
    assert_eq!(continuation(&backend, &registry), baseline);
}

#[test]
fn empty_or_nonfinite_logits_are_not_ready() {
    for logits in [vec![], vec![f32::NAN], vec![f32::INFINITY], vec![f32::NEG_INFINITY]] {
        assert!(next_token(&logits).is_err());
    }
    assert_eq!(next_token(&[-2.0, 3.0, 1.0]).unwrap(), 1);
}
