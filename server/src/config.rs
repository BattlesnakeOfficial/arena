//! Centralized configuration.
//!
//! [`AppConfig::from_env`] is the single place the running server reads
//! environment variables. Everything downstream — request handlers, jobs,
//! the boot sequence — takes values from the `AppConfig` held in
//! `AppState`, so nothing reaches for `std::env` deep in the process. This
//! keeps configuration discoverable in one struct and makes handlers
//! testable without touching global env.
//!
//! (The one exception is the `import-play` subcommand, which is a separate
//! one-shot entry point that exits before `AppState` is built.)

use cja::jobs::worker::{DEFAULT_LOCK_TIMEOUT, DEFAULT_MAX_RETRIES};

use crate::email::MailgunConfig;
use crate::github::auth::GitHubOAuthConfig;

/// Background job worker tuning.
#[derive(Clone, Debug)]
pub struct JobConfig {
    pub poll_interval_ms: u64,
    pub lock_timeout_secs: u64,
    pub max_retries: i32,
    pub workers: usize,
}

/// Which long-running components to start. Driven by `<FEATURE>_DISABLED`
/// env vars (a feature is on unless explicitly disabled).
#[derive(Clone, Copy, Debug)]
pub struct FeatureFlags {
    pub server: bool,
    pub jobs: bool,
    pub cron: bool,
}

/// All resolved configuration, read once at boot.
#[derive(Clone, Debug)]
pub struct AppConfig {
    // Core
    pub database_url: String,
    pub pg_max_connections: u32,
    /// Public base URL, used to build the board-viewer iframe `engine=` param.
    pub base_url: String,

    // Optional services
    pub engine_database_url: Option<String>,
    pub gcs_bucket: Option<String>,
    pub github: Option<GitHubOAuthConfig>,
    pub mailgun: Option<MailgunConfig>,

    // Runtime / telemetry
    pub tokio_worker_multiplier: usize,
    pub gcp_logging: bool,
    pub gcp_project_id: Option<String>,
    pub rust_log: String,

    pub job: JobConfig,
    pub features: FeatureFlags,
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A feature is enabled unless `<FEATURE>_DISABLED` is exactly `"true"`.
fn feature_enabled(feature: &str) -> bool {
    std::env::var(format!("{feature}_DISABLED")).as_deref() != Ok("true")
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl AppConfig {
    /// Read all configuration from the environment. `DATABASE_URL` is the
    /// only hard requirement; everything else has a default or is optional.
    pub fn from_env() -> cja::Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| cja::color_eyre::eyre::eyre!("DATABASE_URL must be set"))?;

        Ok(Self {
            database_url,
            pg_max_connections: parse_env("ARENA_PG_MAX_CONNECTIONS", 5),
            base_url: std::env::var("BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),

            engine_database_url: optional_env("ENGINE_DATABASE_URL"),
            gcs_bucket: optional_env("GCS_BUCKET"),
            github: github_config_from_env(),
            mailgun: mailgun_config_from_env(),

            tokio_worker_multiplier: parse_env("ARENA_TOKIO_WORKER_MULTIPLIER", 2),
            gcp_logging: std::env::var("GCP_LOGGING").is_ok(),
            gcp_project_id: optional_env("GCP_PROJECT_ID"),
            rust_log: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),

            job: JobConfig {
                poll_interval_ms: parse_env("ARENA_JOB_POLL_INTERVAL_MS", 60_000),
                lock_timeout_secs: parse_env(
                    "ARENA_JOB_LOCK_TIMEOUT_SECS",
                    DEFAULT_LOCK_TIMEOUT.as_secs(),
                ),
                max_retries: parse_env("ARENA_JOB_MAX_RETRIES", DEFAULT_MAX_RETRIES),
                workers: parse_env::<usize>("ARENA_JOB_WORKERS", 1).max(1),
            },
            features: FeatureFlags {
                server: feature_enabled("SERVER"),
                jobs: feature_enabled("JOBS"),
                cron: feature_enabled("CRON"),
            },
        })
    }
}

#[cfg(test)]
impl AppConfig {
    /// Inert config for tests: no external services, sensible defaults.
    pub fn test_default() -> Self {
        Self {
            database_url: String::new(),
            pg_max_connections: 5,
            base_url: "http://localhost:3000".to_string(),
            engine_database_url: None,
            gcs_bucket: None,
            github: None,
            mailgun: None,
            tokio_worker_multiplier: 2,
            gcp_logging: false,
            gcp_project_id: None,
            rust_log: "info".to_string(),
            job: JobConfig {
                poll_interval_ms: 60_000,
                lock_timeout_secs: 7200,
                max_retries: 20,
                workers: 1,
            },
            features: FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        }
    }
}

/// Build the GitHub OAuth config, or `None` if the required credentials
/// aren't set (auth is then disabled). The URL fields are overridable so
/// tests can point at a mock OAuth server.
fn github_config_from_env() -> Option<GitHubOAuthConfig> {
    let client_id = optional_env("GITHUB_CLIENT_ID")?;
    let client_secret = optional_env("GITHUB_CLIENT_SECRET")?;
    let redirect_uri = optional_env("GITHUB_REDIRECT_URI")?;

    Some(GitHubOAuthConfig {
        client_id,
        client_secret,
        redirect_uri,
        oauth_url: std::env::var("GITHUB_OAUTH_URL")
            .unwrap_or_else(|_| "https://github.com/login/oauth/authorize".to_string()),
        token_url: std::env::var("GITHUB_TOKEN_URL")
            .unwrap_or_else(|_| "https://github.com/login/oauth/access_token".to_string()),
        api_url: std::env::var("GITHUB_API_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_string()),
    })
}

/// Build the Mailgun config, or `None` when `MAILGUN_API_KEY` is unset
/// (email then no-ops). Other fields fall back to defaults.
fn mailgun_config_from_env() -> Option<MailgunConfig> {
    let api_key = optional_env("MAILGUN_API_KEY")?;
    Some(MailgunConfig {
        api_key,
        domain: std::env::var("MAILGUN_DOMAIN")
            .unwrap_or_else(|_| "mg.battlesnake.com".to_string()),
        from: std::env::var("MAILGUN_FROM")
            .unwrap_or_else(|_| "Battlesnake Arena <reply@battlesnake.com>".to_string()),
        base_url: std::env::var("MAILGUN_BASE_URL")
            .unwrap_or_else(|_| "https://api.mailgun.net".to_string()),
    })
}
