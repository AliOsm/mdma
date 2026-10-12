//! Explicit cache preparation; inference only reads compatible, SHA-keyed artifacts.
use crate::{
    backend::{Backend, Error, Result},
    gguf::GgufModel,
    model::Model,
};
pub(crate) use artifact::Reader;
use artifact::{Manifest, Tensor, invalid};
pub(crate) use source::Stamp;
use source::digest;
pub(crate) use source::fingerprint;
#[cfg(all(test, feature = "rocm"))]
pub(crate) use source::fingerprint as source_fingerprint;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::PathBuf,
};

mod artifact;
pub mod layout;
mod layout_abi;
mod source;
pub use layout::WeightLayout;

pub struct ModelCache {
    root: PathBuf,
}

#[derive(Debug)]
pub struct Prepared {
    pub path: PathBuf,
    pub source_sha256: String,
    pub reused: bool,
    pub packed_tensors: usize,
    pub weight_bytes: u64,
}

impl ModelCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// MDMA_CACHE_DIR, then XDG_CACHE_HOME/mdma, then HOME/.cache/mdma.
    pub fn from_env() -> io::Result<Self> {
        cache_dir(|name| std::env::var_os(name)).map(Self::new)
    }

    fn path(&self, sha: &str, profile: &str) -> io::Result<PathBuf> {
        Ok(self
            .root
            .join(sha)
            .join(format!("{}.mdmac", digest(profile.as_bytes())?)))
    }

    /// Preflight, pack on the host, and atomically publish. No generation or GPU upload.
    pub fn prepare<B: Backend>(
        &self,
        backend: &B,
        gguf: &mut GgufModel,
        force: bool,
    ) -> Result<Prepared> {
        if gguf.cached.is_some() {
            return Err(invalid("prepare requires an uncached GGUF source").into());
        }
        gguf.check_source_unchanged()?;
        Model::validate(backend, gguf)?;
        let profile = backend
            .weight_cache_profile()
            .ok_or(Error::NotImplemented("prepared cache for this backend"))?;
        let (sha, stamp) = fingerprint(&mut gguf.file)?;
        gguf.check_source_unchanged()?;
        let path = self.path(&sha, profile)?;
        let parent = path
            .parent()
            .expect("artifact paths live under the cache root");
        let mut dirs = fs::DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dirs.mode(0o700);
        }
        dirs.create(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path.with_extension("lock"))?;
        lock.lock()?;
        stamp.check(gguf.source_metadata()?)?;
        if !force {
            match Reader::open(&path, gguf, &sha, profile) {
                Ok(mut reader) => {
                    reader.verify()?;
                    stamp.check(gguf.source_metadata()?)?;
                    return Ok(summary(path, &reader.manifest, true));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut offset = 0u64;
        let mut tensors = BTreeMap::new();
        let mut names: Vec<_> = gguf.tensor_names().map(str::to_string).collect();
        names.sort();
        for name in names {
            let desc = gguf
                .tensor_desc(&name)
                .expect("name comes from tensor_names");
            let layout = WeightLayout::for_tensor(desc);
            let bytes = layout.bytes(desc)?;
            tensors.insert(
                name,
                Tensor {
                    dtype: format!("{:?}", desc.dtype),
                    shape: desc.shape.clone(),
                    layout,
                    offset,
                    bytes,
                    sha256: "0".repeat(64),
                },
            );
            offset = offset
                .checked_add(bytes as u64)
                .ok_or(Error::SizeOverflow)?;
        }
        if !tensors.values().any(|t| t.layout != WeightLayout::Gguf) {
            return Err(Error::UnsupportedModel(
                "ROCm cache requires at least one Q4_K matrix; refusing a raw-only copy".into(),
            ));
        }
        let mut manifest = Manifest {
            version: 1,
            source_sha256: sha,
            profile: profile.into(),
            tensors,
        };
        let placeholder = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
        if placeholder.len() as u64 > artifact::MAX_MANIFEST {
            return Err(invalid("manifest exceeds size limit").into());
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(artifact::MAGIC)?;
        temp.write_all(&(placeholder.len() as u64).to_le_bytes())?;
        temp.write_all(&placeholder)?;
        for (name, tensor) in &mut manifest.tensors {
            let raw = gguf
                .read_tensor_bytes(name)
                .map_err(|e| io::Error::other(e.to_string()))?;
            let bytes = match tensor.layout {
                WeightLayout::Gguf => raw,
                WeightLayout::RocmQ4kV3 => layout::pack_q4k(&raw)?,
            };
            tensor.sha256 = digest(bytes.as_slice())?;
            temp.write_all(&bytes)?;
        }
        stamp.check(gguf.source_metadata()?)?;
        let header = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
        if header.len() != placeholder.len() {
            return Err(invalid("manifest size changed during preparation").into());
        }
        temp.seek(SeekFrom::Start(16))?;
        temp.write_all(&header)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()?;
        Ok(summary(path, &manifest, false))
    }

    /// Attach weights to a pinned GGUF. Metadata/tokenizer remain in the original GGUF.
    pub fn attach<B: Backend>(&self, backend: &B, gguf: &mut GgufModel) -> Result<bool> {
        gguf.check_source_unchanged()?;
        Model::validate(backend, gguf)?;
        let profile = backend
            .weight_cache_profile()
            .ok_or(Error::NotImplemented("prepared cache for this backend"))?;
        if let Some(reader) = &gguf.cached {
            if reader.profile() != profile {
                return Err(invalid("backend profile mismatch").into());
            }
            gguf.check_cache_source()?;
            return Ok(true);
        }
        let (sha, stamp) = fingerprint(&mut gguf.file)?;
        gguf.check_source_unchanged()?;
        let path = self.path(&sha, profile)?;
        match Reader::open(&path, gguf, &sha, profile) {
            Ok(reader) => {
                stamp.check(gguf.source_metadata()?)?;
                gguf.cached = Some(reader);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

fn summary(path: PathBuf, manifest: &Manifest, reused: bool) -> Prepared {
    Prepared {
        path,
        source_sha256: manifest.source_sha256.clone(),
        reused,
        packed_tensors: manifest
            .tensors
            .values()
            .filter(|t| t.layout != WeightLayout::Gguf)
            .count(),
        weight_bytes: manifest.tensors.values().map(|t| t.bytes as u64).sum(),
    }
}

fn cache_dir(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    for (key, suffix) in [
        ("MDMA_CACHE_DIR", ""),
        ("XDG_CACHE_HOME", "mdma"),
        ("HOME", ".cache/mdma"),
    ] {
        if let Some(value) = env(key) {
            if value.is_empty() {
                return Err(io::Error::other(format!("{key} must not be empty")));
            }
            let root = PathBuf::from(value);
            if key == "XDG_CACHE_HOME" && !root.is_absolute() {
                return Err(io::Error::other("XDG_CACHE_HOME must be absolute"));
            }
            return Ok(if suffix.is_empty() {
                root
            } else {
                root.join(suffix)
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "set MDMA_CACHE_DIR, XDG_CACHE_HOME or HOME",
    ))
}

pub(crate) fn attach_from_env<B: Backend>(backend: &B, gguf: &mut GgufModel) -> Result<()> {
    attach_with_env(backend, gguf, |key| std::env::var_os(key))
}

fn attach_with_env<B: Backend>(
    backend: &B,
    gguf: &mut GgufModel,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<()> {
    let mode = cache_mode(env("MDMA_CACHE"))?;
    if mode == "off" || gguf.cached.is_some() {
        return Ok(());
    }
    if backend.weight_cache_profile().is_none() {
        return if mode == "required" {
            Err(Error::NotImplemented("prepared cache for this backend"))
        } else {
            Ok(())
        };
    }
    let cache = match cache_dir(env).map(ModelCache::new) {
        Ok(cache) => cache,
        Err(e) if mode == "auto" && e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // No hash pass when no cache has ever been prepared in this directory.
    if !cache.root.try_exists()? && mode == "auto" {
        return Ok(());
    }
    if !cache.attach(backend, gguf)? && mode == "required" {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no compatible model cache; run mdma-cache first",
        )
        .into());
    }
    Ok(())
}

fn cache_mode(mode: Option<std::ffi::OsString>) -> io::Result<String> {
    let mode = mode
        .unwrap_or_else(|| "auto".into())
        .into_string()
        .map_err(|_| io::Error::other("MDMA_CACHE must be UTF-8"))?;
    if !matches!(mode.as_str(), "auto" | "off" | "required") {
        return Err(io::Error::other("MDMA_CACHE must be auto, off or required"));
    }
    Ok(mode)
}

pub(crate) fn require_original_source() -> io::Result<()> {
    original_source_policy(std::env::var_os("MDMA_CACHE"))
}

fn original_source_policy(mode: Option<std::ffi::OsString>) -> io::Result<()> {
    if cache_mode(mode)? == "required" {
        return Err(io::Error::other(
            "frozen artifact loads cannot use MDMA_CACHE=required: prepared payload equivalence is not verified",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
