use super::{
    layout::WeightLayout,
    source::{Stamp, digest},
};
use crate::gguf::GgufModel;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

pub(super) const MAGIC: &[u8; 8] = b"MDMAC001";
pub(super) const MAX_MANIFEST: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tensor {
    pub dtype: String,
    pub shape: Vec<usize>,
    pub layout: WeightLayout,
    pub offset: u64,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub version: u32,
    pub source_sha256: String,
    pub profile: String,
    pub tensors: BTreeMap<String, Tensor>,
}

pub(crate) struct Reader {
    file: File,
    pub(super) manifest: Manifest,
    data_offset: u64,
    stamp: Stamp,
}

pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "model cache: {}; rebuild with mdma-cache --force",
            message.into()
        ),
    )
}

impl Reader {
    pub(super) fn open(
        path: &Path,
        gguf: &GgufModel,
        sha: &str,
        profile: &str,
    ) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let len = metadata.len();
        let stamp = Stamp::read(&metadata)?;
        let mut header = [0; 16];
        file.read_exact(&mut header)
            .map_err(|e| invalid(format!("cannot read header: {e}")))?;
        if &header[..8] != MAGIC {
            return Err(invalid("invalid magic/version"));
        }
        let manifest_len = u64::from_le_bytes(std::array::from_fn(|i| header[8 + i]));
        if manifest_len > MAX_MANIFEST || manifest_len > len.saturating_sub(16) {
            return Err(invalid("invalid manifest length"));
        }
        let mut bytes = vec![0; manifest_len as usize];
        file.read_exact(&mut bytes)?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        if manifest.version != 1 || manifest.source_sha256 != sha || manifest.profile != profile {
            return Err(invalid(
                "source SHA, backend profile or layout version mismatch",
            ));
        }
        if manifest.tensors.len() != gguf.tensor_names().count() {
            return Err(invalid("tensor count mismatch"));
        }
        let data_offset = 16 + manifest_len;
        let mut offset = 0u64;
        for (name, tensor) in &manifest.tensors {
            let desc = gguf
                .tensor_desc(name)
                .ok_or_else(|| invalid(format!("unknown tensor {name}")))?;
            let layout = WeightLayout::for_tensor(desc);
            if tensor.dtype != format!("{:?}", desc.dtype)
                || tensor.shape != desc.shape
                || tensor.layout != layout
                || tensor.bytes != layout.bytes(desc)?
                || tensor.offset != offset
                || tensor.sha256.len() != 64
                || !tensor.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            {
                return Err(invalid(format!("invalid descriptor for {name}")));
            }
            offset = offset
                .checked_add(tensor.bytes as u64)
                .ok_or_else(|| invalid("payload overflow"))?;
        }
        if data_offset.checked_add(offset) != Some(len) {
            return Err(invalid("truncated payload or trailing data"));
        }
        stamp.check_payload(file.metadata()?)?;
        Ok(Self {
            file,
            manifest,
            data_offset,
            stamp,
        })
    }

    pub(crate) fn profile(&self) -> &str {
        &self.manifest.profile
    }

    pub(crate) fn bytes(&self, name: &str) -> usize {
        self.manifest.tensors[name].bytes
    }

    pub(crate) fn read(&mut self, name: &str) -> io::Result<(Vec<u8>, WeightLayout)> {
        self.stamp.check_payload(self.file.metadata()?)?;
        let tensor = self
            .manifest
            .tensors
            .get(name)
            .ok_or_else(|| invalid(format!("missing tensor {name}")))?;
        self.file
            .seek(SeekFrom::Start(self.data_offset + tensor.offset))?;
        let mut bytes = vec![0; tensor.bytes];
        self.file.read_exact(&mut bytes)?;
        if digest(bytes.as_slice())? != tensor.sha256 {
            return Err(invalid(format!("checksum mismatch for {name}")));
        }
        self.stamp.check_payload(self.file.metadata()?)?;
        Ok((bytes, tensor.layout))
    }

    pub(super) fn verify(&mut self) -> io::Result<()> {
        for (name, tensor) in &self.manifest.tensors {
            self.file
                .seek(SeekFrom::Start(self.data_offset + tensor.offset))?;
            if digest((&mut self.file).take(tensor.bytes as u64))? != tensor.sha256 {
                return Err(invalid(format!("checksum mismatch for {name}")));
            }
        }
        self.stamp.check_payload(self.file.metadata()?)
    }
}
