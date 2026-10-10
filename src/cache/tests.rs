use super::*;
use crate::backend::{AttnSpec, KvView, Pool, QWeight, RopeSpec, cpu::CpuBackend};
use crate::gguf::{GgmlDType, tests::Fixture};
use std::cell::Cell;
use std::io::Read;
use std::path::Path;

struct Loader {
    profile: &'static str,
    uploaded: Cell<usize>,
}

impl Loader {
    fn new() -> Self { Self { profile: "rocm/test/q4k-scales-v3/kernels-a", uploaded: Cell::new(0) } }
}

impl Backend for Loader {
    type Buf = Vec<u8>;
    fn weight_cache_profile(&self) -> Option<&str> { Some(self.profile) }
    fn upload_weight(&self, bytes: &[u8], _: WeightLayout) -> Result<Self::Buf> {
        let mut buf = self.alloc(bytes.len(), Pool::Weights)?;
        self.upload(&mut buf, 0, bytes)?;
        Ok(buf)
    }
    fn alloc(&self, n: usize, _: Pool) -> Result<Self::Buf> { Ok(vec![0; n]) }
    fn upload(&self, dst: &mut Self::Buf, offset: usize, bytes: &[u8]) -> Result<()> {
        dst[offset..offset + bytes.len()].copy_from_slice(bytes);
        self.uploaded.set(self.uploaded.get() + bytes.len());
        Ok(())
    }
    fn download(&self, _: &Self::Buf, _: usize, _: &mut [u8]) -> Result<()> { unreachable!() }
    fn copy(&self, _: &Self::Buf, _: usize, _: &mut Self::Buf, _: usize, _: usize) -> Result<()> { unreachable!() }
    fn qmatmul(&self, _: &QWeight<Self::Buf>, _: &Self::Buf, _: &mut Self::Buf) -> Result<()> { unreachable!() }
    fn embed(&self, _: &QWeight<Self::Buf>, _: &[u32], _: &mut Self::Buf) -> Result<()> { unreachable!() }
    fn rmsnorm(&self, _: &Self::Buf, _: &Self::Buf, _: &mut Self::Buf, _: f32) -> Result<()> { unreachable!() }
    fn add(&self, _: &Self::Buf, _: &Self::Buf, _: &mut Self::Buf) -> Result<()> { unreachable!() }
    fn rope(&self, _: &mut Self::Buf, _: &mut Self::Buf, _: u32, _: usize, _: &RopeSpec) -> Result<()> { unreachable!() }
    fn attn(&self, _: &Self::Buf, _: &KvView<Self::Buf>, _: &AttnSpec, _: &mut Self::Buf) -> Result<()> { unreachable!() }
    fn silu_mul(&self, _: &Self::Buf, _: &Self::Buf, _: &mut Self::Buf) -> Result<()> { unreachable!() }
}

fn fixture() -> tempfile::NamedTempFile {
    let mut f = Fixture::new(1, 256).with_tokenizer();
    f.replace("blk.0.attn_q.weight", &[256, 256], GgmlDType::Q4K);
    f.replace("output.weight", &[128, 256], GgmlDType::Q6K);
    f.write()
}

#[test]
fn prepare_reuse_force_and_budgeted_runtime_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    let original = fs::read(file.path()).unwrap();
    let built = cache.prepare(&backend, &mut source, false).unwrap();
    assert!(!built.reused);
    assert_eq!(built.packed_tensors, 1);
    assert_eq!(backend.uploaded.get(), 0);
    let before = fs::metadata(&built.path).unwrap().modified().unwrap();
    let hit = cache.prepare(&backend, &mut source, false).unwrap();
    assert!(hit.reused);
    assert_eq!(before, fs::metadata(&built.path).unwrap().modified().unwrap());
    assert!(cache.attach(&backend, &mut source).unwrap());
    let raw_size: usize = source.tensor_names().map(|n| source.tensor_desc(n).unwrap().n_bytes).sum();
    assert_eq!(built.weight_bytes as usize, raw_size);
    let small = crate::memory::NativeMemoryBudget::new(raw_size - 1);
    assert!(Model::load_with_budget(&backend, &mut source, &small).is_err());
    assert_eq!(backend.uploaded.get(), 0);
    let budget = crate::memory::NativeMemoryBudget::new(built.weight_bytes as usize);
    let model = Model::load_with_budget(&backend, &mut source, &budget).unwrap();
    assert_eq!(budget.used(), built.weight_bytes as usize);
    assert_eq!(backend.uploaded.get(), budget.used());
    drop(model);
    assert_eq!(budget.used(), 0);
    assert!(Model::validate(&CpuBackend, &source).is_err());
    let mut fresh = GgufModel::open(file.path()).unwrap();
    let forced = cache.prepare(&backend, &mut fresh, true).unwrap();
    assert!(!forced.reused);
    assert_eq!(forced.path, hit.path);
    // An already-open reader survives atomic replacement by --force.
    assert!(source.read_weight_bytes("blk.0.attn_q.weight").is_ok());
    assert_eq!(original, fs::read(file.path()).unwrap());
}

#[test]
fn content_sha_not_path_or_size_controls_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    let first = cache.prepare(&backend, &mut source, false).unwrap();
    let moved = dir.path().join("renamed.gguf");
    fs::copy(file.path(), &moved).unwrap();
    let mut copy = GgufModel::open(&moved).unwrap();
    assert_eq!(cache.prepare(&backend, &mut copy, false).unwrap().path, first.path);
    let mut modified = fs::read(&moved).unwrap();
    *modified.last_mut().unwrap() ^= 1;
    fs::write(&moved, modified).unwrap();
    let mut changed = GgufModel::open(&moved).unwrap();
    assert!(!cache.attach(&backend, &mut changed).unwrap());
    let second = cache.prepare(&backend, &mut changed, false).unwrap();
    assert_ne!(first.source_sha256, second.source_sha256);
    assert_ne!(first.path, second.path);
}

#[test]
fn profile_changes_miss_and_corrupted_payload_needs_force() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    let built = cache.prepare(&backend, &mut source, false).unwrap();
    let old = Loader { profile: "rocm/test/q4k-scales-v2/kernels-a", ..Loader::new() };
    assert!(!cache.attach(&old, &mut source).unwrap());
    let other = Loader { profile: "rocm/test/q4k-scales-v3/kernels-b", ..Loader::new() };
    assert!(!cache.attach(&other, &mut source).unwrap());
    let mut bytes = fs::read(&built.path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&built.path, bytes).unwrap();
    assert!(cache.prepare(&backend, &mut source, false).unwrap_err().to_string().contains("checksum"));
    assert!(cache.attach(&backend, &mut source).unwrap());
    assert!(source.read_weight_bytes("token_embd.weight").unwrap_err().to_string().contains("checksum"));
    let mut source = GgufModel::open(file.path()).unwrap();
    assert!(!cache.prepare(&backend, &mut source, true).unwrap().reused);
    assert!(cache.prepare(&backend, &mut source, false).unwrap().reused);
}

#[test]
fn malformed_manifest_and_truncated_artifacts_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    let built = cache.prepare(&backend, &mut source, false).unwrap();
    let original = fs::read(&built.path).unwrap();
    for length in [0, 8, 16, original.len() - 1] {
        fs::write(&built.path, &original[..length]).unwrap();
        assert!(cache.attach(&backend, &mut source).is_err());
    }
    let mut oversized = original.clone();
    oversized[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    fs::write(&built.path, oversized).unwrap();
    assert!(cache.attach(&backend, &mut source).is_err());
    let n = u64::from_le_bytes(original[8..16].try_into().unwrap()) as usize;
    let mut manifest: serde_json::Value = serde_json::from_slice(&original[16..16 + n]).unwrap();
    manifest["tensors"]["blk.0.attn_q.weight"]["offset"] = serde_json::json!(u64::MAX);
    let header = serde_json::to_vec(&manifest).unwrap();
    let mut corrupt = artifact::MAGIC.to_vec();
    corrupt.extend_from_slice(&(header.len() as u64).to_le_bytes());
    corrupt.extend_from_slice(&header);
    corrupt.extend_from_slice(&original[16 + n..]);
    fs::write(&built.path, corrupt).unwrap();
    assert!(cache.attach(&backend, &mut source).is_err());
}

#[test]
fn concurrent_writers_publish_one_complete_entry() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let results = std::thread::scope(|s| {
        #[expect(clippy::needless_collect, reason = "spawn every writer before joining any")]
        let jobs: Vec<_> = (0..3).map(|_| s.spawn(|| {
            ModelCache::new(dir.path()).prepare(&Loader::new(), &mut GgufModel::open(file.path()).unwrap(), false).unwrap()
        })).collect();
        jobs.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| !r.reused).count(), 1);
    assert!(results.iter().all(|r| r.path == results[0].path));
}

#[test]
fn source_or_cache_mutation_after_attachment_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    let built = cache.prepare(&backend, &mut source, false).unwrap();
    cache.attach(&backend, &mut source).unwrap();
    let mut writer = OpenOptions::new().write(true).open(&built.path).unwrap();
    writer.seek(SeekFrom::End(-1)).unwrap();
    writer.write_all(&[17]).unwrap();
    assert!(source.read_weight_bytes("blk.0.attn_q.weight").is_err());
    file.as_file().set_modified(file.as_file().metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(2)).unwrap();
    assert!(source.check_cache_source().is_err());
}

#[test]
fn source_mutation_between_parsing_and_fingerprinting_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let cache = ModelCache::new(dir.path());
    let backend = Loader::new();
    let mut source = GgufModel::open(file.path()).unwrap();
    file.as_file().set_modified(file.as_file().metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(2)).unwrap();
    assert!(cache.prepare(&backend, &mut source, false).unwrap_err().to_string().contains("changed"));
    assert!(cache.attach(&backend, &mut source).unwrap_err().to_string().contains("changed"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(backend.uploaded.get(), 0);
}

#[test]
fn cache_directory_precedence_and_no_raw_only_copies() {
    let env = |pairs: &[(&str, &str)]| cache_dir(|k| pairs.iter().find(|p| p.0 == k).map(|p| p.1.into()));
    assert_eq!(env(&[("HOME", "/home/u")]).unwrap(), Path::new("/home/u/.cache/mdma"));
    assert_eq!(env(&[("HOME", "/home/u"), ("XDG_CACHE_HOME", "/cache")]).unwrap(), Path::new("/cache/mdma"));
    assert_eq!(env(&[("MDMA_CACHE_DIR", "/custom"), ("XDG_CACHE_HOME", "/cache")]).unwrap(), Path::new("/custom"));
    assert!(env(&[("MDMA_CACHE_DIR", "")]).is_err());
    assert!(env(&[("XDG_CACHE_HOME", "relative")]).is_err());
    assert!(env(&[]).is_err());
    let dir = tempfile::tempdir().unwrap();
    let file = Fixture::new(1, 8).write();
    assert!(ModelCache::new(dir.path()).prepare(&Loader::new(), &mut GgufModel::open(file.path()).unwrap(), false)
        .unwrap_err().to_string().contains("raw-only"));
}

#[test]
fn sha256_matches_known_vector() {
    assert_eq!(digest(&b"abc"[..]).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"abc").unwrap();
    assert_eq!(fingerprint(&mut file).unwrap().0, digest(&b"abc"[..]).unwrap());
    let mut tail = String::new();
    file.read_to_string(&mut tail).unwrap();
    assert!(tail.is_empty());
}

#[test]
fn automatic_loading_honors_modes_hits_misses_and_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture();
    let backend = Loader::new();
    let lookup = |root: &Path, mode: Option<&str>, gguf: &mut GgufModel| {
        attach_with_env(&backend, gguf, |key| match key {
            "MDMA_CACHE" => mode.map(Into::into),
            "MDMA_CACHE_DIR" => Some(root.as_os_str().into()),
            _ => None,
        })
    };
    let mut source = GgufModel::open(file.path()).unwrap();
    lookup(&dir.path().join("absent"), None, &mut source).unwrap();
    assert!(source.cached.is_none());
    assert!(lookup(dir.path(), Some("required"), &mut source).is_err());
    assert!(source.cached.is_none());
    let built = ModelCache::new(dir.path()).prepare(&backend, &mut source, false).unwrap();
    for mode in [None, Some("auto"), Some("required")] {
        let mut source = GgufModel::open(file.path()).unwrap();
        lookup(dir.path(), mode, &mut source).unwrap();
        assert!(source.cached.is_some());
    }
    fs::write(built.path, b"invalid").unwrap();
    lookup(dir.path(), Some("off"), &mut source).unwrap();
    assert!(source.cached.is_none());
    assert!(lookup(dir.path(), Some("auto"), &mut source).is_err());
    assert!(lookup(dir.path(), Some("typo"), &mut source).is_err());
    assert!(lookup(dir.path(), Some(""), &mut source).is_err());
    assert!(attach_with_env(&CpuBackend, &mut source, |_| None).is_ok());
    assert!(attach_with_env(&CpuBackend, &mut source, |key| (key == "MDMA_CACHE").then(|| "required".into())).is_err());
    assert_eq!(backend.uploaded.get(), 0);
}
#[test]
fn frozen_load_cache_policy_requires_original_payload() {
    assert!(original_source_policy(None).is_ok());
    for mode in ["off", "auto"] {
        assert!(original_source_policy(Some(mode.into())).is_ok());
    }
    assert!(original_source_policy(Some("required".into())).unwrap_err().to_string().contains("equivalence"));
    assert!(original_source_policy(Some("invalid".into())).is_err());
}
