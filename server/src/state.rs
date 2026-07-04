use std::sync::Arc;

use color_eyre::eyre::{Context as _, eyre};
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::config::AppConfig;
use crate::email::Mailer;
use crate::game_channels::GameChannels;

#[derive(Clone)]
pub struct AppState {
    /// All resolved configuration, read once at boot. Deep code reads
    /// settings from here instead of reaching for `std::env`.
    pub config: Arc<AppConfig>,
    pub db: sqlx::Pool<sqlx::Postgres>,
    pub cookie_key: cja::server::cookies::CookieKey,
    /// Connection to the legacy Battlesnake Engine database (for game backup)
    pub engine_db: Option<sqlx::Pool<sqlx::Postgres>>,
    /// Broadcast channels for live game updates
    pub game_channels: GameChannels,
    /// HTTP client for calling snake APIs
    pub http_client: reqwest::Client,
    /// Transactional email sender (no-op until Mailgun is configured)
    pub mailer: Mailer,
    /// Scoring algorithm registry
    pub scoring: std::sync::Arc<crate::scoring::ScoringRegistry>,
}

impl AppState {
    /// Turn resolved [`AppConfig`] into live resources (pools, clients).
    /// All environment reading already happened in [`AppConfig::from_env`].
    pub async fn from_config(config: AppConfig) -> cja::Result<Self> {
        async fn setup_db_pool(database_url: &str, max_connections: u32) -> cja::Result<PgPool> {
            const MIGRATION_LOCK_ID: i64 = 0xDB_DB_DB_DB_DB_DB_DB;

            let pool = PgPoolOptions::new()
                .max_connections(max_connections)
                .connect(database_url)
                .await?;

            sqlx::query!("SELECT pg_advisory_lock($1)", MIGRATION_LOCK_ID)
                .execute(&pool)
                .await?;

            sqlx::migrate!("../migrations").run(&pool).await?;

            let unlock_result = sqlx::query!("SELECT pg_advisory_unlock($1)", MIGRATION_LOCK_ID)
                .fetch_one(&pool)
                .await?
                .pg_advisory_unlock;

            match unlock_result {
                Some(b) => {
                    if b {
                        tracing::info!("Migration lock unlocked");
                    } else {
                        tracing::info!("Failed to unlock migration lock");
                    }
                }
                None => return Err(eyre!("Failed to unlock migration lock")),
            }

            Ok(pool)
        }

        let pool = setup_db_pool(&config.database_url, config.pg_max_connections).await?;

        let cookie_key = cja::server::cookies::CookieKey::from_env_or_generate()?;

        if config.github.is_some() {
            tracing::info!("GitHub OAuth configured");
        } else {
            tracing::warn!("GitHub OAuth not configured, auth will be disabled");
        }

        // Optional: Engine database for game backup
        let engine_db = match &config.engine_database_url {
            Some(url) => {
                tracing::info!("Connecting to Engine database for game backup");
                let engine_pool = PgPoolOptions::new()
                    .max_connections(2)
                    .connect(url)
                    .await
                    .wrap_err("Failed to connect to Engine database")?;
                Some(engine_pool)
            }
            None => {
                tracing::info!("ENGINE_DATABASE_URL not set, game backup disabled");
                None
            }
        };

        if config.gcs_bucket.is_some() {
            tracing::info!("GCS bucket configured for game backup");
        }

        // HTTP client for calling snake APIs (connection pooling, timeout slightly longer than game timeout)
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(600))
            .pool_max_idle_per_host(10)
            .build()
            .wrap_err("Failed to create HTTP client")?;
        tracing::info!("HTTP client initialized for snake API calls");

        // Optional: Mailgun transactional email (disabled until configured).
        // Uses its own client — the snake client's 600ms timeout is far too
        // tight for a public email API and would fail most real sends.
        let mailer = match &config.mailgun {
            Some(mailgun) => {
                let email_client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .wrap_err("Failed to create email HTTP client")?;
                tracing::info!(domain = %mailgun.domain, "Mailgun configured for email");
                Mailer::new(Some(mailgun.clone()), email_client)
            }
            None => {
                tracing::info!("MAILGUN_API_KEY not set, transactional email disabled");
                Mailer::disabled()
            }
        };

        let mut scoring_registry = crate::scoring::ScoringRegistry::new();
        scoring_registry.register(Box::new(crate::scoring::weng_lin::WengLinScoring));
        scoring_registry.register(Box::new(crate::scoring::win_rate::WinRateScoring));
        scoring_registry.register(Box::new(crate::scoring::food_eaten::FoodEatenScoring));

        Ok(Self {
            config: Arc::new(config),
            db: pool,
            cookie_key,
            engine_db,
            game_channels: GameChannels::new(),
            http_client,
            mailer,
            scoring: std::sync::Arc::new(scoring_registry),
        })
    }
}

#[cfg(test)]
impl AppState {
    /// Minimal AppState for DB-backed tests: a real pool, inert everything
    /// else (no OAuth, no engine DB, an empty scoring registry).
    pub fn test_from_pool(db: sqlx::PgPool) -> Self {
        Self {
            config: Arc::new(AppConfig::test_default()),
            db,
            cookie_key: cja::server::cookies::CookieKey::from_env_or_generate()
                .expect("failed to generate a test cookie key"),
            engine_db: None,
            game_channels: GameChannels::new(),
            http_client: reqwest::Client::new(),
            mailer: crate::email::Mailer::disabled(),
            scoring: std::sync::Arc::new(crate::scoring::ScoringRegistry::new()),
        }
    }
}

impl cja::app_state::AppState for AppState {
    fn version(&self) -> &str {
        env!("VERGEN_GIT_SHA")
    }

    fn db(&self) -> &sqlx::PgPool {
        &self.db
    }

    fn cookie_key(&self) -> &cja::server::cookies::CookieKey {
        &self.cookie_key
    }
}
