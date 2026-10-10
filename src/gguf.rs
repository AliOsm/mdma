//! GGUF metadata and raw weight loading.
pub use candle_core::quantized::GgmlDType;

use candle_core::quantized::gguf_file::{Content, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to parse GGUF container: {0}")]
    Parse(String),
    #[error("unknown tensor {0}")]
    UnknownTensor(String),
}

#[derive(Debug, Clone)]
pub struct TensorDesc {
    pub dtype: GgmlDType,
    pub shape: Vec<usize>,
    pub offset: u64,
    pub n_bytes: usize,
}

pub struct GgufModel {
    pub(crate) file: File,
    source_stamp: crate::cache::Stamp,
    tensor_data_offset: u64,
    metadata: HashMap<String, Value>,
    tensors: HashMap<String, TensorDesc>,
    pub(crate) cached: Option<crate::cache::Reader>,
}

impl GgufModel {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file(File::open(path)?)
    }

    pub(crate) fn from_file(mut file: File) -> Result<Self> {
        let source_stamp = crate::cache::Stamp::read(&file.metadata()?)?;
        let content = Content::read(&mut file).map_err(|e| Error::Parse(e.to_string()))?;
        source_stamp.check(file.metadata()?)?;

        let tensors = content
            .tensor_infos
            .into_iter()
            .map(|(name, info)| {
                let n_bytes = tensor_n_bytes(&name, info.ggml_dtype, info.shape.dims())?;
                let desc = TensorDesc {
                    dtype: info.ggml_dtype,
                    shape: info.shape.dims().to_vec(),
                    offset: info.offset,
                    n_bytes,
                };
                Ok((name, desc))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(Self {
            file,
            source_stamp,
            tensor_data_offset: content.tensor_data_offset,
            metadata: content.metadata,
            tensors,
            cached: None,
        })
    }

    pub(crate) fn source_metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    pub(crate) fn check_source_unchanged(&self) -> std::io::Result<()> {
        self.source_stamp.check(self.source_metadata()?)
    }

    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata.get(key)?.to_string().ok().map(String::as_str)
    }

    pub fn has_metadata(&self, key: &str) -> bool {
        self.metadata.contains_key(key)
    }

    pub fn metadata_keys(&self) -> impl Iterator<Item = &str> {
        self.metadata.keys().map(String::as_str)
    }

    pub fn metadata_u32(&self, key: &str) -> Option<u32> {
        self.metadata.get(key)?.to_u32().ok()
    }

    pub fn metadata_bool(&self, key: &str) -> Option<bool> {
        self.metadata.get(key)?.to_bool().ok()
    }

    pub fn metadata_f32(&self, key: &str) -> Option<f32> {
        self.metadata.get(key)?.to_f32().ok()
    }

    fn metadata_array<T>(&self, key: &str, elem: impl Fn(&Value) -> Option<T>) -> Option<Vec<T>> {
        self.metadata.get(key)?.to_vec().ok()?.iter().map(elem).collect()
    }

    pub fn metadata_string_array(&self, key: &str) -> Option<Vec<String>> {
        self.metadata_array(key, |v| v.to_string().ok().cloned())
    }

    pub fn metadata_f32_array(&self, key: &str) -> Option<Vec<f32>> {
        self.metadata_array(key, |v| v.to_f32().ok())
    }

    pub fn metadata_i32_array(&self, key: &str) -> Option<Vec<i32>> {
        self.metadata_array(key, |v| v.to_i32().ok())
    }

    pub fn metadata_u32_array(&self, key: &str) -> Option<Vec<u32>> {
        // GGUF writers commonly use signed integers for per-layer arrays.
        self.metadata_array(key, |v| match v {
            Value::U32(n) => Some(*n),
            Value::I32(n) => u32::try_from(*n).ok(),
            _ => None,
        })
    }

    pub fn metadata_bool_array(&self, key: &str) -> Option<Vec<bool>> {
        self.metadata_array(key, |v| v.to_bool().ok())
    }

    pub fn tensor_names(&self) -> impl Iterator<Item = &str> {
        self.tensors.keys().map(String::as_str)
    }

    pub fn tensor_desc(&self, name: &str) -> Option<&TensorDesc> {
        self.tensors.get(name)
    }

    /// Resident weight bytes, including a prepared layout's padding/headers.
    pub(crate) fn weight_bytes(&self, name: &str) -> usize {
        self.cached.as_ref().map_or_else(|| self.tensors[name].n_bytes, |cache| cache.bytes(name))
    }

    pub(crate) fn cache_profile(&self) -> Option<&str> {
        self.cached.as_ref().map(crate::cache::Reader::profile)
    }

    pub(crate) fn check_cache_source(&self) -> std::io::Result<()> {
        if self.cached.is_some() {
            self.check_source_unchanged()?;
        }
        Ok(())
    }

    /// Check every payload extent against the open file without reading it.
    pub fn validate_tensor_ranges(&self) -> Result<()> {
        let file_len = self.file.metadata()?.len();
        for (name, desc) in &self.tensors {
            tensor_start(name, desc, self.tensor_data_offset, file_len)?;
        }
        Ok(())
    }

    /// GGUF-layout bytes. Prepared matrices must use the model weight loader.
    pub fn read_tensor_bytes(&mut self, name: &str) -> Result<Vec<u8>> {
        let (bytes, layout) = self.read_weight_bytes(name)?;
        if layout != crate::cache::WeightLayout::Gguf {
            return Err(Error::Parse(format!("{name}: prepared weight is not in GGUF layout")));
        }
        Ok(bytes)
    }

    pub(crate) fn read_weight_bytes(&mut self, name: &str) -> Result<(Vec<u8>, crate::cache::WeightLayout)> {
        self.check_cache_source()?;
        if let Some(cache) = &mut self.cached {
            return Ok(cache.read(name)?);
        }
        let desc = self.tensors.get(name).ok_or_else(|| Error::UnknownTensor(name.to_string()))?;
        let start = tensor_start(name, desc, self.tensor_data_offset, self.file.metadata()?.len())?;
        let mut buf = vec![0u8; desc.n_bytes];
        self.file.seek(SeekFrom::Start(start))?;
        self.file.read_exact(&mut buf)?;
        Ok((buf, crate::cache::WeightLayout::Gguf))
    }
}

fn tensor_n_bytes(name: &str, dtype: GgmlDType, shape: &[usize]) -> Result<usize> {
    let overflow = || Error::Parse(format!("{name}: tensor byte size overflows usize"));
    let elements = shape.iter().try_fold(1usize, |n, &dim| n.checked_mul(dim)).ok_or_else(overflow)?;
    if !elements.is_multiple_of(dtype.block_size()) {
        return Err(Error::Parse(format!("{name}: element count {elements} is not aligned to {dtype:?} blocks")));
    }
    (elements / dtype.block_size()).checked_mul(dtype.type_size()).ok_or_else(overflow)
}

fn tensor_start(name: &str, desc: &TensorDesc, data_offset: u64, file_len: u64) -> Result<u64> {
    let overflow = || Error::Parse(format!("{name}: tensor byte range overflows u64"));
    let start = data_offset.checked_add(desc.offset).ok_or_else(overflow)?;
    let bytes = u64::try_from(desc.n_bytes).map_err(|_| overflow())?;
    let end = start.checked_add(bytes).ok_or_else(overflow)?;
    if end > file_len {
        return Err(Error::Parse(format!("{name}: tensor byte range {start}..{end} exceeds file length {file_len}")));
    }
    Ok(start)
}

#[cfg(test)]
pub(crate) mod tests;
