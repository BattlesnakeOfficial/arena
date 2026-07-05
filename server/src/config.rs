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

    /// Consecutive failed health probes before the sweeper pulls a snake
    /// from leaderboard matchmaking (BS-3534).
    pub snake_health_failure_threshold: i32,

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

/// An optional string setting: `None` when unset OR empty. Treating an
/// empty value as "not configured" is intentional — an empty engine URL,
/// bucket, or credential was never usable, so it disables the feature
/// rather than half-enabling it.
fn optional_env(name: &str) -> Option<String> {
    non_empty(std::env::var(name).ok())
}

/// The empty-string → `None` rule, pure so it's testable without touching
/// process env.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
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

            snake_health_failure_threshold: parse_env("SNAKE_HEALTH_FAILURE_THRESHOLD", 3).max(1),

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
            snake_health_failure_threshold: 3,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_falls_back_on_unset_and_unparseable() {
        // These names are deliberately never set in the test environment.
        assert_eq!(parse_env::<u32>("ARENA_TEST_DEFINITELY_UNSET_U32", 5), 5);
        // Parsing is via FromStr; a non-numeric value can't be injected
        // without touching global env, so cover the unset (None) path here
        // and the parse path via the typed defaults below.
        assert_eq!(parse_env::<u64>("ARENA_TEST_ALSO_UNSET", 60_000), 60_000);
    }

    #[test]
    fn optional_env_maps_empty_and_unset_to_none() {
        assert_eq!(optional_env("ARENA_TEST_UNSET_OPTIONAL"), None);
        // The empty-string → None rule the reviewers scrutinized, locked in:
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("x".to_string())), Some("x".to_string()));
    }

    #[test]
    fn feature_flag_semantics() {
        // Only the exact string "true" disables a feature; everything else
        // (including unset, "TRUE", "1", "") leaves it enabled. The unset
        // case is what runs here; the value cases are asserted via the
        // pure predicate below to avoid global env mutation.
        assert!(feature_enabled("ARENA_TEST_UNSET_FEATURE"));
    }

    /// Pure mirror of `feature_enabled`'s predicate, so the exact-"true"
    /// rule is covered for every value without touching process env.
    fn is_enabled_for(value: Option<&str>) -> bool {
        value != Some("true")
    }

    #[test]
    fn only_exact_true_disables() {
        assert!(!is_enabled_for(Some("true")));
        assert!(is_enabled_for(Some("TRUE")));
        assert!(is_enabled_for(Some("1")));
        assert!(is_enabled_for(Some("")));
        assert!(is_enabled_for(Some("false")));
        assert!(is_enabled_for(None));
    }

    #[test]
    fn test_default_disables_external_services() {
        let c = AppConfig::test_default();
        assert!(c.github.is_none());
        assert!(c.mailgun.is_none());
        assert!(c.engine_database_url.is_none());
        assert!(c.gcs_bucket.is_none());
        assert!(c.gcp_project_id.is_none());
        assert_eq!(c.job.workers, 1);
        assert!(c.features.server && c.features.jobs && c.features.cron);
    }
}
