//! Registry-local ownership: one pinned source and one upload per file identity.
use super::{Decoder, Entry, Error, ModelsFile, Result, Unavailable};
use crate::backend::{self, Backend};
use crate::gguf::GgufModel;
use crate::memory::{NativeMemoryBudget, Reservation, sum};
use crate::model::Model;
use crate::tokenizer::Tokenizer;
use same_file::Handle;
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io;
use std::sync::Arc;
use std::time::SystemTime;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const DEVICE_HEADROOM: usize = 256 << 20;

#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl Snapshot {
    fn read(metadata: &Metadata) -> io::Result<Self> {
        if !metadata.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "GGUF source must be a regular file"));
        }
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

fn changed() -> io::Error {
    io::Error::other("GGUF source changed since preflight; keep model files immutable while loading")
}

fn failure(name: &str, source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
    Error::Load { name: name.to_string(), source: source.into() }
}

struct Artifact {
    name: String,
    gguf: GgufModel,
    snapshot: Snapshot,
    id: Option<crate::artifact::ArtifactId>,
}

impl Artifact {
    fn check_unchanged(&self) -> io::Result<()> {
        if Snapshot::read(&self.gguf.source_metadata()?)? != self.snapshot {
            return Err(changed());
        }
        Ok(())
    }
}

struct Native<B: Backend> {
    artifact: Artifact,
    tokenizer: Arc<Tokenizer>,
    model: Option<Arc<Model<B>>>,
    memory: Option<Reservation>,
    unavailable: Option<String>,
}

pub(super) struct Sources<'a, B: Backend> {
    backend: &'a B,
    aliases: Vec<Option<usize>>,
    unique: Vec<Native<B>>,
}

impl<'a, B: Backend> Sources<'a, B> {
    pub(super) fn prepare(backend: &'a B, file: &ModelsFile) -> Result<Self> {
        Self::prepare_inner(backend, file, false)
    }

    pub(super) fn prepare_frozen(backend: &'a B, file: &ModelsFile) -> Result<Self> {
        if file.models.iter().any(|spec| spec.gguf.is_some()) {
            crate::cache::require_original_source().map_err(|e| failure("frozen native sources", e))?;
        }
        Self::prepare_inner(backend, file, true)
    }

    fn prepare_inner(backend: &'a B, file: &ModelsFile, freeze: bool) -> Result<Self> {
        let mut identities = HashMap::<Handle, (usize, Snapshot)>::new();
        let mut artifacts: Vec<Artifact> = Vec::new();
        let mut aliases = Vec::with_capacity(file.models.len());
        for spec in &file.models {
            let Some(path) = &spec.gguf else {
                aliases.push(None);
                continue;
            };
            let mut prepare = || -> std::result::Result<usize, Box<dyn std::error::Error + Send + Sync>> {
                let source = File::open(path)?;
                let snapshot = Snapshot::read(&source.metadata()?)?;
                let identity = Handle::from_file(source.try_clone()?)?;
                if let Some((index, original)) = identities.get(&identity) {
                    if *original != snapshot {
                        return Err(changed().into());
                    }
                    artifacts[*index].check_unchanged()?;
                    return Ok(*index);
                }
                let (source, id) = if freeze {
                    let (source, id) = crate::artifact::freeze_file(source)?;
                    (source, Some(id))
                } else {
                    (source, None)
                };
                let loaded_snapshot = if freeze {
                    Snapshot::read(&source.metadata()?)?
                } else {
                    snapshot.clone()
                };
                let gguf = GgufModel::from_file(source)?;
                Model::validate(backend, &gguf)?;
                let artifact = Artifact { name: spec.name.clone(), gguf, snapshot: loaded_snapshot, id };
                artifact.check_unchanged()?;
                let index = artifacts.len();
                artifacts.push(artifact);
                identities.insert(identity, (index, snapshot));
                Ok(index)
            };
            aliases.push(Some(prepare().map_err(|e| failure(&spec.name, e))?));
        }
        // Preserve the all-model capability gate before constructing tokenizers.
        let unique = artifacts.into_iter().map(|mut artifact| {
            // A cache's self-declared source hash does not prove equivalence to
            // its derived payload. Until that gate exists, frozen loads consume
            // only the GGUF bytes whose digest we captured.
            if !freeze {
                Model::prepare_source(backend, &mut artifact.gguf)
                    .map_err(|e| failure(&artifact.name, e))?;
            }
            let tokenizer = Tokenizer::from_gguf(&artifact.gguf)
                .ok_or_else(|| failure(&artifact.name, "failed to build tokenizer from GGUF metadata"))?;
            Ok(Native { artifact, tokenizer: Arc::new(tokenizer), model: None, memory: None, unavailable: None })
        }).collect::<Result<Vec<_>>>()?;
        let sources = Self { backend, aliases, unique };
        sources.check_unchanged()?;
        Ok(sources)
    }

    pub(super) fn artifact_id(&self, index: usize) -> Option<&crate::artifact::ArtifactId> {
        let source = &self.unique[self.aliases[index]?];
        source.model.as_ref()?;
        source.artifact.id.as_ref()
    }

    pub(super) fn reserve_weights(&mut self, budget: &NativeMemoryBudget) -> Result<()> {
        let sizes = self.unique.iter().map(|source| crate::model::memory::weight_bytes(&source.artifact.gguf))
            .collect::<backend::Result<Vec<_>>>().map_err(|e| failure("native weight set", e))?;
        let total = sum(sizes.iter().copied()).map_err(|e| failure("native weight set", e))?;
        let mut memory = budget.reserve(total).map_err(|e| failure("native weight set", e))?;
        for (source, bytes) in self.unique.iter_mut().zip(sizes) {
            source.memory = Some(memory.split(bytes));
        }
        Ok(())
    }

    pub(super) fn decoder(
        &mut self, index: usize, n_ctx: usize, on_decoder: &mut impl FnMut(&Model<B>),
    ) -> backend::Result<(u64, Decoder<B>)> {
        let source = &mut self.unique[self.aliases[index].expect("native entry was preflighted")];
        source.artifact.check_unchanged()?;
        if source.model.is_none() {
            let model = Model::load_reserved(self.backend, &mut source.artifact.gguf, source.memory.take())?;
            source.artifact.check_unchanged()?;
            on_decoder(&model);
            source.model = Some(Arc::new(model));
        }
        let model = source.model.as_ref().expect("native model was loaded");
        Ok((source.artifact.snapshot.len, Decoder {
            model: model.clone(),
            tokenizer: source.tokenizer.clone(),
            architecture: source.artifact.gguf.metadata_str("general.architecture").expect("validated architecture").to_string(),
            n_ctx: n_ctx.min(model.config().n_ctx),
        }))
    }

    pub(super) fn available_decoder(
        &mut self, index: usize, n_ctx: usize, budget: Option<&NativeMemoryBudget>,
        on_decoder: &mut impl FnMut(&Model<B>),
    ) -> backend::Result<(u64, Entry<B>)> {
        let identity = self.aliases[index].expect("native entry was preflighted");
        if self.unique[identity].unavailable.is_none() {
            let mut load = || -> backend::Result<(u64, Decoder<B>)> {
                let source = &mut self.unique[identity];
                if source.model.is_none() {
                    let bytes = crate::model::memory::weight_bytes(&source.artifact.gguf)?;
                    if let Some(available) = self.backend.available_device_bytes()? {
                        // Leave space for driver state, warm-up and request buffers.
                        let requested = sum([bytes, DEVICE_HEADROOM])?;
                        if requested > available {
                            return Err(backend::Error::InsufficientMemory { requested, available });
                        }
                    }
                    source.memory = budget.map(|budget| budget.reserve(bytes)).transpose()?;
                }
                self.decoder(index, n_ctx, on_decoder)
            };
            match load() {
                Ok((bytes, decoder)) => return Ok((bytes, Entry::Decoder(decoder))),
                Err(error) if error.is_memory_exhausted() => {
                    let source = &mut self.unique[identity];
                    source.memory = None;
                    source.unavailable = Some(error.to_string());
                }
                Err(error) => return Err(error),
            }
        }
        let source = &self.unique[identity];
        let config = Model::validate(self.backend, &source.artifact.gguf)?;
        Ok((source.artifact.snapshot.len, Entry::Unavailable(Unavailable {
            architecture: source.artifact.gguf.metadata_str("general.architecture").expect("validated architecture").to_string(),
            n_ctx: n_ctx.min(config.n_ctx),
            reason: source.unavailable.as_ref().expect("memory-exhausted sources record the failure").clone(),
        })))
    }

    pub(super) fn check_unchanged(&self) -> Result<()> {
        for source in &self.unique {
            source.artifact.check_unchanged().map_err(|e| failure(&source.artifact.name, e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
