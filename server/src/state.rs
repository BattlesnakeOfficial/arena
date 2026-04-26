use color_eyre::eyre::{Context as _, eyre};
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::game_channels::GameChannels;
use crate::github::auth::GitHubOAuthConfig;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::Pool<sqlx::Postgres>,
    pub cookie_key: cja::server::cookies::CookieKey,
    pub github_oauth_config: Option<GitHubOAuthConfig>,
    /// Connection to the legacy Battlesnake Engine database (for game backup)
    pub engine_db: Option<sqlx::Pool<sqlx::Postgres>>,
    /// GCS bucket name for game backups
    pub gcs_bucket: Option<String>,
    /// Broadcast channels for live game updates
    pub game_channels: GameChannels,
    /// HTTP client for calling snake APIs
    pub http_client: reqwest::Client,
    /// Scoring algorithm registry
    pub scoring: std::sync::Arc<crate::scoring::ScoringRegistry>,
}

impl AppState {
    pub async fn from_env() -> cja::Result<Self> {
        #[tracing::instrument(err)]
        pub async fn setup_db_pool() -> cja::Result<PgPool> {
            const MIGRATION_LOCK_ID: i64 = 0xDB_DB_DB_DB_DB_DB_DB;

            let database_url =
                std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;
            let max_connections: u32 = std::env::var("ARENA_PG_MAX_CONNECTIONS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let pool = PgPoolOptions::new()
                .max_connections(max_connections)
                .connect(&database_url)
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

        let pool = setup_db_pool().await?;

        let cookie_key = cja::server::cookies::CookieKey::from_env_or_generate()?;

        // Initialize GitHub OAuth config (optional - auth disabled if not configured)
        let github_oauth_config = match GitHubOAuthConfig::from_env() {
            Ok(config) => {
                tracing::info!("GitHub OAuth configured");
                Some(config)
            }
            Err(e) => {
                tracing::warn!("GitHub OAuth not configured, auth will be disabled: {}", e);
                None
            }
        };

        // Optional: Engine database for game backup
        let engine_db = match std::env::var("ENGINE_DATABASE_URL") {
            Ok(url) => {
                tracing::info!("Connecting to Engine database for game backup");
                let engine_pool = PgPoolOptions::new()
                    .max_connections(2)
                    .connect(&url)
                    .await
                    .wrap_err("Failed to connect to Engine database")?;
                Some(engine_pool)
            }
            Err(_) => {
                tracing::info!("ENGINE_DATABASE_URL not set, game backup disabled");
                None
            }
        };

        // Optional: GCS bucket for game backup
        let gcs_bucket = std::env::var("GCS_BUCKET").ok();
        if gcs_bucket.is_some() {
            tracing::info!("GCS bucket configured for game backup");
        }

        // HTTP client for calling snake APIs (connection pooling, timeout slightly longer than game timeout)
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(600))
            .pool_max_idle_per_host(10)
            .build()
            .wrap_err("Failed to create HTTP client")?;
        tracing::info!("HTTP client initialized for snake API calls");

        let mut scoring_registry = crate::scoring::ScoringRegistry::new();
        scoring_registry.register(Box::new(crate::scoring::weng_lin::WengLinScoring));
        scoring_registry.register(Box::new(crate::scoring::win_rate::WinRateScoring));
        scoring_registry.register(Box::new(crate::scoring::food_eaten::FoodEatenScoring));

        Ok(Self {
            db: pool,
            cookie_key,
            github_oauth_config,
            engine_db,
            gcs_bucket,
            game_channels: GameChannels::new(),
            http_client,
            scoring: std::sync::Arc::new(scoring_registry),
        })
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
