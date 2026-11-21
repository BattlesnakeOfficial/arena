use std::sync::Arc;

use cja::jobs::worker::IdlePollGate;
use color_eyre::eyre::{Context as _, eyre};
use sqlx::{PgPool, migrate::Migrator, postgres::PgPoolOptions};
use std::num::NonZeroUsize;

use crate::config::AppConfig;
use crate::discord::DiscordNotifier;
use crate::email::Mailer;
use crate::watched_games::WatchedGames;

#[derive(Clone)]
pub struct AppState {
    /// All resolved configuration, read once at boot. Deep code reads
    /// settings from here instead of reaching for `std::env`.
    pub config: Arc<AppConfig>,
    pub idle_poll_gate: IdlePollGate,
    pub db: sqlx::Pool<sqlx::Postgres>,
    pub cookie_key: cja::server::cookies::CookieKey,
    /// Connection to the legacy Battlesnake Engine database (for game backup)
    pub engine_db: Option<sqlx::Pool<sqlx::Postgres>>,
    /// Process-local viewers, shared by clones of this state.
    pub watched_games: WatchedGames,
    /// HTTP client for calling snake APIs
    pub http_client: reqwest::Client,
    pub proxy_east_client: reqwest::Client,
    pub proxy_europe_client: reqwest::Client,
    pub proxy_east_health_client: reqwest::Client,
    pub proxy_europe_health_client: reqwest::Client,
    /// Transactional email sender (no-op until Mailgun is configured)
    pub mailer: Mailer,
    /// Discord webhook notifier (no-op until `arena/discord/webhook-url` is in setec)
    pub discord: DiscordNotifier,
    /// Discord webhook notifier for the private weekly highlights digest
    /// (no-op until `arena/discord/highlights-webhook-url` is in setec;
    /// DEV-1993). Separate from `discord`, which posts to the community
    /// channel — highlights must never go there.
    pub discord_highlights: DiscordNotifier,
    /// Jev content-moderation judge (inert until TYPESAFE_API_KEY is
    /// configured; the offline hard-block list always runs)
    pub moderation: crate::moderation::ModerationJudge,
    /// Scoring algorithm registry
    pub scoring: std::sync::Arc<crate::scoring::ScoringRegistry>,
    /// TTL memo for the anonymous-homepage feed (see HOME_FEED_CACHE_SECS)
    pub home_feed_cache: Arc<crate::cache::TtlCell<crate::models::leaderboard::HomeFeed>>,
    pub stats_cache: Arc<crate::cache::TtlCell<crate::models::stats::StatsSnapshot>>,
    pub stats_refresh: Arc<tokio::sync::Mutex<()>>,
    pub activity_recorder: Arc<crate::activity::ActivityRecorder>,
    /// Head & Tail Studio guards (upload and processing slots, rate limit) and the
    /// upload processor (an `arena studio-worker` child process per upload).
    pub studio: crate::routes::studio::StudioState,
    /// The one object-storage seam: backups write through it, the archive
    /// reader reads through it (constructed from a clone of this same Arc).
    pub object_store: std::sync::Arc<dyn crate::object_store::ObjectStore>,
    /// Read side of the game archive (see `archive.rs`).
    pub archive: crate::archive::ArchiveReader,
}

const MIGRATION_LOCK_ID: i64 = 0xDB_DB_DB_DB_DB_DB_DB;

async fn run_migrations_with_lock(pool: &PgPool, migrator: &Migrator) -> cja::Result<()> {
    let mut conn = pool.acquire().await?;
    conn.close_on_drop();
    // Poll for the lock instead of blocking in pg_advisory_lock: a session
    // parked in that statement holds an open snapshot, so a concurrent
    // CREATE INDEX CONCURRENTLY run by the lock holder waits on the waiter's
    // virtualxid while the waiter waits on the advisory lock. Postgres then
    // detects the cycle, aborts the index build, and leaves an INVALID index
    // that a later `indisvalid` assertion turns into a permanent boot
    // failure. Each pg_try_advisory_lock attempt is a short statement, so no
    // booter ever holds an open transaction while waiting for the lock.
    loop {
        let acquired: bool = sqlx::query_scalar!(
            r#"SELECT pg_try_advisory_lock($1) AS "acquired!""#,
            MIGRATION_LOCK_ID
        )
        .fetch_one(&mut *conn)
        .await?;
        if acquired {
            break;
        }
        tracing::debug!(lock_id = MIGRATION_LOCK_ID, "migration lock held elsewhere");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let migration_result = migrator.run(&mut *conn).await;
    let unlock_result: cja::Result<()> = async {
        let unlocked: bool = sqlx::query_scalar!(
            r#"SELECT pg_advisory_unlock($1) AS "unlocked!""#,
            MIGRATION_LOCK_ID
        )
        .fetch_one(&mut *conn)
        .await?;
        if !unlocked {
            return Err(eyre!("Failed to unlock migration lock"));
        }
        Ok(())
    }
    .await;
    let close_result = conn.close().await;

    migration_result?;
    unlock_result?;
    close_result?;
    tracing::info!("Migration lock unlocked");
    Ok(())
}

impl AppState {
    /// Turn resolved [`AppConfig`] into live resources (pools, clients).
    /// All environment reading already happened in [`AppConfig::from_env`].
    pub async fn from_config(
        config: AppConfig,
        secrets: Option<&crate::secrets::AppSecrets>,
    ) -> cja::Result<Self> {
        async fn setup_db_pool(database_url: &str, max_connections: u32) -> cja::Result<PgPool> {
            let pool = PgPoolOptions::new()
                .max_connections(max_connections)
                .acquire_timeout(std::time::Duration::from_secs(60))
                .connect(database_url)
                .await?;
            run_migrations_with_lock(&pool, &sqlx::migrate!("../migrations")).await?;
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
                    .acquire_timeout(std::time::Duration::from_secs(60))
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
        let http_client = crate::snake_client::client_builder()
            .timeout(std::time::Duration::from_millis(600))
            .pool_max_idle_per_host(10)
            .build()
            .wrap_err("Failed to create HTTP client")?;
        tracing::info!("HTTP client initialized for snake API calls");
        let proxy_client = |timeout| -> cja::Result<reqwest::Client> {
            crate::snake_client::client_builder()
                .timeout(timeout)
                .pool_max_idle_per_host(80)
                .build()
                .wrap_err("Failed to create engine proxy client")
        };
        let proxy_east_client = proxy_client(std::time::Duration::from_secs(2))?;
        let proxy_europe_client = proxy_client(std::time::Duration::from_secs(2))?;
        let proxy_east_health_client = proxy_client(std::time::Duration::from_secs(6))?;
        let proxy_europe_health_client = proxy_client(std::time::Duration::from_secs(6))?;
        if config.engine_proxy.token.is_none() {
            tracing::warn!(
                "ENGINE_PROXY_AUTH_TOKEN is unset; regional snakes will be called directly"
            );
        }

        // Optional: Mailgun transactional email (disabled until configured).
        // Uses its own client — the snake client's 600ms timeout is far too
        // tight for a public email API and would fail most real sends.
        let mailer = match &config.mailgun {
            Some(mailgun) => {
                let email_client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    // A connect-phase deadline that surfaces as a *connect*
                    // error (so `SendError` can classify it definite
                    // non-delivery and shutdown reservations get released)
                    // instead of burning the whole total timeout and landing
                    // in the ambiguous no-response bucket.
                    .connect_timeout(std::time::Duration::from_secs(5))
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

        // Optional: Discord webhook notifications (disabled until configured).
        // One webhook client shared by both notifiers (the snake client's
        // 600ms timeout is too tight for an external webhook API).
        let discord_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .wrap_err("Failed to create Discord HTTP client")?;
        let discord = DiscordNotifier::new(
            secrets.and_then(|s| s.discord_webhook_url.clone()),
            discord_client.clone(),
        );
        if discord.is_enabled() {
            tracing::info!("Discord webhook configured for community notifications");
        } else {
            tracing::info!("No Discord webhook in setec, Discord notifications disabled");
        }

        // Second notifier for the private highlights webhook (DEV-1993);
        // absent means the weekly post is skipped and logged.
        let discord_highlights = DiscordNotifier::new(
            secrets.and_then(|s| s.discord_highlights_webhook_url.clone()),
            discord_client,
        );
        if discord_highlights.is_enabled() {
            tracing::info!("Discord highlights webhook configured for weekly digests");
        } else {
            tracing::info!(
                "No highlights webhook in setec, weekly highlights posts will be skipped"
            );
        }

        // Optional: Jev content moderation (disabled until configured).
        let moderation = crate::moderation::ModerationJudge::from_config(&config.moderation);
        if moderation.is_enabled() {
            tracing::info!("TYPESAFE_API_KEY set, Jev moderation enabled");
        } else {
            tracing::info!("TYPESAFE_API_KEY not set, Jev moderation disabled");
        }

        let scoring_registry = crate::scoring::default_registry();

        let home_feed_cache = Arc::new(crate::cache::TtlCell::new(std::time::Duration::from_secs(
            config.home_feed_cache_secs,
        )));
        let stats_cache = Arc::new(crate::cache::TtlCell::new(std::time::Duration::from_secs(
            config.stats_cache_secs,
        )));

        let studio = crate::routes::studio::StudioState::subprocess()
            .wrap_err("Failed to find this executable for the studio worker")?;

        let object_store: std::sync::Arc<dyn crate::object_store::ObjectStore> = match config
            .gcs_bucket
            .clone()
        {
            Some(bucket) => std::sync::Arc::new(crate::object_store::GcsObjectStore::new(bucket)),
            None => std::sync::Arc::new(crate::object_store::DisabledObjectStore),
        };
        let archive = crate::archive::ArchiveReader::new(
            object_store.clone(),
            config.archive_cache_max_bytes,
        );

        Ok(Self {
            idle_poll_gate: IdlePollGate::new(
                NonZeroUsize::new(config.job.idle_pollers).unwrap_or(NonZeroUsize::MIN),
            ),
            config: Arc::new(config),
            db: pool,
            cookie_key,
            engine_db,
            watched_games: WatchedGames::new(),
            http_client,
            proxy_east_client,
            proxy_europe_client,
            proxy_east_health_client,
            proxy_europe_health_client,
            mailer,
            discord,
            discord_highlights,
            moderation,
            scoring: std::sync::Arc::new(scoring_registry),
            home_feed_cache,
            stats_cache,
            stats_refresh: Arc::new(tokio::sync::Mutex::new(())),
            activity_recorder: Arc::new(crate::activity::ActivityRecorder::default()),
            studio,
            object_store,
            archive,
        })
    }
}

#[cfg(test)]
impl AppState {
    /// Minimal AppState for DB-backed tests: a real pool, inert everything
    /// else (no OAuth, no engine DB, an empty scoring registry).
    pub fn test_from_pool(db: sqlx::PgPool) -> Self {
        let config = AppConfig::test_default();
        let object_store: std::sync::Arc<dyn crate::object_store::ObjectStore> =
            std::sync::Arc::new(crate::object_store::InMemoryObjectStore::default());
        let archive = crate::archive::ArchiveReader::new(
            object_store.clone(),
            crate::archive::DEFAULT_ARCHIVE_CACHE_MAX_BYTES,
        );
        Self {
            idle_poll_gate: IdlePollGate::new(NonZeroUsize::MIN),
            config: Arc::new(config),
            db,
            cookie_key: cja::server::cookies::CookieKey::from_env_or_generate()
                .expect("failed to generate a test cookie key"),
            engine_db: None,
            watched_games: WatchedGames::new(),
            http_client: reqwest::Client::new(),
            proxy_east_client: reqwest::Client::new(),
            proxy_europe_client: reqwest::Client::new(),
            proxy_east_health_client: reqwest::Client::new(),
            proxy_europe_health_client: reqwest::Client::new(),
            mailer: crate::email::Mailer::disabled(),
            discord: crate::discord::DiscordNotifier::disabled(),
            discord_highlights: crate::discord::DiscordNotifier::disabled(),
            moderation: crate::moderation::ModerationJudge::disabled(),
            scoring: std::sync::Arc::new(crate::scoring::ScoringRegistry::new()),
            home_feed_cache: Arc::new(crate::cache::TtlCell::new(std::time::Duration::ZERO)),
            stats_cache: Arc::new(crate::cache::TtlCell::new(std::time::Duration::ZERO)),
            stats_refresh: Arc::new(tokio::sync::Mutex::new(())),
            activity_recorder: Arc::new(crate::activity::ActivityRecorder::default()),
            // The test binary has no `studio-worker` subcommand: process in-process.
            studio: crate::routes::studio::StudioState::in_process(),
            object_store,
            archive,
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

    fn job_enqueued(&self) {
        self.idle_poll_gate.wake_one();
    }

    fn cookie_key(&self) -> &cja::server::cookies::CookieKey {
        &self.cookie_key
    }

    fn eyes_base_url(&self) -> Option<&str> {
        Some(crate::config::ARENA_PUBLIC_BASE_URL)
    }

    fn eyes_http_monitors(&self) -> Vec<cja::app_state::HttpMonitor> {
        crate::observability::http_monitors(self.config.features.server)
    }
}

#[cfg(test)]
mod migration_lock_tests {
    use super::{MIGRATION_LOCK_ID, run_migrations_with_lock};
    use sqlx::{PgPool, migrate::Migrator};
    use std::time::Duration;

    async fn exercise(pool: PgPool, sql: &str, should_fail: bool) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("20261121130000_probe.sql"), sql).unwrap();
        let migrator = Migrator::new(directory.path()).await.unwrap();
        let migration_pool = pool.clone();
        let directory_guard = directory;
        let migration = async move {
            let _directory_guard = directory_guard;
            run_migrations_with_lock(&migration_pool, &migrator).await
        };

        let observer = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pid: Option<i32> = sqlx::query_scalar(
                    "SELECT a.pid FROM pg_stat_activity a JOIN pg_locks l ON l.pid = a.pid
                     WHERE a.pid <> pg_backend_pid() AND a.datname = current_database() AND a.state = 'active'
                       AND a.query LIKE '%pg_sleep%'
                       AND l.locktype = 'advisory' AND l.granted
                       AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
                       AND l.classid::text::bigint = ($1::bigint >> 32)
                       AND l.objid::text::bigint = ($1::bigint & 4294967295)
                     LIMIT 1",
                )
                .bind(MIGRATION_LOCK_ID)
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some(pid) = pid {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let (result, holder) = tokio::join!(migration, observer);
        let holder = holder.expect("migrator must hold the application lock on its own backend");
        if should_fail {
            let message = result.unwrap_err().to_string();
            assert!(message.contains("sentinel migration failure"), "{message}");
        } else {
            result.unwrap();
        }
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = $1")
                .bind(holder)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0, "migration backend must be closed");
        // pg_locks is cluster-wide while each #[sqlx::test] runs in its own
        // database: scope to this database so a sibling test's advisory lock
        // (the success/failure probes and the CIC test all hold the same key
        // concurrently) is not counted here.
        let locks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
               AND classid::text::bigint = ($1::bigint >> 32)
               AND objid::text::bigint = ($1::bigint & 4294967295)",
        )
        .bind(MIGRATION_LOCK_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(locks, 0);
    }

    #[sqlx::test(migrations = false)]
    async fn application_migrator_runs_on_disposable_database(pool: PgPool) {
        run_migrations_with_lock(&pool, &sqlx::migrate!("../migrations"))
            .await
            .unwrap();
        let seasons: i64 = sqlx::query_scalar("SELECT count(*) FROM leaderboard_seasons")
            .fetch_one(&pool)
            .await
            .unwrap();
        let leagues: i64 = sqlx::query_scalar("SELECT count(*) FROM leaderboards")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(seasons, leagues);
    }

    #[sqlx::test(migrations = false)]
    async fn migration_lock_uses_migrator_backend_on_success(pool: PgPool) {
        exercise(pool, "SELECT pg_sleep(2);", false).await;
    }

    #[sqlx::test(migrations = false)]
    async fn migration_lock_uses_migrator_backend_on_failure(pool: PgPool) {
        exercise(
            pool,
            "DO $$ BEGIN PERFORM pg_sleep(2); RAISE EXCEPTION 'sentinel migration failure'; END $$;",
            true,
        )
        .await;
    }

    // Regression: a blocking `pg_advisory_lock` wait keeps the statement (and
    // its snapshot) open, so when the lock holder reaches a CREATE INDEX
    // CONCURRENTLY migration the two sessions wait on each other, Postgres
    // reports a deadlock, the index build aborts, and the leftover INVALID
    // index would fail every later boot. A second booter using the polling
    // helper must coexist with a concurrent index build instead.
    #[sqlx::test(migrations = false)]
    async fn concurrent_lock_waiter_survives_concurrent_index_build(pool: PgPool) {
        let index_directory = tempfile::tempdir().unwrap();
        std::fs::write(
            index_directory.path().join("20261121130100_probe.sql"),
            "CREATE TABLE deadlock_probe (id integer);\nSELECT pg_sleep(2);",
        )
        .unwrap();
        std::fs::write(
            index_directory.path().join("20261121130200_probe_cic.sql"),
            "-- no-transaction\nCREATE INDEX CONCURRENTLY IF NOT EXISTS deadlock_probe_idx ON deadlock_probe (id);",
        )
        .unwrap();
        // Both booters resolve the same migrations: a migrator whose set is
        // missing a version another booter already applied is rejected by
        // sqlx (VersionMissing), so the waiter replays this set as a no-op
        // after the index runner finishes. What is under test is the wait
        // protocol around the lock, not the waiter's own migrations.
        let migrator = Migrator::new(index_directory.path()).await.unwrap();

        // The index runner acquires the lock first and spends two seconds in
        // its first migration, so the waiter is guaranteed to be waiting (in
        // whatever protocol it uses) by the time the concurrent index build
        // starts. With the old blocking wait that overlap deadlocked.
        let index_pool = pool.clone();
        let (index_run, waiter_run) = tokio::join!(
            async { run_migrations_with_lock(&index_pool, &migrator).await },
            async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                run_migrations_with_lock(&pool, &migrator).await
            }
        );
        index_run.unwrap();
        waiter_run.unwrap();

        let valid: bool = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index WHERE indexrelid = 'deadlock_probe_idx'::regclass",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            valid,
            "concurrent index must be valid after a contended boot"
        );
    }
}
