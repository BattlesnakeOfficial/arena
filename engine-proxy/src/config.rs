//! Environment-driven configuration, read once at boot.
//!
//! Every knob is an env var with a default matching the Go proxy unless noted
//! in `docs/engine-proxy.md`. The auth token is required and aborts boot when
//! missing, exactly like Go's `proxy_server` command.

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{WrapErr, eyre};

/// Default request/response body cap: 1 MiB. Arena reads snake responses
/// capped at 64 KiB anyway; the cap exists so a hostile snake cannot OOM the
/// proxy with an unbounded stream.
pub const DEFAULT_BODY_CAP_BYTES: usize = 1024 * 1024;

/// Upper bound for the client-supplied `X-Battlesnake-Timeout-Ms` budget.
/// Arena sends per-move timeouts (typically 500ms); the clamp protects the
/// proxy from a bogus header requesting minutes of resources.
pub const DEFAULT_MAX_TIMEOUT_MS: i64 = 10_000;

/// Overall cap on one connector call's DNS+connect race. Exists to bound the
/// *background* dials hyper-util spawns when its checkout-vs-dial race loses:
/// a blackholed host would otherwise hold a task + socket for the kernel's
/// ~2-minute SYN-retry window. On-path dials are bounded by the request
/// deadline first. Raise in step with `ENGINE_PROXY_MAX_TIMEOUT_MS`.
pub const DEFAULT_DIAL_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone)]
pub struct ProxyConfig {
    pub auth_token: String,
    pub app_version: String,
    pub health_check_urls: Vec<String>,
    pub pool_idle_per_host: usize,
    pub pool_idle_timeout: Duration,
    pub dial_timeout: Duration,
    pub body_cap_bytes: usize,
    pub max_timeout_ms: i64,
    /// Optional additional CA bundle (PEM) appended to the webpki root store,
    /// e.g. for mock snakes during load checks.
    pub extra_ca: Option<PathBuf>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("auth_token", &"[redacted]")
            .field("app_version", &self.app_version)
            .field("health_check_urls", &self.health_check_urls)
            .field("pool_idle_per_host", &self.pool_idle_per_host)
            .field("pool_idle_timeout", &self.pool_idle_timeout)
            .field("dial_timeout", &self.dial_timeout)
            .field("body_cap_bytes", &self.body_cap_bytes)
            .field("max_timeout_ms", &self.max_timeout_ms)
            .field("extra_ca", &self.extra_ca)
            .finish()
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            auth_token: String::new(),
            app_version: "unknown".to_string(),
            health_check_urls: Vec::new(),
            pool_idle_per_host: 8,
            pool_idle_timeout: Duration::from_secs(90),
            dial_timeout: Duration::from_millis(DEFAULT_DIAL_TIMEOUT_MS),
            body_cap_bytes: DEFAULT_BODY_CAP_BYTES,
            max_timeout_ms: DEFAULT_MAX_TIMEOUT_MS,
            extra_ca: None,
        }
    }
}

/// Upper bound the dial cap will accept from the environment. `Instant`
/// arithmetic on an absurd cap could overflow when deadlines are computed —
/// validated in `from_env` so the connector never has to panic on it
/// (PR #277 round 1).
const MAX_DIAL_TIMEOUT_MS: u64 = 60 * 60 * 1000;

impl ProxyConfig {
    /// Read configuration from the environment. Missing
    /// `BATTLESNAKE_PROXY_AUTH_TOKEN` is an error so `main` exits non-zero,
    /// matching Go.
    pub fn from_env() -> color_eyre::Result<Self> {
        let auth_token = std::env::var("BATTLESNAKE_PROXY_AUTH_TOKEN")
            .wrap_err("BATTLESNAKE_PROXY_AUTH_TOKEN not set in environment")?;
        if auth_token.is_empty() {
            return Err(eyre!("BATTLESNAKE_PROXY_AUTH_TOKEN is empty"));
        }
        let app_version = std::env::var("APP_VERSION")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let mut cfg = Self {
            auth_token,
            app_version,
            ..Self::default()
        };
        if let Ok(v) = std::env::var("HEALTH_CHECK_URLS") {
            cfg.health_check_urls = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Some(v) = env_parse("ENGINE_PROXY_POOL_IDLE_PER_HOST")? {
            cfg.pool_idle_per_host = v;
        }
        if let Some(v) = env_parse("ENGINE_PROXY_POOL_IDLE_TIMEOUT_SECS")? {
            cfg.pool_idle_timeout = Duration::from_secs(v);
        }
        if let Some(v) = env_parse("ENGINE_PROXY_BODY_CAP_BYTES")? {
            cfg.body_cap_bytes = v;
        }
        if let Some(v) = env_parse("ENGINE_PROXY_MAX_TIMEOUT_MS")? {
            cfg.max_timeout_ms = v;
        }
        if let Some(v) = env_parse("ENGINE_PROXY_DIAL_TIMEOUT_MS")? {
            if v == 0 || v > MAX_DIAL_TIMEOUT_MS {
                return Err(eyre!(
                    "Invalid ENGINE_PROXY_DIAL_TIMEOUT_MS={v}: must be between 1 and {MAX_DIAL_TIMEOUT_MS} ms"
                ));
            }
            cfg.dial_timeout = Duration::from_millis(v);
        }
        if let Ok(v) = std::env::var("ENGINE_PROXY_EXTRA_CA")
            && !v.is_empty()
        {
            cfg.extra_ca = Some(PathBuf::from(v));
        }
        Ok(cfg)
    }
}

fn env_parse<T>(name: &str) -> color_eyre::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v
            .parse::<T>()
            .map(Some)
            .map_err(|e| eyre!("Invalid {name}={v}: {e}")),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_token() {
        let cfg = ProxyConfig {
            auth_token: "super-secret-token".to_string(),
            ..ProxyConfig::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("[redacted]"), "{rendered}");
    }
}
