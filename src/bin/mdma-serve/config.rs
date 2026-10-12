use mdma::power::{DegradedLimits, GpuProbe, WatchPolicy};
use rama::error::BoxError;
use rama::net::address::SocketAddress;
use std::env::VarError;
use std::time::Duration;

/// Output tokens one request may ask for; its KV is reserved up front.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;
/// "mdma" on a phone keypad.
const DEFAULT_LISTEN: &str = "127.0.0.1:6362";
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u32 = 30;
const MAX_SHUTDOWN_TIMEOUT_SECS: u32 = 86400;
const MAX_POWER_POLL_SECS: u32 = 3600;
const MAX_POWER_DEBOUNCE: u32 = 100;

/// The `MDMA_MODELS_DIR` folder, or none for control-only federation.
#[derive(Debug)]
pub(super) enum ModelsSource {
    Dir { path: String, default: Option<String> },
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
    /// MDMA_OPERATOR_SPKI pins.
    pub operators: Vec<String>,
    /// MDMA_NODE_NAME, or the short host name.
    pub node: String,
    pub backend: String,
    /// Validated on every build so `MDMA_HIP_F32` typos fail startup; read by ROCm.
    #[cfg_attr(not(any(feature = "rocm", test)), expect(dead_code, reason = "consumed only by the ROCm backend"))]
    pub rocm_f32: bool,
    /// MDMA_N_CTX lowers every model's context; `None` uses each model's trained context.
    pub n_ctx: Option<usize>,
    /// MDMA_MAX_OUTPUT_TOKENS: the most tokens one request may generate.
    pub max_output_tokens: usize,
    pub native_tensor_bytes: Option<usize>,
    pub device_buffer_bytes: Option<usize>,
    pub host_buffer_bytes: Option<usize>,
    pub shutdown_timeout: Duration,
    pub warmup_rounds: u32,
    /// Unload a model unused this long; `None` keeps models until evicted.
    pub idle_unload: Option<Duration>,
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
        let required = |name: &str| super::vars::required(&env, name);
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
        let optional_positive = |name: &str, max: u32| -> Result<Option<u32>, BoxError> {
            match env(name) {
                Ok(value) => value.parse::<u32>().ok().filter(|n| (1..=max).contains(n)).map(Some)
                    .ok_or_else(|| format!("{name} must be between 1 and {max}").into()),
                Err(VarError::NotPresent) => Ok(None),
                Err(e) => Err(format!("{name}: {e}").into()),
            }
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
        let flag = |name: &str| -> Result<bool, BoxError> {
            match optional(name, "0")?.as_str() {
                "0" => Ok(false),
                "1" => Ok(true),
                _ => Err(format!("{name} must be 0 or 1").into()),
            }
        };
        let control_only = flag("MDMA_FEDERATION_CONTROL_ONLY")?;
        let models = if control_only {
            if !matches!(env("MDMA_MODELS_DIR"), Err(VarError::NotPresent)) {
                return Err("control-only federation cannot load model sources".into());
            }
            ModelsSource::ControlOnly
        } else {
            let default = match env("MDMA_DEFAULT_MODEL") {
                Ok(name) if name.trim().is_empty() => return Err("MDMA_DEFAULT_MODEL must not be empty".into()),
                Ok(name) => Some(name.trim().to_owned()),
                Err(VarError::NotPresent) => None,
                Err(e) => return Err(format!("MDMA_DEFAULT_MODEL: {e}").into()),
            };
            ModelsSource::Dir { path: required("MDMA_MODELS_DIR")?, default }
        };
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
        let rocm_f32 = flag("MDMA_HIP_F32")?;
        if (device_buffer_bytes.is_some() || host_buffer_bytes.is_some()) && !matches!(backend.as_str(), "rocm" | "cuda" | "vulkan") {
            return Err("MDMA_DEVICE_BUFFER_BYTES / MDMA_HOST_BUFFER_BYTES require rocm, cuda or vulkan".into());
        }
        let warmup_rounds = match env("MDMA_WARMUP_ROUNDS") {
            Ok(value) => value.parse::<u32>().ok().filter(|n| *n <= mdma::registry::MAX_WARMUP_ROUNDS)
                .ok_or_else(|| format!("MDMA_WARMUP_ROUNDS must be between 0 and {}", mdma::registry::MAX_WARMUP_ROUNDS))?,
            Err(VarError::NotPresent) => 1,
            Err(e) => return Err(format!("MDMA_WARMUP_ROUNDS: {e}").into()),
        };
        let idle_unload = match env("MDMA_IDLE_UNLOAD_SECS") {
            Ok(value) => match value.parse::<u64>() {
                Ok(0) => None,
                Ok(secs) if secs <= 86_400 => Some(Duration::from_secs(secs)),
                _ => return Err("MDMA_IDLE_UNLOAD_SECS must be between 0 and 86400".into()),
            },
            Err(VarError::NotPresent) => None,
            Err(e) => return Err(format!("MDMA_IDLE_UNLOAD_SECS: {e}").into()),
        };
        let federation_config = match env("MDMA_FEDERATION_CONFIG") {
            Ok(path) if !path.trim().is_empty() => Some(path),
            Ok(_) => return Err("MDMA_FEDERATION_CONFIG must not be empty".into()),
            Err(VarError::NotPresent) => None,
            Err(error) => return Err(format!("MDMA_FEDERATION_CONFIG: {error}").into()),
        };
        let operators = match env("MDMA_OPERATOR_SPKI") {
            Ok(list) => list.split(',').map(str::trim).filter(|pin| !pin.is_empty()).map(|pin| {
                let valid = pin.strip_prefix("sha256:")
                    .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
                if valid { Ok(pin.to_owned()) } else {
                    Err(format!("MDMA_OPERATOR_SPKI: {pin:?} is not a sha256:<64 lowercase hex> pin (see `mdma-certs --spki`)"))
                }
            }).collect::<Result<Vec<_>, _>>()?,
            Err(VarError::NotPresent) => Vec::new(),
            Err(e) => return Err(format!("MDMA_OPERATOR_SPKI: {e}").into()),
        };
        let node = match env("MDMA_NODE_NAME") {
            Ok(name) if name.trim().is_empty() => return Err("MDMA_NODE_NAME must not be empty".into()),
            Ok(name) => name.trim().to_owned(),
            Err(VarError::NotPresent) => sysinfo::System::host_name()
                .map(|host| host.split('.').next().unwrap_or_default().to_owned())
                .unwrap_or_default(),
            Err(e) => return Err(format!("MDMA_NODE_NAME: {e}").into()),
        };
        if control_only && (federation_config.is_none() || backend != "cpu") {
            return Err("control-only federation requires MDMA_FEDERATION_CONFIG and cpu backend".into());
        }
        Ok(Self {
            models,
            listen: match env("MDMA_LISTEN") {
                Ok(addr) => addr,
                Err(VarError::NotPresent) => DEFAULT_LISTEN.to_owned(),
                Err(e) => return Err(format!("MDMA_LISTEN: {e}").into()),
            }
            .parse()
            .map_err(|e| format!("MDMA_LISTEN: {e}"))?,
            tls_cert: required("MDMA_TLS_CERT")?,
            tls_key: required("MDMA_TLS_KEY")?,
            tls_ca: required("MDMA_TLS_CA")?,
            federation_config,
            operators,
            node,
            backend,
            rocm_f32,
            n_ctx: optional_positive("MDMA_N_CTX", u32::MAX)?.map(|n| n as usize),
            max_output_tokens: positive("MDMA_MAX_OUTPUT_TOKENS", DEFAULT_MAX_OUTPUT_TOKENS, u32::MAX)? as usize,
            native_tensor_bytes,
            device_buffer_bytes,
            host_buffer_bytes,
            warmup_rounds,
            idle_unload,
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
