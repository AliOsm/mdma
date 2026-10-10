use super::*;
use std::assert_matches;
use crate::backend::cpu::CpuBackend;
use crate::backend::{AttnSpec, KvView, Pool, QWeight, RopeSpec};
use crate::gguf::tests::Fixture;
use crate::registry::{Entry, ModelSpec, Registry};
use crate::sampler::SamplerConfig;
use crate::session::Session;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

// Loader-only backend: tracks buffer lifetime and injects failures; no kernels.
#[derive(Default)]
struct LoadBackend {
    allocations: Cell<usize>,
    uploads: Cell<usize>,
    live_bytes: Arc<AtomicUsize>,
    fail_upload: Cell<Option<usize>>,
    fail_memory_upload: Cell<Option<usize>>,
    free_device_bytes: Cell<Option<usize>>,
    on_upload: RefCell<Option<Box<dyn FnOnce()>>>,
}

struct Buffer {
    bytes: Vec<u8>,
    live_bytes: Arc<AtomicUsize>,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.live_bytes.fetch_sub(self.bytes.len(), SeqCst);
    }
}

impl Backend for LoadBackend {
    type Buf = Buffer;

    fn device_label(&self) -> Option<String> { Some("CPU".into()) }

    fn available_device_bytes(&self) -> backend::Result<Option<usize>> {
        Ok(self.free_device_bytes.get().map(|bytes| bytes.saturating_sub(self.live_bytes.load(SeqCst))))
    }

    fn alloc(&self, bytes: usize, pool: Pool) -> backend::Result<Buffer> {
        assert_matches!(pool, Pool::Weights);
        self.allocations.set(self.allocations.get() + 1);
        self.live_bytes.fetch_add(bytes, SeqCst);
        Ok(Buffer { bytes: vec![0; bytes], live_bytes: self.live_bytes.clone() })
    }
    fn upload(&self, dst: &mut Buffer, offset: usize, src: &[u8]) -> backend::Result<()> {
        let count = self.uploads.get() + 1;
        self.uploads.set(count);
        if self.fail_memory_upload.get() == Some(count) {
            return Err(backend::Error::OutOfMemory("injected allocation race".into()));
        }
        if self.fail_upload.get() == Some(count) {
            return Err(backend::Error::Backend("injected upload failure".into()));
        }
        dst.bytes[offset..offset + src.len()].copy_from_slice(src);
        if let Some(hook) = self.on_upload.borrow_mut().take() {
            hook();
        }
        Ok(())
    }
    fn download(&self, _: &Buffer, _: usize, _: &mut [u8]) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn copy(&self, _: &Buffer, _: usize, _: &mut Buffer, _: usize, _: usize) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn qmatmul(&self, _: &QWeight<Buffer>, _: &Buffer, _: &mut Buffer) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn embed(&self, _: &QWeight<Buffer>, _: &[u32], _: &mut Buffer) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn rmsnorm(&self, _: &Buffer, _: &Buffer, _: &mut Buffer, _: f32) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn add(&self, _: &Buffer, _: &Buffer, _: &mut Buffer) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn rope(&self, _: &mut Buffer, _: &mut Buffer, _: u32, _: usize, _: &RopeSpec) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn attn(&self, _: &Buffer, _: &KvView<Buffer>, _: &AttnSpec, _: &mut Buffer) -> backend::Result<()> { unreachable!("loader-only backend") }
    fn silu_mul(&self, _: &Buffer, _: &Buffer, _: &mut Buffer) -> backend::Result<()> { unreachable!("loader-only backend") }
}

fn config(paths: &[&Path]) -> ModelsFile {
    ModelsFile {
        default: None,
        models: paths.iter().enumerate().map(|(i, path)| ModelSpec {
            name: format!("m{i}"), gguf: Some(path.to_str().unwrap().into()), laya: None, upstream: None, n_ctx: None,
        }).collect(),
    }
}

fn decoder<'a, B: Backend>(registry: &'a Registry<B>, name: &str) -> &'a Decoder<B> {
    match &registry.get(name).unwrap().entry {
        Entry::Decoder(decoder) => decoder,
        _ => panic!("expected decoder"),
    }
}

fn change_stamp(file: &File) {
    let modified = file.metadata().unwrap().modified().unwrap();
    file.set_modified(modified + Duration::from_secs(1)).unwrap();
}

#[test]
fn frozen_aliases_keep_exact_digest_and_one_upload_when_original_changes_during_upload() {
    use sha2::{Digest, Sha256};
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let expected = format!("sha256:{}", hex::encode(Sha256::digest(std::fs::read(file.path()).unwrap())));
    let mut config = config(&[file.path(), file.path()]);
    config.models[1].n_ctx = Some(8);
    let backend = LoadBackend::default();
    let path = file.path().to_owned();
    *backend.on_upload.borrow_mut() = Some(Box::new(move || std::fs::write(path, b"replaced").unwrap()));
    let mut callbacks = 0;
    let registry = Registry::load_available_frozen_with_budget(&backend, &config, 32, None, |_| callbacks += 1).unwrap();
    assert_eq!(callbacks, 1);
    assert_eq!(registry.artifact_id("").unwrap().as_str(), expected);
    assert_eq!(registry.artifact_id("m1"), registry.artifact_id("m0"));
    assert!(registry.artifact_id("missing").is_none());
    assert!(Arc::ptr_eq(&decoder(&registry, "m0").model, &decoder(&registry, "m1").model));
    assert_eq!(decoder(&registry, "m1").n_ctx, 8);
}

#[test]
fn frozen_load_uses_original_payload_even_after_path_replacement() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let original = Registry::load(&CpuBackend, &config(&[file.path()]), 32, |_| {}).unwrap();
    assert!(original.artifact_id("").is_none());
    let mut sources = Sources::prepare_frozen(&CpuBackend, &config(&[file.path()])).unwrap();
    let mut replacement = Fixture::new(1, 8).with_tokenizer();
    replacement.replace("output.weight", &[128, 8], crate::gguf::GgmlDType::F32);
    let replacement = replacement.write();
    std::fs::copy(replacement.path(), file.path()).unwrap();
    let (_, captured) = sources.decoder(0, 32, &mut |_| {}).unwrap();
    let replacement = Registry::load(&CpuBackend, &config(&[file.path()]), 32, |_| {}).unwrap();
    let forward = |model: &Model<CpuBackend>| {
        let mut kv = model.new_kv_cache(&CpuBackend, 1).unwrap();
        let mut ws = model.alloc_workspace(&CpuBackend, 1).unwrap();
        model.forward(&CpuBackend, &mut kv, &mut ws, 1, 0).unwrap()
    };
    assert_eq!(forward(&captured.model), forward(&decoder(&original, "").model));
    assert_ne!(forward(&captured.model), forward(&decoder(&replacement, "").model));
    sources.check_unchanged().unwrap();
    assert!(sources.artifact_id(0).is_some());
    assert!(Registry::single("unproven", captured).artifact_id("").is_none());
}

#[test]
fn complete_gguf_digest_includes_unconsumed_trailing_bytes() {
    use std::io::{Seek, Write};
    let mut file = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[file.path()]);
    let first = Registry::load_available_frozen_with_budget(&CpuBackend, &config, 32, None, |_| {}).unwrap();
    file.as_file_mut().seek(std::io::SeekFrom::End(0)).unwrap();
    file.write_all(b"whole-artifact-trailer").unwrap();
    let second = Registry::load_available_frozen_with_budget(&CpuBackend, &config, 32, None, |_| {}).unwrap();
    assert_ne!(first.artifact_id(""), second.artifact_id(""));
}

#[test]
fn frozen_unavailable_models_have_no_publishable_identity() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let budget = NativeMemoryBudget::new(0);
    let registry = Registry::load_available_frozen_with_budget(&CpuBackend, &config(&[file.path(), file.path()]), 32, Some(&budget), |_| panic!("no calibration")).unwrap();
    assert!(registry.artifact_id("m0").is_none());
    assert!(registry.artifact_id("m1").is_none());
}

#[test]
fn native_cpu_profiles_share_alias_modes_but_never_profile_unfrozen_or_library_entries() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = config(&[file.path(), file.path()]);
    config.models[1].n_ctx = Some(8);
    let mut registry = Registry::load_available_frozen_with_budget(&CpuBackend, &config, 32, None, |_| {}).unwrap();
    assert!(registry.execution_profile("").is_none());
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 2);
    let profile = registry.execution_profile("").unwrap();
    assert!(profile.is_current());
    assert_eq!(profile.as_str(), registry.execution_profile("m1").unwrap().as_str());
    assert_eq!(profile.manifest().as_object().unwrap()["kv_dtype"].as_str(), Some("f32"));
    assert!(registry.execution_profile("missing").is_none());
    let d = decoder(&registry, "");
    let mut single = Registry::single("library", Decoder {
        model: d.model.clone(), tokenizer: d.tokenizer.clone(),
        architecture: d.architecture.clone(), n_ctx: d.n_ctx,
    });
    assert_eq!(single.capture_execution_profiles(&CpuBackend).unwrap(), 0);
    assert!(single.execution_profile("").is_none());
    let mut normal = Registry::load(&CpuBackend, &config, 32, |_| {}).unwrap();
    assert_eq!(normal.capture_execution_profiles(&CpuBackend).unwrap(), 0);
    assert!(normal.execution_profile("").is_none());
}

#[test]
fn a_backend_calling_itself_cpu_does_not_get_the_native_cpu_profile() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let backend = LoadBackend::default();
    assert_eq!(backend.device_label().as_deref(), Some("CPU"));
    let mut registry = Registry::load_available_frozen_with_budget(&backend, &config(&[file.path()]), 32, None, |_| {}).unwrap();
    assert!(registry.artifact_id("").is_some());
    assert_eq!(registry.capture_execution_profiles(&backend).unwrap(), 0);
    assert!(registry.execution_profile("").is_none());
}

#[test]
fn failed_executor_recapture_clears_prior_alias_profiles_without_unloading_models() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut registry = Registry::load_available_frozen_with_budget(
        &CpuBackend, &config(&[file.path(), file.path()]), 32, None, |_| {},
    ).unwrap();
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 2);
    let error = registry.capture_execution_profiles_with(&CpuBackend, || {
        Err(io::Error::other("running image is unverifiable"))
    }).unwrap_err();
    assert!(error.to_string().contains("unverifiable"));
    for name in ["m0", "m1"] {
        assert!(registry.artifact_id(name).is_some());
        assert!(registry.execution_profile(name).is_none());
        assert!(matches!(registry.get(name).unwrap().entry, Entry::Decoder(_)));
    }
}

#[test]
fn warmup_failure_withdraws_all_cpu_alias_profiles_and_recapture_cannot_revive_them() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let weights = crate::model::memory::weight_bytes(&GgufModel::open(file.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(weights + 1);
    let mut registry = Registry::load_available_frozen_with_budget(
        &CpuBackend, &config(&[file.path(), file.path()]), 32, Some(&budget), |_| {},
    ).unwrap();
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 2);
    assert!(registry.execution_profile("").is_some());
    assert!(registry.warmup_available(&CpuBackend, 1, || false).unwrap());
    assert!(registry.execution_profile("m0").is_none());
    assert!(registry.execution_profile("m1").is_none());
    assert_eq!(registry.capture_execution_profiles(&CpuBackend).unwrap(), 0);
    assert_eq!(budget.used(), 0);
}

#[cfg(any(feature = "vulkan", all(feature = "metal", target_os = "macos")))]
fn frozen_gpu_payload_matches_cpu_after_original_is_removed<B>(backend: &B)
where
    B: Backend + Send + Sync + 'static,
    B::Buf: Send + Sync,
{
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[file.path(), file.path()]);
    let cpu = Registry::load_available_frozen_with_budget(&CpuBackend, &config, 32, None, |_| {}).unwrap();
    let mut gpu = Registry::load_available_frozen_with_budget(backend, &config, 32, None, |_| {}).unwrap();
    assert_eq!(gpu.capture_execution_profiles(backend).unwrap(), 2);
    assert!(gpu.execution_profile("").is_some());
    assert_eq!(gpu.execution_profile("m0").unwrap().as_str(), gpu.execution_profile("m1").unwrap().as_str());
    assert_ne!(gpu.execution_profile("m0").unwrap().manifest().as_object().unwrap()["backend"].as_str(), Some("cpu"));
    assert_eq!(cpu.artifact_id(""), gpu.artifact_id(""));
    assert_eq!(gpu.artifact_id(""), gpu.artifact_id("m1"));
    assert!(Arc::ptr_eq(&decoder(&gpu, "m0").model, &decoder(&gpu, "m1").model));
    std::fs::remove_file(file.path()).unwrap();
    let model = &decoder(&cpu, "").model;
    let mut kv = model.new_kv_cache(&CpuBackend, 1).unwrap();
    let mut ws = model.alloc_workspace(&CpuBackend, 1).unwrap();
    let expected = model.forward(&CpuBackend, &mut kv, &mut ws, 1, 0).unwrap();
    let model = &decoder(&gpu, "").model;
    let mut kv = model.new_kv_cache(backend, 1).unwrap();
    let mut ws = model.alloc_workspace(backend, 1).unwrap();
    let actual = model.forward(backend, &mut kv, &mut ws, 1, 0).unwrap();
    assert_eq!(actual.len(), expected.len());
    for (&actual, &expected) in actual.iter().zip(&expected) {
        assert!(actual.is_finite() && (actual - expected).abs() < 2e-3, "{actual} != {expected}");
    }
    let inventory = crate::federation::inventory::Native {
        registry: Arc::new(gpu), shutdown: None, power: None,
    };
    let snapshot = crate::federation::inventory::Inventory::snapshot(
        &inventory, &["m0".into(), "m1".into()],
    );
    assert_eq!(snapshot.models.len(), 2, "actual current GPU profiles can be cataloged");
    assert!(snapshot.models.iter().all(|m| m.backend != "cpu"));
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
fn frozen_metal_payload_matches_cpu_after_source_removal() {
    let backend = crate::backend::metal::MetalBackend::new().unwrap();
    frozen_gpu_payload_matches_cpu_after_original_is_removed(&backend);
}

#[cfg(all(feature = "metal", target_os = "macos"))]
#[test]
fn metal_profiles_cannot_be_recaptured_or_served_by_a_different_backend_instance() {
    let first = crate::backend::metal::MetalBackend::new().unwrap();
    let second = Arc::new(crate::backend::metal::MetalBackend::new().unwrap());
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut registry = Registry::load_available_frozen_with_budget(&first, &config(&[file.path()]), 32, None, |_| {}).unwrap();
    assert_eq!(registry.capture_execution_profiles(&first).unwrap(), 1);
    assert!(registry.capture_execution_profiles(&*second).is_err());
    assert!(registry.execution_profile("").is_none());
    assert_eq!(registry.capture_execution_profiles(&first).unwrap(), 1);
    let service = crate::grpc::MdmaService::new(second, registry, "metal");
    assert!(service.federation_inventory().snapshot(&["m0".into()]).models.is_empty());
}

#[cfg(feature = "vulkan")]
#[test]
fn frozen_vulkan_payload_matches_cpu_after_source_removal() {
    let backend = crate::backend::vulkan::VulkanBackend::new().unwrap().with_f16_weights(false);
    frozen_gpu_payload_matches_cpu_after_original_is_removed(&backend);
}

#[cfg(feature = "vulkan")]
#[test]
fn vulkan_profile_is_withdrawn_on_mode_change_even_when_restored() {
    let backend = crate::backend::vulkan::VulkanBackend::new().unwrap();
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut registry = Registry::load_available_frozen_with_budget(&backend, &config(&[file.path()]), 32, None, |_| {}).unwrap();
    registry.capture_execution_profiles(&backend).unwrap();
    let before = registry.execution_profile("").unwrap().as_str().to_string();
    let mode = backend.fused_norm();
    backend.set_fused_norm(!mode);
    assert!(registry.execution_profile("").is_none());
    backend.set_fused_norm(mode);
    assert!(registry.execution_profile("").is_none());
    registry.capture_execution_profiles(&backend).unwrap();
    assert_eq!(registry.execution_profile("").unwrap().as_str(), before);
}

#[test]
fn budget_charges_alias_weights_once_until_the_last_lease() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(file.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes);
    let backend = LoadBackend::default();
    let registry = Registry::load_with_budget(&backend, &config(&[file.path(), file.path()]), 32, Some(&budget), |_| {}).unwrap();
    assert_eq!(budget.used(), bytes);
    assert_eq!(budget.used(), backend.live_bytes.load(SeqCst));
    let lease = decoder(&registry, "m1").model.clone();
    drop(registry);
    assert_eq!(budget.used(), bytes);
    drop(lease);
    assert_eq!(budget.used(), 0);
}

#[test]
fn daemon_skips_device_pressure_before_upload_and_preserves_alias_policies() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(file.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes);
    let backend = LoadBackend::default();
    backend.free_device_bytes.set(Some(bytes + DEVICE_HEADROOM - 1));
    let mut config = config(&[file.path(), file.path()]);
    config.default = Some("m1".into());
    config.models[0].n_ctx = Some(4);
    let registry = Registry::load_available_with_budget(&backend, &config, 32, Some(&budget), |_| panic!("no calibration")).unwrap();
    assert_eq!(registry.names(), ["m0", "m1"]);
    assert_eq!(registry.default_model().name, "m1");
    for (name, n_ctx) in [("m0", 4), ("m1", 32)] {
        let Entry::Unavailable(model) = &registry.get(name).unwrap().entry else { panic!("model must be skipped"); };
        assert_eq!((model.architecture.as_str(), model.n_ctx), ("internlm2", n_ctx));
        assert!(model.reason.contains("insufficient device memory"));
    }
    assert_eq!((backend.allocations.get(), backend.uploads.get(), budget.used()), (0, 0, 0));
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn daemon_skips_a_large_model_and_loads_a_later_model_that_fits() {
    let large = Fixture::new(2, 8).with_tokenizer().write();
    let small = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(small.path()).unwrap()).unwrap();
    let backend = LoadBackend::default();
    backend.free_device_bytes.set(Some(bytes + DEVICE_HEADROOM));
    let registry = Registry::load_available_with_budget(&backend, &config(&[large.path(), small.path()]), 32, None, |_| {}).unwrap();
    assert_matches!(&registry.get("m0").unwrap().entry, Entry::Unavailable(_));
    assert_matches!(&registry.get("m1").unwrap().entry, Entry::Decoder(_));
    assert_eq!(backend.live_bytes.load(SeqCst), bytes);
    assert_eq!(backend.uploads.get(), 12);
    assert_eq!(registry.get("").unwrap().name, "m0", "never silently change the requested default");
}

#[test]
fn daemon_admits_unique_weights_individually_without_overcharging_aliases() {
    let a = Fixture::new(1, 8).with_tokenizer().write();
    let b = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(a.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes);
    let backend = LoadBackend::default();
    let registry = Registry::load_available_with_budget(&backend, &config(&[a.path(), b.path(), a.path()]), 32, Some(&budget), |_| {}).unwrap();
    assert_matches!(&registry.get("m1").unwrap().entry, Entry::Unavailable(_));
    assert!(Arc::ptr_eq(&decoder(&registry, "m0").model, &decoder(&registry, "m2").model));
    assert_eq!((budget.used(), backend.live_bytes.load(SeqCst), backend.uploads.get()), (bytes, bytes, 12));
    drop(registry);
    assert_eq!((budget.used(), backend.live_bytes.load(SeqCst)), (0, 0));
}

#[test]
fn daemon_refunds_partial_oom_upload_and_does_not_retry_its_aliases() {
    let a = Fixture::new(1, 8).with_tokenizer().write();
    let b = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(a.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes);
    let backend = LoadBackend::default();
    backend.fail_memory_upload.set(Some(3));
    let mut callbacks = 0;
    let registry = Registry::load_available_with_budget(&backend, &config(&[a.path(), a.path(), b.path()]), 32, Some(&budget), |_| callbacks += 1).unwrap();
    for name in ["m0", "m1"] {
        let Entry::Unavailable(model) = &registry.get(name).unwrap().entry else { panic!("model must be skipped"); };
        assert!(model.reason.contains("injected allocation race"));
    }
    assert_matches!(&registry.get("m2").unwrap().entry, Entry::Decoder(_));
    assert_eq!((budget.used(), backend.live_bytes.load(SeqCst), backend.uploads.get(), callbacks), (bytes, bytes, 15, 1));
    drop(registry);
    assert_eq!((budget.used(), backend.live_bytes.load(SeqCst)), (0, 0));
}

#[test]
fn daemon_does_not_hide_nonmemory_driver_or_preflight_failures() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let backend = LoadBackend::default();
    backend.fail_upload.set(Some(3));
    let error = Registry::load_available_with_budget(&backend, &config(&[file.path()]), 32, None, |_| {}).err().unwrap();
    assert!(error.to_string().contains("injected upload failure"));
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
    let broken = Fixture::new(1, 8).write();
    let before = backend.uploads.get();
    let error = Registry::load_available_with_budget(&backend, &config(&[file.path(), broken.path()]), 32, None, |_| {}).err().unwrap();
    assert!(error.to_string().contains("failed to build tokenizer"));
    assert_eq!(backend.uploads.get(), before);
}

#[test]
fn budget_rejects_the_whole_unique_weight_set_before_upload() {
    let a = Fixture::new(1, 8).with_tokenizer().write();
    let b = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(a.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes * 2 - 1);
    let backend = LoadBackend::default();
    let error = Registry::load_with_budget(&backend, &config(&[a.path(), a.path(), b.path()]), 32, Some(&budget), |_| panic!("no calibration"))
        .err().unwrap();
    assert!(error.to_string().contains("native tensor memory budget exhausted"));
    assert_eq!(budget.used(), 0);
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (0, 0));
    let registry = Registry::load_with_budget(&backend, &config(&[a.path()]), 32, Some(&budget), |_| {}).unwrap();
    // Sharing the budget, unlike file identity, works across registry loads.
    assert!(Registry::load_with_budget(&backend, &config(&[b.path()]), 32, Some(&budget), |_| {}).is_err());
    assert_eq!(backend.uploads.get(), 12);
    assert_eq!(budget.used(), bytes);
    drop(registry);
    assert_eq!(budget.used(), 0);
}

#[test]
fn budget_rolls_back_loaded_and_unloaded_weights_after_failure() {
    let a = Fixture::new(1, 8).with_tokenizer().write();
    let b = Fixture::new(1, 8).with_tokenizer().write();
    let bytes = crate::model::memory::weight_bytes(&GgufModel::open(a.path()).unwrap()).unwrap();
    let budget = NativeMemoryBudget::new(bytes * 2);
    let backend = LoadBackend::default();
    backend.fail_upload.set(Some(15));
    let config = config(&[a.path(), b.path(), a.path()]);
    assert!(Registry::load_with_budget(&backend, &config, 32, Some(&budget), |_| {}).is_err());
    assert_eq!(budget.used(), 0);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
    backend.fail_upload.set(None);
    let registry = Registry::load_with_budget(&backend, &config, 32, Some(&budget), |_| {}).unwrap();
    assert_eq!(budget.used(), bytes * 2);
    drop(registry);
    assert_eq!(budget.used(), 0);
}

#[test]
fn budget_is_refunded_when_the_calibration_callback_panics() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let budget = NativeMemoryBudget::new(100_000);
    let backend = LoadBackend::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = Registry::load_with_budget(&backend, &config(&[file.path()]), 32, Some(&budget), |_| panic!("injected calibration panic"));
    }));
    assert!(result.is_err());
    assert_eq!(budget.used(), 0);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn aliases_share_one_upload_and_tokenizer_but_keep_names_and_policies() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = config(&[file.path(), file.path(), file.path()]);
    config.default = Some("m2".into());
    config.models[0].n_ctx = Some(4);
    config.models[2].n_ctx = Some(32);
    let certs = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap().self_signed(&key).unwrap();
    let tls_cert = certs.path().join("client.pem");
    let tls_key = certs.path().join("key.pem");
    std::fs::write(&tls_cert, cert.pem()).unwrap();
    std::fs::write(&tls_key, key.serialize_pem()).unwrap();
    config.models.insert(1, ModelSpec {
        name: "upstream".into(), gguf: None, laya: None,
        upstream: Some(crate::upstream::UpstreamConfig {
            url: "https://localhost:1".into(), model: String::new(), tls_ca: tls_cert.clone(), tls_cert, tls_key,
        }),
        n_ctx: None,
    });
    let backend = LoadBackend::default();
    let mut callbacks = 0;
    let registry = Registry::load(&backend, &config, 16, |_| callbacks += 1).unwrap();
    assert_eq!(registry.names(), ["m0", "upstream", "m1", "m2"]);
    assert_eq!(registry.default_model().name, "m2");
    assert_eq!(registry.get("").unwrap().name, "m2");
    let first = decoder(&registry, "m0");
    for name in ["m1", "m2"] {
        let alias = decoder(&registry, name);
        assert!(Arc::ptr_eq(&first.model, &alias.model));
        assert!(Arc::ptr_eq(&first.tokenizer, &alias.tokenizer));
        assert_eq!(alias.architecture, "internlm2");
        assert_eq!(registry.get(name).unwrap().weight_bytes, file.as_file().metadata().unwrap().len());
    }
    assert_eq!((first.n_ctx, decoder(&registry, "m1").n_ctx, decoder(&registry, "m2").n_ctx), (4, 16, 32));
    assert_eq!((backend.allocations.get(), backend.uploads.get(), callbacks), (12, 12, 1));
    assert_eq!(Arc::strong_count(&first.model), 3);
    let lease = first.model.clone();
    let tokenizer = Arc::downgrade(&first.tokenizer);
    let live_bytes = backend.live_bytes.load(SeqCst);
    assert!(live_bytes > 0);
    drop(registry);
    assert_eq!(backend.live_bytes.load(SeqCst), live_bytes);
    assert!(tokenizer.upgrade().is_none());
    drop(lease);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn prepare_parses_and_builds_tokenizer_once_per_file() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[file.path(), file.path(), file.path()]);
    let backend = LoadBackend::default();
    let sources = Sources::prepare(&backend, &config).unwrap();
    assert_eq!(sources.unique.len(), 1);
    assert_eq!(sources.aliases, [Some(0), Some(0), Some(0)]);
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (0, 0));
}

#[test]
#[ignore = "local MiniCheck file; metadata/tokenizer only, no weight uploads"]
fn real_minicheck_alias_preflight() {
    let path = crate::backend::test_support::fixture();
    let config = config(&[path, path]);
    let backend = LoadBackend::default();
    let mut sources = Sources::prepare(&backend, &config).unwrap();
    assert_eq!(sources.unique.len(), 1);
    assert_eq!(sources.aliases, [Some(0), Some(0)]);
    assert!(sources.unique[0].model.is_none());
    assert_eq!(sources.unique[0].tokenizer.eos_id(), Some(92542));
    sources.check_unchanged().unwrap();
    let bytes = crate::model::memory::weight_bytes(&sources.unique[0].artifact.gguf).unwrap();
    assert!(bytes > u32::MAX as usize);
    let budget = NativeMemoryBudget::new(bytes);
    sources.reserve_weights(&budget).unwrap();
    assert_eq!(budget.used(), bytes, "two aliases fit a one-copy budget");
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (0, 0));
    drop(sources);
    assert_eq!(budget.used(), 0);
    eprintln!("MiniCheck metadata admission: {bytes} tensor bytes, two aliases, zero uploads");
}

#[cfg(unix)]
#[test]
fn symlinks_hardlinks_and_dotted_paths_share_the_open_file() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let dir = tempfile::tempdir().unwrap();
    let hardlink = dir.path().join("hard.gguf");
    let symlink = dir.path().join("symbolic.gguf");
    std::fs::hard_link(file.path(), &hardlink).unwrap();
    std::os::unix::fs::symlink(file.path(), &symlink).unwrap();
    let dotted = dir.path().join(".").join("hard.gguf");
    let config = config(&[file.path(), &hardlink, &symlink, &dotted]);
    let backend = LoadBackend::default();
    let registry = Registry::load(&backend, &config, 32, |_| {}).unwrap();
    for name in ["m1", "m2", "m3"] {
        assert!(Arc::ptr_eq(&decoder(&registry, "m0").model, &decoder(&registry, name).model));
    }
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (12, 12));
}

#[test]
fn copies_and_same_sized_different_weights_are_not_aliases() {
    let first = Fixture::new(1, 8).with_tokenizer().write();
    let copy = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(first.path(), copy.path()).unwrap();
    let mut different = Fixture::new(1, 8).with_tokenizer();
    different.replace("output.weight", &[128, 8], crate::gguf::GgmlDType::F32);
    let different = different.write();
    assert_eq!(first.as_file().metadata().unwrap().len(), different.as_file().metadata().unwrap().len());
    let backend = LoadBackend::default();
    let mut callbacks = 0;
    let registry = Registry::load(&backend, &config(&[first.path(), copy.path(), different.path()]), 32, |_| callbacks += 1).unwrap();
    let first = decoder(&registry, "m0");
    for name in ["m1", "m2"] {
        assert!(!Arc::ptr_eq(&first.model, &decoder(&registry, name).model));
        assert!(!Arc::ptr_eq(&first.tokenizer, &decoder(&registry, name).tokenizer));
    }
    assert_eq!((backend.allocations.get(), backend.uploads.get(), callbacks), (36, 36, 3));
    drop(registry);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn separate_registry_loads_do_not_share_backend_buffers() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[file.path(), file.path()]);
    let a = LoadBackend::default();
    let b = LoadBackend::default();
    let first = Registry::load(&a, &config, 32, |_| {}).unwrap();
    let second = Registry::load(&b, &config, 32, |_| {}).unwrap();
    assert!(!Arc::ptr_eq(&decoder(&first, "m0").model, &decoder(&second, "m0").model));
    assert_eq!((a.uploads.get(), b.uploads.get()), (12, 12));
    drop(first);
    assert_eq!(a.live_bytes.load(SeqCst), 0);
    assert!(b.live_bytes.load(SeqCst) > 0);
}

#[test]
fn a_bad_later_tokenizer_fails_before_any_upload() {
    let first = Fixture::new(1, 8).with_tokenizer().write();
    let second = Fixture::new(1, 8).write();
    let backend = LoadBackend::default();
    let error = Registry::load(&backend, &config(&[first.path(), first.path(), second.path()]), 32, |_| panic!("unexpected callback"))
        .err().unwrap();
    assert!(error.to_string().contains("model m2: failed to build tokenizer"), "{error}");
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (0, 0));
}

#[test]
fn a_failed_later_load_releases_all_unique_weights_and_retry_is_fresh() {
    let first = Fixture::new(1, 8).with_tokenizer().write();
    let second = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[first.path(), first.path(), second.path()]);
    let backend = LoadBackend::default();
    backend.fail_upload.set(Some(15));
    let mut callbacks = 0;
    let error = Registry::load(&backend, &config, 32, |_| callbacks += 1).err().unwrap();
    assert!(error.to_string().contains("model m2: backend error: injected upload failure"), "{error}");
    assert_eq!(callbacks, 1);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
    backend.fail_upload.set(None);
    let before = backend.uploads.get();
    let registry = Registry::load(&backend, &config, 32, |_| callbacks += 1).unwrap();
    assert_eq!(backend.uploads.get() - before, 24);
    assert_eq!(callbacks, 3);
    drop(registry);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn a_source_change_after_preflight_is_rejected_before_upload() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let config = config(&[file.path(), file.path()]);
    let backend = LoadBackend::default();
    let mut sources = Sources::prepare(&backend, &config).unwrap();
    change_stamp(file.as_file());
    let error = sources.decoder(0, 32, &mut |_| panic!("unexpected callback")).err().unwrap();
    assert!(error.to_string().contains("source changed since preflight"), "{error}");
    assert_eq!((backend.allocations.get(), backend.uploads.get()), (0, 0));
}

#[test]
fn a_source_change_during_upload_releases_partial_load_without_calibration() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let backend = LoadBackend::default();
    let source = file.reopen().unwrap();
    *backend.on_upload.borrow_mut() = Some(Box::new(move || change_stamp(&source)));
    let error = Registry::load(&backend, &config(&[file.path(), file.path()]), 32, |_| panic!("unexpected callback")).err().unwrap();
    assert!(error.to_string().contains("model m0:") && error.to_string().contains("source changed"), "{error}");
    assert!(backend.uploads.get() > 0);
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[test]
fn final_source_check_prevents_publishing_a_changed_artifact() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let backend = LoadBackend::default();
    let error = Registry::load(&backend, &config(&[file.path()]), 32, |_| change_stamp(file.as_file())).err().unwrap();
    assert!(error.to_string().contains("source changed since preflight"), "{error}");
    assert_eq!(backend.live_bytes.load(SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn retargeting_a_symlink_does_not_switch_the_preflighted_payload() {
    let first = Fixture::new(1, 8).with_tokenizer().write();
    let mut different = Fixture::new(1, 8).with_tokenizer();
    different.replace("output.weight", &[128, 8], crate::gguf::GgmlDType::F32);
    let second = different.write();
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("model.gguf");
    std::os::unix::fs::symlink(first.path(), &link).unwrap();
    let config = config(&[&link, &link]);
    let mut sources = Sources::prepare(&CpuBackend, &config).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(second.path(), &link).unwrap();
    let (_, original) = sources.decoder(0, 32, &mut |_| {}).unwrap();
    let (_, alias) = sources.decoder(1, 32, &mut |_| panic!("alias recalibration")).unwrap();
    assert!(Arc::ptr_eq(&original.model, &alias.model));
    let first = Model::load(&CpuBackend, &mut GgufModel::open(first.path()).unwrap()).unwrap();
    let replacement = Model::load(&CpuBackend, &mut GgufModel::open(&link).unwrap()).unwrap();
    let forward = |model: &Model<CpuBackend>| {
        let mut kv = model.new_kv_cache(&CpuBackend, 1).unwrap();
        let mut ws = model.alloc_workspace(&CpuBackend, 1).unwrap();
        model.forward(&CpuBackend, &mut kv, &mut ws, 1, 0).unwrap()
    };
    assert_eq!(forward(&original.model), forward(&first));
    assert_ne!(forward(&original.model), forward(&replacement));
    sources.check_unchanged().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alias_sessions_keep_context_caps_and_concurrent_kv_independent() {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut config = config(&[file.path(), file.path()]);
    config.models[0].n_ctx = Some(4);
    config.models[1].n_ctx = Some(8);
    let backend = Arc::new(CpuBackend);
    let registry = Registry::load(backend.as_ref(), &config, 16, |_| {}).unwrap();
    let session = |name| {
        let d = decoder(&registry, name);
        Session::new(backend.clone(), d.model.clone(), d.tokenizer.clone(), SamplerConfig {
            temperature: 0.0, ..Default::default()
        }, d.n_ctx)
    };
    let short = session("m0");
    let long = session("m1");
    let collect = |s: &Session<CpuBackend>, prompt: &str, count| {
        let stream = s.generate(prompt, count, CancellationToken::new()).unwrap();
        async move { stream.map(|item| item.unwrap().token_id).collect::<Vec<_>>().await }
    };
    assert!(short.generate("ABCDE", 1, CancellationToken::new()).is_err());
    let expected_short = collect(&short, "AB", 6).await;
    let expected_long = collect(&long, "ABCDE", 6).await;
    assert_eq!(expected_short.len(), 2);
    assert_eq!(expected_long.len(), 3);
    let (actual_short, actual_long) = tokio::join!(collect(&short, "AB", 6), collect(&long, "ABCDE", 6));
    assert_eq!(actual_short, expected_short);
    assert_eq!(actual_long, expected_long);
}
