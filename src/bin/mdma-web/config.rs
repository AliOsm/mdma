use rama::error::BoxError;
use rama::net::address::SocketAddress;
use std::env::VarError;
use std::net::SocketAddr;
use std::time::Duration;

const DEFAULT_LISTEN: &str = "127.0.0.1:6364";
const DEFAULT_SERVER: &str = "localhost:6362";
const DEFAULT_MAX_STREAMS: u32 = 2;
const MAX_STREAMS: u32 = 8;
const DEFAULT_WATCH_MS: u32 = 2000;
const WATCH_MS: std::ops::RangeInclusive<u32> = 250..=60_000;

pub(super) struct Config {
    /// Loopback only.
    pub listen: SocketAddr,
    /// `https://host:port` of `mdma-serve`.
    pub server: String,
    pub tls_cert: String,
    pub tls_key: String,
    pub tls_ca: String,
    /// Concurrent playground generations.
    pub max_streams: usize,
    /// `Watch` snapshot interval.
    pub watch_interval: Duration,
}

impl Config {
    pub fn load(env: impl Fn(&str) -> Result<String, VarError>) -> Result<Self, BoxError> {
        let required = |name: &str| super::vars::required(&env, name);
        let optional = |name: &str, default: &str| -> Result<String, BoxError> {
            match env(name) {
                Ok(value) if value.trim().is_empty() => {
                    Err(format!("{name} must not be empty").into())
                }
                Ok(value) => Ok(value),
                Err(VarError::NotPresent) => Ok(default.to_owned()),
                Err(error) => Err(format!("{name}: {error}").into()),
            }
        };
        let bounded = |name: &str,
                       default: u32,
                       range: std::ops::RangeInclusive<u32>|
         -> Result<u32, BoxError> {
            optional(name, &default.to_string())?
                .parse::<u32>()
                .ok()
                .filter(|n| range.contains(n))
                .ok_or_else(|| {
                    format!(
                        "{name} must be an integer between {} and {}",
                        range.start(),
                        range.end()
                    )
                    .into()
                })
        };

        let listen = optional("MDMA_WEB_LISTEN", DEFAULT_LISTEN)?;
        let listen = listen
            .parse::<SocketAddress>()
            .map_err(|e| format!("MDMA_WEB_LISTEN '{listen}' is not an ip:port address: {e}"))?
            .into_std();
        if !listen.ip().is_loopback() {
            return Err(format!(
                "MDMA_WEB_LISTEN must be a loopback address (127.0.0.1 or [::1]), got {listen}: \
                 the gateway acts with its client certificate for every browser that reaches it"
            )
            .into());
        }
        Ok(Self {
            listen,
            server: format!("https://{}", optional("MDMA_ADDR", DEFAULT_SERVER)?),
            tls_cert: required("MDMA_TLS_CERT")?,
            tls_key: required("MDMA_TLS_KEY")?,
            tls_ca: required("MDMA_TLS_CA")?,
            max_streams: bounded("MDMA_WEB_MAX_STREAMS", DEFAULT_MAX_STREAMS, 1..=MAX_STREAMS)?
                as usize,
            watch_interval: Duration::from_millis(u64::from(bounded(
                "MDMA_WEB_WATCH_MS",
                DEFAULT_WATCH_MS,
                WATCH_MS,
            )?)),
        })
    }
}

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
