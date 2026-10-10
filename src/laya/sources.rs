//! The complete consumed file set. Parsers receive these captured bytes only.
use crate::artifact::{ArtifactId, laya_id};
use crate::cache::Stamp;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const PATHS: [&str; 5] = [
    "model.safetensors",
    "rl_agent_config.json",
    "encoder/config.json",
    "tokenizer/tokenizer.json",
    "tokenizer/tokenizer_config.json",
];

pub(super) struct Sources {
    files: Vec<(&'static str, Vec<u8>)>,
}

impl Sources {
    pub(super) fn capture(dir: &Path) -> io::Result<Self> {
        Self::capture_with(dir, || {})
    }

    fn capture_with(dir: &Path, after_read: impl FnOnce()) -> io::Result<Self> {
        // Pin every handle before reading any file; detect in-place writers
        // across the complete capture, not just the current read.
        let mut handles = PATHS
            .into_iter()
            .map(|path| {
                let file = File::open(dir.join(path))?;
                let stamp = Stamp::read(&file.metadata()?)?;
                Ok((path, file, stamp))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut files = Vec::with_capacity(handles.len());
        for (path, file, _) in &mut handles {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            files.push((*path, bytes));
        }
        after_read();
        for (_, file, stamp) in &handles {
            stamp.check(file.metadata()?)?;
        }
        Ok(Self { files })
    }

    pub(super) fn weights(&self) -> &[u8] {
        &self.files[0].1
    }

    pub(super) fn agent(&self) -> io::Result<&str> {
        self.text(1)
    }

    pub(super) fn encoder(&self) -> io::Result<&str> {
        self.text(2)
    }

    pub(super) fn tokenizer(&self) -> io::Result<&str> {
        self.text(3)
    }

    pub(super) fn tokenizer_config(&self) -> io::Result<&str> {
        self.text(4)
    }

    fn text(&self, index: usize) -> io::Result<&str> {
        std::str::from_utf8(&self.files[index].1)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub(super) fn id(&self) -> ArtifactId {
        laya_id(&self.files)
    }
}

#[cfg(test)]
mod tests;
