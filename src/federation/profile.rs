//! Local execution-profile provenance. Neither permission nor remote attestation.
use super::canonical_json::Value;
use crate::cache::{Stamp, fingerprint};
use sha2::{Digest, Sha256};
use std::{fs::File, io, sync::Arc};

mod executable;
pub(crate) mod gpu;

/// SHA-256 of the complete actual running executable, not MDMA_BUILD_ID,
/// an operator-supplied path, a VCS token or a library's source archive.
/// The pinned descriptor is retained for current provenance checks.
pub struct ExecutorIdentity {
    file: File,
    stamp: Stamp,
    build_id: String,
}

impl ExecutorIdentity {
    /// No pathname fallback when the platform cannot identify the running image.
    pub fn capture() -> io::Result<Self> {
        let mut file = executable::open()?;
        let (hash, stamp) = fingerprint(&mut file)?;
        executable::validate(&file)?;
        stamp.check(file.metadata()?)?;
        Ok(Self {
            file,
            stamp,
            build_id: format!("sha256:{hash}"),
        })
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    fn current(&self) -> bool {
        self.file
            .metadata()
            .is_ok_and(|metadata| self.stamp.check(metadata).is_ok())
            && executable::validate(&self.file).is_ok()
    }
}

/// A loader-created canonical profile. No public constructor/deserializer.
#[derive(Clone)]
pub struct ExecutionProfile {
    id: String,
    manifest: Value,
    executor: Arc<ExecutorIdentity>,
    gpu: Option<gpu::Gpu>,
}

impl ExecutionProfile {
    pub fn as_str(&self) -> &str {
        &self.id
    }

    pub fn manifest(&self) -> &Value {
        &self.manifest
    }

    pub fn build_id(&self) -> &str {
        self.executor.build_id()
    }

    /// Revalidate the executable snapshot. Model availability still requires
    /// the current registry getter; this does not authorize an export or
    /// attest a remote executor.
    pub fn is_current(&self) -> bool {
        self.executor.current() && self.gpu.as_ref().is_none_or(gpu::Gpu::current)
    }

    pub(crate) fn gpu(executor: Arc<ExecutorIdentity>, gpu: gpu::Gpu) -> Self {
        let manifest = Value::object([
            (
                "format".into(),
                Value::String("mdma.execution-profile.v1".into()),
            ),
            ("build_id".into(), Value::String(executor.build_id.clone())),
            ("backend".into(), Value::String(gpu.backend.into())),
            (
                "activation_dtype".into(),
                Value::String(gpu.activation.into()),
            ),
            ("kv_dtype".into(), Value::String(gpu.kv.into())),
            ("modes".into(), gpu.modes.clone()),
        ])
        .expect("fixed unique GPU profile fields");
        let id = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(manifest.encode().as_bytes()))
        );
        Self {
            id,
            manifest,
            executor,
            gpu: Some(gpu),
        }
    }

    pub(crate) fn cpu(executor: Arc<ExecutorIdentity>, laya: bool) -> Self {
        // Vocabulary is fixed by the actual CpuBackend implementation, not
        // caller strings/device labels/environment claims. Geometry, source
        // quantization, RoPE parameters and calibration belong to artifact_id.
        let modes = Value::object([
            ("vocabulary".into(), Value::String("mdma.cpu.v1".into())),
            (
                "workload".into(),
                Value::String(if laya { "laya" } else { "decoder" }.into()),
            ),
            ("execution".into(), Value::String("eager".into())),
            (
                "weights".into(),
                Value::String(
                    if laya {
                        "safetensors-raw-matrices-f32-norms"
                    } else {
                        "gguf-raw-matrices-f32-norms"
                    }
                    .into(),
                ),
            ),
            (
                "matmul".into(),
                Value::String(
                    // Laya's f32 weights never take the packed path.
                    match (laya, crate::backend::cpu::matmul()) {
                        (false, crate::backend::cpu::Matmul::Packed) => {
                            "rayon-rows-packed-q8-activation-blocks-else-16-lane-f32-dot"
                        }
                        _ => "rayon-rows-16-lane-f32-dot",
                    }
                    .into(),
                ),
            ),
            (
                "attention".into(),
                Value::String("scalar-sequential-f32-softmax".into()),
            ),
            (
                "normalization".into(),
                Value::String("scalar-sequential-f32".into()),
            ),
            (
                "activation".into(),
                Value::String("mdma-native-activations-v1".into()),
            ),
            (
                "rope".into(),
                Value::String("scalar-f32-powf-sin-cos".into()),
            ),
            (
                "math".into(),
                Value::String("rust-native-system-math".into()),
            ),
            (
                "target_arch".into(),
                Value::String(std::env::consts::ARCH.into()),
            ),
            (
                "target_os".into(),
                Value::String(std::env::consts::OS.into()),
            ),
            (
                "batching".into(),
                Value::String(
                    if laya {
                        "serial-chunks-16"
                    } else {
                        "native-request-prefill"
                    }
                    .into(),
                ),
            ),
            (
                "decisions".into(),
                Value::String(
                    if laya {
                        "f32-temperature-softmax-f64-confidence-round4"
                    } else {
                        "none"
                    }
                    .into(),
                ),
            ),
        ])
        .expect("fixed unique mode fields");
        let manifest = Value::object([
            (
                "format".into(),
                Value::String("mdma.execution-profile.v1".into()),
            ),
            ("build_id".into(), Value::String(executor.build_id.clone())),
            ("backend".into(), Value::String("cpu".into())),
            ("activation_dtype".into(), Value::String("f32".into())),
            (
                "kv_dtype".into(),
                Value::String(if laya { "none" } else { "f32" }.into()),
            ),
            ("modes".into(), modes),
        ])
        .expect("fixed unique profile fields");
        let id = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(manifest.encode().as_bytes()))
        );
        Self {
            id,
            manifest,
            executor,
            gpu: None,
        }
    }
}

#[cfg(test)]
mod tests;
