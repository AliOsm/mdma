//! Local load provenance, not a claim about remote execution or export permission.
use crate::cache::Stamp;
use crate::federation::canonical_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Seek, Write};

/// Complete native artifact digest, available only after a frozen native load.
/// There is deliberately no public constructor or deserializer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactId(String);

impl ArtifactId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn digest(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
}

/// The anonymous copy is private to the loader: no path can retarget it and
/// nobody outside the loader receives a writable handle. This is not OS
/// sealing against the operator/root. It costs temporary disk space, not a
/// second model-sized RAM allocation.
pub(crate) fn freeze_file(source: File) -> io::Result<(File, ArtifactId)> {
    freeze_file_with(source, || {})
}

fn freeze_file_with(mut source: File, after_copy: impl FnOnce()) -> io::Result<(File, ArtifactId)> {
    let stamp = Stamp::read(&source.metadata()?)?;
    source.rewind()?;
    let mut frozen = tempfile::tempfile()?;
    let mut hash = Sha256::new();
    let mut chunk = vec![0; 1024 * 1024];
    loop {
        let n = source.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
        frozen.write_all(&chunk[..n])?;
    }
    after_copy();
    stamp.check(source.metadata()?)?;
    frozen.rewind()?;
    Ok((
        frozen,
        ArtifactId(format!("sha256:{}", hex::encode(hash.finalize()))),
    ))
}

/// The same digest as a frozen copy, read in place with positional reads so a
/// concurrent loader's file offset is untouched. Fails if the file changes meanwhile.
pub(crate) fn hash_at(file: &File) -> io::Result<ArtifactId> {
    use std::os::unix::fs::FileExt;
    let stamp = Stamp::read(&file.metadata()?)?;
    let mut hash = Sha256::new();
    let mut chunk = vec![0; 1024 * 1024];
    let mut offset = 0;
    loop {
        let n = file.read_at(&mut chunk, offset)?;
        if n == 0 {
            break;
        }
        hash.update(&chunk[..n]);
        offset += n as u64;
    }
    stamp.check(file.metadata()?)?;
    Ok(ArtifactId(format!("sha256:{}", hex::encode(hash.finalize()))))
}

/// Logical paths are fixed by the consuming loader, never supplied by a
/// manifest or filesystem traversal. Hash the original bytes before parsing
/// or tokenizer canonicalization.
pub(crate) fn laya_id(files: &[(&str, Vec<u8>)]) -> ArtifactId {
    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort_by_key(|(path, _)| *path);
    let files: Vec<_> = sorted
        .into_iter()
        .map(|(path, bytes)| {
            Value::object([
                ("path".into(), Value::String((*path).into())),
                (
                    "sha256".into(),
                    Value::String(hex::encode(Sha256::digest(bytes))),
                ),
            ])
            .expect("fixed unique manifest fields")
        })
        .collect();
    let manifest = Value::object([
        (
            "format".into(),
            Value::String("mdma.laya-artifact.v1".into()),
        ),
        ("files".into(), Value::Array(files)),
    ])
    .expect("fixed unique manifest fields");
    ArtifactId::digest(manifest.encode().as_bytes())
}

#[cfg(test)]
mod tests;
