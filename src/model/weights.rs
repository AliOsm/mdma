// `model.safetensors` into backend buffers. Matrices keep their stored
// dtype (F16/BF16/F32 all dequantize on the way into a matmul); norms and
// biases become f32 rows like the decoder's norm weights.
use crate::backend::{Backend, Pool, QWeight};
use crate::gguf::GgmlDType;
use safetensors::{Dtype, SafeTensors};
use std::ops::Range;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("checkpoint: {0}")]
    Checkpoint(String),
    #[error(transparent)]
    Backend(#[from] crate::backend::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Weights<'a> {
    st: SafeTensors<'a>,
}

fn ggml_dtype(dtype: Dtype) -> Result<GgmlDType> {
    Ok(match dtype {
        Dtype::F32 => GgmlDType::F32,
        Dtype::F16 => GgmlDType::F16,
        Dtype::BF16 => GgmlDType::BF16,
        other => {
            return Err(Error::Checkpoint(format!(
                "unsupported tensor dtype {other:?}"
            )));
        }
    })
}

fn to_f32(dtype: Dtype, bytes: &[u8]) -> Result<Vec<f32>> {
    Ok(match dtype {
        Dtype::F32 => bytemuck::cast_slice::<u8, f32>(bytes).to_vec(),
        Dtype::F16 => bytemuck::cast_slice::<u8, half::f16>(bytes)
            .iter()
            .map(|v| v.to_f32())
            .collect(),
        Dtype::BF16 => bytemuck::cast_slice::<u8, half::bf16>(bytes)
            .iter()
            .map(|v| v.to_f32())
            .collect(),
        other => {
            return Err(Error::Checkpoint(format!(
                "unsupported tensor dtype {other:?}"
            )));
        }
    })
}

impl<'a> Weights<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Ok(Self {
            st: SafeTensors::deserialize(bytes).map_err(|e| Error::Checkpoint(e.to_string()))?,
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.st.names().into_iter().map(String::from).collect()
    }

    fn view(&self, name: &str) -> Result<safetensors::tensor::TensorView<'a>> {
        self.st
            .tensor(name)
            .map_err(|_| Error::Checkpoint(format!("missing tensor {name}")))
    }

    fn upload<B: Backend>(backend: &B, bytes: &[u8]) -> Result<B::Buf> {
        let mut buf = backend.alloc(bytes.len(), Pool::Weights)?;
        backend.upload(&mut buf, 0, bytes)?;
        Ok(buf)
    }

    /// A rank-2 `[rows, cols]` matrix, optionally just a row range of it
    /// (fused projections such as `Wqkv` split into their parts).
    pub fn matrix<B: Backend>(
        &self,
        backend: &B,
        name: &str,
        rows: Option<Range<usize>>,
    ) -> Result<QWeight<B::Buf>> {
        let view = self.view(name)?;
        let &[n_rows, n_cols] = view.shape() else {
            return Err(Error::Checkpoint(format!(
                "{name}: expected a rank-2 tensor, got {:?}",
                view.shape()
            )));
        };
        let dtype = ggml_dtype(view.dtype())?;
        let rows = rows.unwrap_or(0..n_rows);
        if rows.end > n_rows {
            return Err(Error::Checkpoint(format!(
                "{name}: rows {rows:?} out of {n_rows}"
            )));
        }
        let row_bytes = n_cols * dtype.type_size();
        let bytes = &view.data()[rows.start * row_bytes..rows.end * row_bytes];
        Ok(QWeight {
            dtype,
            shape: vec![rows.len(), n_cols],
            buf: Self::upload(backend, bytes)?,
        })
    }

    /// A rank-1 tensor (norm weight, bias) as f32, optionally a sub-range.
    pub fn vector<B: Backend>(
        &self,
        backend: &B,
        name: &str,
        range: Option<Range<usize>>,
    ) -> Result<B::Buf> {
        let view = self.view(name)?;
        let &[len] = view.shape() else {
            return Err(Error::Checkpoint(format!(
                "{name}: expected a rank-1 tensor, got {:?}",
                view.shape()
            )));
        };
        let values = to_f32(view.dtype(), view.data())?;
        let range = range.unwrap_or(0..len);
        let slice = values
            .get(range.clone())
            .ok_or_else(|| Error::Checkpoint(format!("{name}: range {range:?} out of {len}")))?;
        Self::upload(backend, bytemuck::cast_slice(slice))
    }

    pub fn has(&self, name: &str) -> bool {
        self.st.tensor(name).is_ok()
    }
}
