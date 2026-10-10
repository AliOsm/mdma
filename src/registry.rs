//! Named GGUF decoders, CPU Laya checkpoints, and models on other MDMA nodes.
use crate::backend::Backend;
use crate::backend::cpu::CpuBackend;
use crate::laya::Predictor;
use crate::model::Model;
use crate::tokenizer::Tokenizer;
use crate::upstream::{Upstream, UpstreamConfig};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

mod native;
mod warmup;

pub use warmup::MAX_WARMUP_ROUNDS;
pub use crate::artifact::ArtifactId;
pub use crate::federation::profile::ExecutionProfile;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("models config: {0}")]
    Config(String),
    #[error("model {name}: {source}")]
    Load { name: String, source: Box<dyn std::error::Error + Send + Sync> },
}

pub type Result<T> = std::result::Result<T, Error>;

/// One `[[models]]` entry. Exactly one of `gguf`, `laya`, `upstream`.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    #[serde(default)]
    pub gguf: Option<String>,
    #[serde(default)]
    pub laya: Option<String>,
    #[serde(default)]
    pub upstream: Option<UpstreamConfig>,
    /// Per-request KV cap for a decoder; the server default otherwise.
    #[serde(default)]
    pub n_ctx: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gguf,
    Laya,
    Upstream,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gguf => "gguf",
            Self::Laya => "laya",
            Self::Upstream => "upstream",
        }
    }
}

impl ModelSpec {
    pub fn kind(&self) -> Result<Kind> {
        match (&self.gguf, &self.laya, &self.upstream) {
            (Some(_), None, None) => Ok(Kind::Gguf),
            (None, Some(_), None) => Ok(Kind::Laya),
            (None, None, Some(_)) => Ok(Kind::Upstream),
            _ => Err(Error::Config(format!("model {:?} needs exactly one of gguf, laya, upstream", self.name))),
        }
    }
}

/// The `MDMA_MODELS` file.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelsFile {
    /// Which model an empty `model` field means; the first entry otherwise.
    #[serde(default)]
    pub default: Option<String>,
    pub models: Vec<ModelSpec>,
}

impl ModelsFile {
    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        file.validate()?;
        Ok(file)
    }

    /// One GGUF, named after its file stem.
    pub fn single_gguf(path: &str) -> Self {
        let name = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("default").to_string();
        Self { default: None, models: vec![ModelSpec { name, gguf: Some(path.to_string()), laya: None, upstream: None, n_ctx: None }] }
    }

    fn validate(&self) -> Result<()> {
        if self.models.is_empty() {
            return Err(Error::Config("no models listed".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for spec in &self.models {
            if spec.name.trim().is_empty() {
                return Err(Error::Config("a model has an empty name".into()));
            }
            if !seen.insert(spec.name.as_str()) {
                return Err(Error::Config(format!("model {:?} is listed twice", spec.name)));
            }
            spec.kind()?;
            if let Some(upstream) = &spec.upstream {
                upstream.validate().map_err(|e| Error::Config(format!("model {:?}: {e}", spec.name)))?;
            }
            if spec.n_ctx == Some(0) {
                return Err(Error::Config(format!("model {:?}: n_ctx must be positive", spec.name)));
            }
        }
        if let Some(default) = &self.default
            && !seen.contains(default.as_str())
        {
            return Err(Error::Config(format!("default model {default:?} is not listed")));
        }
        Ok(())
    }

    pub fn default_index(&self) -> usize {
        self.default.as_ref().and_then(|d| self.models.iter().position(|m| &m.name == d)).unwrap_or(0)
    }
}

pub struct Decoder<B: Backend> {
    pub model: Arc<Model<B>>,
    pub tokenizer: Arc<Tokenizer>,
    pub architecture: String,
    pub n_ctx: usize,
}

#[derive(Clone)]
pub struct Unavailable {
    pub architecture: String,
    pub n_ctx: usize,
    pub reason: String,
}

pub enum Entry<B: Backend> {
    Decoder(Decoder<B>),
    Laya(Arc<Predictor<CpuBackend>>),
    Upstream(Arc<Upstream>),
    Unavailable(Unavailable),
}

impl<B: Backend> std::fmt::Debug for Entry<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decoder(decoder) => f.debug_struct("Decoder")
                .field("architecture", &decoder.architecture)
                .field("n_ctx", &decoder.n_ctx)
                .finish_non_exhaustive(),
            Self::Laya(_) => f.debug_tuple("Laya").finish_non_exhaustive(),
            Self::Upstream(_) => f.debug_tuple("Upstream").finish_non_exhaustive(),
            Self::Unavailable(info) => f.debug_struct("Unavailable")
                .field("architecture", &info.architecture)
                .field("n_ctx", &info.n_ctx)
                .field("reason", &info.reason)
                .finish(),
        }
    }
}

pub struct Loaded<B: Backend> {
    pub name: String,
    pub kind: Kind,
    /// Logical artifact size on disk; repeated for aliases, 0 for an upstream.
    /// Not additive device-residency accounting.
    pub weight_bytes: u64,
    pub entry: Entry<B>,
}

pub struct Registry<B: Backend> {
    models: Vec<Loaded<B>>,
    default: usize,
    artifacts: std::collections::HashMap<String, ArtifactId>,
    profiles: std::collections::HashMap<String, ExecutionProfile>,
    execution_identity: Option<Arc<()>>,
}

fn file_size(path: &str) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

impl<B: Backend> Registry<B> {
    /// Preflights all native GGUFs before loading any weights. Aliases of the
    /// same open file share one model/tokenizer within this registry/backend.
    /// `on_decoder` runs once per unique native model after load (Vulkan f16
    /// calibration), not once per alias.
    pub fn load(backend: &B, file: &ModelsFile, default_n_ctx: usize, on_decoder: impl FnMut(&Model<B>)) -> Result<Self> {
        Self::load_with_budget(backend, file, default_n_ctx, None, on_decoder)
    }

    /// With a budget, admit the entire unique native weight set before any
    /// upload. Models carry the same budget into KV/workspace allocations and
    /// Session admission. Laya/upstream entries are not charged.
    pub fn load_with_budget(
        backend: &B, file: &ModelsFile, default_n_ctx: usize,
        budget: Option<&crate::memory::NativeMemoryBudget>, on_decoder: impl FnMut(&Model<B>),
    ) -> Result<Self> {
        Self::load_inner(backend, file, default_n_ctx, budget, false, false, on_decoder)
    }

    /// Daemon admission: skip native models on memory exhaustion, preserving
    /// their names/default and serving the models which fit. Other errors fail.
    pub fn load_available_with_budget(
        backend: &B, file: &ModelsFile, default_n_ctx: usize,
        budget: Option<&crate::memory::NativeMemoryBudget>, on_decoder: impl FnMut(&Model<B>),
    ) -> Result<Self> {
        Self::load_inner(backend, file, default_n_ctx, budget, true, false, on_decoder)
    }

    /// Federation provenance: copy each unique GGUF into a private anonymous
    /// snapshot and capture all consumed Laya bytes before parsing/uploading.
    /// Prepared GGUF caches are bypassed (`MDMA_CACHE=required` fails closed).
    /// This does not grant export permission or establish an execution profile.
    pub fn load_available_frozen_with_budget(
        backend: &B, file: &ModelsFile, default_n_ctx: usize,
        budget: Option<&crate::memory::NativeMemoryBudget>, on_decoder: impl FnMut(&Model<B>),
    ) -> Result<Self> {
        Self::load_inner(backend, file, default_n_ctx, budget, true, true, on_decoder)
    }

    fn load_inner(
        backend: &B, file: &ModelsFile, default_n_ctx: usize,
        budget: Option<&crate::memory::NativeMemoryBudget>, skip_memory: bool, freeze: bool,
        mut on_decoder: impl FnMut(&Model<B>),
    ) -> Result<Self> {
        file.validate()?;
        let mut native = if freeze {
            native::Sources::prepare_frozen(backend, file)?
        } else {
            native::Sources::prepare(backend, file)?
        };
        if !skip_memory && let Some(budget) = budget {
            native.reserve_weights(budget)?;
        }
        let mut models = Vec::with_capacity(file.models.len());
        let mut artifacts = std::collections::HashMap::new();
        for (index, spec) in file.models.iter().enumerate() {
            let fail = |e: Box<dyn std::error::Error + Send + Sync>| Error::Load { name: spec.name.clone(), source: e };
            let (kind, weight_bytes, entry) = match spec.kind()? {
                Kind::Gguf => {
                    let n_ctx = spec.n_ctx.unwrap_or(default_n_ctx);
                    let (bytes, entry) = if skip_memory {
                        native.available_decoder(index, n_ctx, budget, &mut on_decoder)
                    } else {
                        native.decoder(index, n_ctx, &mut on_decoder).map(|(bytes, decoder)| (bytes, Entry::Decoder(decoder)))
                    }.map_err(|e| fail(e.into()))?;
                    if let Some(id) = native.artifact_id(index) {
                        artifacts.insert(spec.name.clone(), id.clone());
                    }
                    (Kind::Gguf, bytes, entry)
                }
                Kind::Laya => {
                    let dir = spec.laya.as_deref().expect("Laya entries carry a checkpoint path");
                    let (predictor, weights) = if freeze {
                        let (predictor, id, bytes) = Predictor::load_frozen(CpuBackend, Path::new(dir)).map_err(|e| fail(e.into()))?;
                        artifacts.insert(spec.name.clone(), id);
                        (predictor, bytes)
                    } else {
                        let predictor = Predictor::load(CpuBackend, Path::new(dir)).map_err(|e| fail(e.into()))?;
                        (predictor, file_size(&format!("{dir}/model.safetensors")))
                    };
                    (Kind::Laya, weights, Entry::Laya(Arc::new(predictor)))
                }
                Kind::Upstream => {
                    let upstream = Upstream::new(spec.upstream.as_ref().expect("upstream entries carry their target")).map_err(fail)?;
                    (Kind::Upstream, 0, Entry::Upstream(Arc::new(upstream)))
                }
            };
            models.push(Loaded { name: spec.name.clone(), kind, weight_bytes, entry });
        }
        native.check_unchanged()?;
        Ok(Self { models, default: file.default_index(), artifacts, profiles: std::collections::HashMap::default(),
            execution_identity: backend.execution_identity() })
    }

    /// One already-loaded decoder under `name` (library callers and tests).
    pub fn single(name: &str, decoder: Decoder<B>) -> Self {
        Self { models: vec![Loaded { name: name.to_string(), kind: Kind::Gguf, weight_bytes: 0, entry: Entry::Decoder(decoder) }], default: 0, artifacts: std::collections::HashMap::default(), profiles: std::collections::HashMap::default(), execution_identity: None }
    }

    fn same_backend(&self, backend: &B) -> bool {
        match (&self.execution_identity, backend.execution_identity()) {
            (Some(loaded), Some(current)) => Arc::ptr_eq(loaded, &current),
            (None, None) => true,
            _ => false,
        }
    }

    pub(crate) fn bind_execution_backend(&mut self, backend: &B) {
        if !self.same_backend(backend) {
            self.profiles.clear();
        }
    }

    /// Capture profiles after calibration/warm-up, only for frozen native
    /// entries. Only concrete implemented backends (not labels or caller
    /// claims) and native CPU Laya have a defined vocabulary. Capture GPU
    /// runtime/device/kernel and resolved modes after calibration.
    /// Recapture clears old snapshots before any fallible work.
    pub fn capture_execution_profiles(&mut self, backend: &B) -> std::io::Result<usize>
    where B: 'static {
        use crate::federation::profile::ExecutorIdentity;
        self.capture_execution_profiles_with(backend, ExecutorIdentity::capture)
    }

    fn capture_execution_profiles_with(
        &mut self, backend: &B,
        capture: impl FnOnce() -> std::io::Result<crate::federation::profile::ExecutorIdentity>,
    ) -> std::io::Result<usize>
    where B: 'static {
        let cpu = (backend as &dyn std::any::Any).is::<CpuBackend>();
        self.profiles.clear();
        let gpu = crate::federation::profile::gpu::capture(backend)?;
        if gpu.is_some() && !self.same_backend(backend) {
            return Err(std::io::Error::other("profile backend differs from uploaded GPU resources"));
        }
        let candidates: Vec<_> = self.models.iter().filter(|loaded| {
            self.artifacts.contains_key(&loaded.name)
                && match &loaded.entry {
                    Entry::Laya(_) => true,
                    Entry::Decoder(_) => cpu || gpu.is_some(),
                    _ => false,
                }
        }).map(|loaded| (loaded.name.clone(), matches!(loaded.entry, Entry::Laya(_)))).collect();
        if candidates.is_empty() {
            return Ok(0);
        }
        let executor = Arc::new(capture()?);
        for (name, laya) in candidates {
            let profile = if laya || cpu {
                ExecutionProfile::cpu(executor.clone(), laya)
            } else {
                ExecutionProfile::gpu(executor.clone(), gpu.clone().expect("selected concrete GPU"))
            };
            self.profiles.insert(name, profile);
        }
        Ok(self.profiles.len())
    }

    /// Current profile of a frozen, still-loaded native model. Deployment
    /// changes/unverifiable image identity conservatively withdraw the profile.
    /// Neither provenance getter confers federation export/execute permission.
    pub fn execution_profile(&self, name: &str) -> Option<&ExecutionProfile> {
        self.artifact_id(name)?;
        let profile = self.profiles.get(&self.get(name)?.name)?;
        profile.is_current().then_some(profile)
    }

    /// Exact captured bytes of a currently loaded native entry, never an
    /// upstream or a library-constructed decoder. `""` resolves the default.
    pub fn artifact_id(&self, name: &str) -> Option<&ArtifactId> {
        let loaded = self.get(name)?;
        if !matches!(loaded.entry, Entry::Decoder(_) | Entry::Laya(_)) {
            return None;
        }
        self.artifacts.get(&loaded.name)
    }

    /// `""` means the default model.
    pub fn get(&self, name: &str) -> Option<&Loaded<B>> {
        if name.is_empty() { Some(&self.models[self.default]) } else { self.models.iter().find(|m| m.name == name) }
    }

    pub fn default_model(&self) -> &Loaded<B> {
        &self.models[self.default]
    }

    pub fn iter(&self) -> impl Iterator<Item = &Loaded<B>> {
        self.models.iter()
    }

    pub fn names(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.name.as_str()).collect()
    }
}
