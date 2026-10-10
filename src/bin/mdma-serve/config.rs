use mdma::power::{DegradedLimits, GpuProbe, WatchPolicy};
use rama::error::BoxError;
use rama::net::address::SocketAddress;
use std::env::VarError;
use std::time::Duration;

const DEFAULT_N_CTX: u32 = 4096;
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u32 = 30;
const MAX_SHUTDOWN_TIMEOUT_SECS: u32 = 86400;
const MAX_POWER_POLL_SECS: u32 = 3600;
const MAX_POWER_DEBOUNCE: u32 = 100;

/// A `MDMA_MODELS` registry file or a single `MDMA_GGUF_PATH` model.
#[derive(Debug)]
pub(super) enum ModelsSource {
    File(String),
    Gguf(String),
    ControlOnly,
}

#[expect(clippy::struct_field_names, reason = "`federation_config` mirrors MDMA_FEDERATION_CONFIG")]
pub(super) struct Config {
    pub models: ModelsSource,
    pub listen: SocketAddress,
    pub tls_cert: String,
    pub tls_key: String,
    pub tls_ca: String,
    pub federation_config: Option<String>,
    pub backend: String,
    /// Validated on every build so `MDMA_HIP_F32` typos fail startup; read by ROCm.
    #[cfg_attr(not(any(feature = "rocm", test)), expect(dead_code, reason = "consumed only by the ROCm backend"))]
    pub rocm_f32: bool,
    pub n_ctx: usize,
    pub native_tensor_bytes: Option<usize>,
    pub device_buffer_bytes: Option<usize>,
    pub host_buffer_bytes: Option<usize>,
    pub shutdown_timeout: Duration,
    pub warmup_rounds: u32,
    /// `None` when `MDMA_POWER=off`: no watcher, always serving.
    pub power: Option<PowerConfig>,
}

pub(super) struct PowerConfig {
    pub watch: WatchPolicy,
    pub degraded: DegradedLimits,
    /// Set by the backend that can read its GPU's power limit (cuda).
    pub gpu: Option<Box<dyn GpuProbe>>,
}

impl Config {
    pub fn load(env: impl Fn(&str) -> Result<String, VarError>) -> Result<Self, BoxError> {
        let required = |name: &str| -> Result<String, BoxError> {
            let value = env(name).map_err(|e| format!("{name}: {e}"))?;
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty").into());
            }
            Ok(value)
        };
        let optional = |name: &str, default: &str| -> Result<String, BoxError> {
            match env(name) {
                Ok(value) => Ok(value),
                Err(VarError::NotPresent) => Ok(default.to_string()),
                Err(e) => Err(format!("{name}: {e}").into()),
            }
        };
        let positive = |name: &str, default: u32, max: u32| -> Result<u32, BoxError> {
            let value = match env(name) {
                Ok(value) => value.parse::<u32>().map_err(|e| format!("{name}: {e}"))?,
                Err(VarError::NotPresent) => default,
                Err(e) => return Err(format!("{name}: {e}").into()),
            };
            if value == 0 || value > max {
                return Err(format!("{name} must be between 1 and {max}").into());
            }
            Ok(value)
        };
        let percentage = |name: &str, default: u8, max: u8| -> Result<u8, BoxError> {
            match env(name) {
                Ok(value) => value.parse::<u8>().ok().filter(|p| *p <= max)
                    .ok_or_else(|| format!("{name} must be between 0 and {max}").into()),
                Err(VarError::NotPresent) => Ok(default),
                Err(e) => Err(format!("{name}: {e}").into()),
            }
        };
        let power = match optional("MDMA_POWER", "auto")?.as_str() {
            "off" => None,
            "auto" => {
                let watch = WatchPolicy::default();
                let degraded = DegradedLimits::default();
                let critical_percent = percentage("MDMA_POWER_CRITICAL_PCT", watch.critical_percent, 99)?;
                let gpu_cap_percent = percentage("MDMA_POWER_GPU_CAP_PCT", watch.gpu_cap_percent, 100)?;
                Some(PowerConfig {
                    watch: WatchPolicy {
                        poll: Duration::from_secs(
                            u64::from(positive("MDMA_POWER_POLL_SECS", watch.poll.as_secs() as u32, MAX_POWER_POLL_SECS)?)
                        ),
                        debounce: positive("MDMA_POWER_DEBOUNCE", watch.debounce, MAX_POWER_DEBOUNCE)?,
                        critical_percent,
                        gpu_cap_percent,
                    },
                    degraded: DegradedLimits {
                        max_tokens: positive("MDMA_DEGRADED_MAX_TOKENS", degraded.max_tokens as u32, u32::MAX)? as usize,
                        n_ctx: positive("MDMA_DEGRADED_N_CTX", degraded.n_ctx as u32, u32::MAX)? as usize,
                    },
                    gpu: None,
                })
            }
            other => return Err(format!("MDMA_POWER must be auto or off, not '{other}'").into()),
        };
        let control_only = match optional("MDMA_FEDERATION_CONTROL_ONLY", "0")?.as_str() {
            "0" => false,
            "1" => true,
            _ => return Err("MDMA_FEDERATION_CONTROL_ONLY must be 0 or 1".into()),
        };
        let models = if control_only {
            if !matches!(env("MDMA_MODELS"), Err(VarError::NotPresent))
                || !matches!(env("MDMA_GGUF_PATH"), Err(VarError::NotPresent)) {
                return Err("control-only federation cannot load model sources".into());
            }
            ModelsSource::ControlOnly
        } else { match env("MDMA_MODELS") {
            Ok(path) if !path.trim().is_empty() => ModelsSource::File(path),
            Ok(_) => return Err("MDMA_MODELS must not be empty".into()),
            Err(VarError::NotPresent) => ModelsSource::Gguf(required("MDMA_GGUF_PATH").map_err(|e| format!("{e} (or set MDMA_MODELS)"))?),
            Err(e) => return Err(format!("MDMA_MODELS: {e}").into()),
        }};
        let byte_limit = |name: &str| -> Result<Option<usize>, BoxError> {
            match env(name) {
                Ok(value) => Ok(Some(value.parse::<usize>().ok().filter(|n| *n > 0)
                    .ok_or_else(|| format!("{name} must be a positive integer byte count fitting usize"))?)),
                Err(VarError::NotPresent) => Ok(None),
                Err(e) => Err(format!("{name}: {e}").into()),
            }
        };
        let native_tensor_bytes = byte_limit("MDMA_NATIVE_TENSOR_BYTES")?;
        let device_buffer_bytes = byte_limit("MDMA_DEVICE_BUFFER_BYTES")?;
        let host_buffer_bytes = byte_limit("MDMA_HOST_BUFFER_BYTES")?;
        let backend = optional("MDMA_BACKEND", "cpu")?;
        let rocm_f32 = match optional("MDMA_HIP_F32", "0")?.as_str() {
            "0" => false,
            "1" => true,
            _ => return Err("MDMA_HIP_F32 must be 0 or 1".into()),
        };
        if (device_buffer_bytes.is_some() || host_buffer_bytes.is_some()) && !matches!(backend.as_str(), "rocm" | "cuda" | "vulkan") {
            return Err("MDMA_DEVICE_BUFFER_BYTES / MDMA_HOST_BUFFER_BYTES require rocm, cuda or vulkan".into());
        }
        let warmup_rounds = match env("MDMA_WARMUP_ROUNDS") {
            Ok(value) => value.parse::<u32>().ok().filter(|n| *n <= mdma::registry::MAX_WARMUP_ROUNDS)
                .ok_or_else(|| format!("MDMA_WARMUP_ROUNDS must be between 0 and {}", mdma::registry::MAX_WARMUP_ROUNDS))?,
            Err(VarError::NotPresent) => 2,
            Err(e) => return Err(format!("MDMA_WARMUP_ROUNDS: {e}").into()),
        };
        let federation_config = match env("MDMA_FEDERATION_CONFIG") {
            Ok(path) if !path.trim().is_empty() => Some(path),
            Ok(_) => return Err("MDMA_FEDERATION_CONFIG must not be empty".into()),
            Err(VarError::NotPresent) => None,
            Err(error) => return Err(format!("MDMA_FEDERATION_CONFIG: {error}").into()),
        };
        if control_only && (federation_config.is_none() || backend != "cpu") {
            return Err("control-only federation requires MDMA_FEDERATION_CONFIG and cpu backend".into());
        }
        Ok(Self {
            models,
            listen: required("MDMA_LISTEN")?
                .parse()
                .map_err(|e| format!("MDMA_LISTEN: {e}"))?,
            tls_cert: required("MDMA_TLS_CERT")?,
            tls_key: required("MDMA_TLS_KEY")?,
            tls_ca: required("MDMA_TLS_CA")?,
            federation_config,
            backend,
            rocm_f32,
            n_ctx: positive("MDMA_N_CTX", DEFAULT_N_CTX, u32::MAX)? as usize,
            native_tensor_bytes,
            device_buffer_bytes,
            host_buffer_bytes,
            warmup_rounds,
            shutdown_timeout: Duration::from_secs(u64::from(positive(
                "MDMA_SHUTDOWN_TIMEOUT_SECS",
                DEFAULT_SHUTDOWN_TIMEOUT_SECS,
                MAX_SHUTDOWN_TIMEOUT_SECS,
            )?)),
            power,
        })
    }
}

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
