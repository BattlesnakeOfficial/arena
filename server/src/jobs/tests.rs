use std::{path::Path, process::Command, time::Duration};

use cja::jobs::worker::{DEFAULT_MAX_RETRIES, JobLeaseConfig, job_worker_configured};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use super::{AppState, Jobs};
use crate::config::{AppConfig, DEFAULT_JOB_SHUTDOWN_DRAIN_SECS};

fn test_lease() -> JobLeaseConfig {
    let job = AppConfig::test_default().job;
    JobLeaseConfig {
        heartbeat_interval: Duration::from_secs(job.heartbeat_interval_secs),
        reclaim_window: Duration::from_secs(job.lock_timeout_secs),
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn exhausted_job_is_archived_and_worker_continues(pool: PgPool) -> cja::Result<()> {
    let failed_id = uuid::Uuid::new_v4();
    let healthy_id = uuid::Uuid::new_v4();
    // An unknown job deterministically fails without making external calls.
    // Higher priority guarantees it runs before the healthy control job.
    let created_at = sqlx::query_scalar!(
        "INSERT INTO jobs (job_id, name, payload, context, priority, error_count)
         VALUES ($1, 'MigrationTestFailure', '{\"reference\":713}', 'migration-test', 10, $2)
         RETURNING created_at",
        failed_id,
        DEFAULT_MAX_RETRIES,
    )
    .fetch_one(&pool)
    .await?;
    sqlx::query!(
        "INSERT INTO jobs (job_id, name, payload, context, priority)
         VALUES ($1, 'NoopJob', 'null', 'migration-test', 0)",
        healthy_id,
    )
    .execute(&pool)
    .await?;

    let shutdown = CancellationToken::new();
    let worker = job_worker_configured(
        AppState::test_from_pool(pool.clone()),
        Jobs,
        Duration::from_millis(10),
        DEFAULT_MAX_RETRIES,
        shutdown.clone(),
        test_lease(),
        Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
    );
    let observe = async {
        // Cancel even on a query failure or timeout, so the worker always exits.
        let _cancel = shutdown.drop_guard();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let remaining = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs")
                    .fetch_one(&pool)
                    .await?;
                if remaining == Some(0) {
                    return Ok::<_, cja::color_eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?
    };
    let (worker_result, observed) = tokio::join!(worker, observe);
    worker_result?;
    observed?;

    let archived = sqlx::query!(
        "SELECT original_job_id, name, payload, context, priority, error_count,
                last_error_message, created_at, failed_at FROM dead_letter_jobs"
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(archived.len(), 1, "only the exhausted job is archived");
    let archived = &archived[0];
    assert_eq!(archived.original_job_id, failed_id);
    assert_eq!(archived.name, "MigrationTestFailure");
    assert_eq!(archived.payload, serde_json::json!({"reference": 713}));
    assert_eq!(archived.context, "migration-test");
    assert_eq!(archived.priority, 10);
    assert_eq!(archived.error_count, DEFAULT_MAX_RETRIES + 1);
    assert_eq!(
        archived.last_error_message.as_deref(),
        Some("Unknown job type: MigrationTestFailure")
    );
    assert_eq!(archived.created_at, created_at);
    assert!(archived.failed_at >= created_at);
    Ok(())
}

mod lease_probes {
    use super::AppState;
    use cja::jobs::Job;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct PanicProbeJob;

    #[async_trait::async_trait]
    impl Job<AppState> for PanicProbeJob {
        const NAME: &'static str = "PanicProbeJob";

        async fn run(&self, _state: AppState) -> cja::Result<()> {
            panic!("poison job");
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    pub struct HealthyProbeJob;

    #[async_trait::async_trait]
    impl Job<AppState> for HealthyProbeJob {
        const NAME: &'static str = "HealthyProbeJob";

        async fn run(&self, _state: AppState) -> cja::Result<()> {
            Ok(())
        }
    }

    cja::impl_job_registry!(AppState, PanicProbeJob, HealthyProbeJob);
}

struct ProbeWorker {
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<cja::Result<()>>,
}

impl ProbeWorker {
    fn start(pool: PgPool, max_retries: i32) -> Self {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(job_worker_configured(
            AppState::test_from_pool(pool),
            lease_probes::Jobs,
            Duration::from_millis(50),
            max_retries,
            shutdown.clone(),
            test_lease(),
            Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
        ));
        Self { shutdown, handle }
    }

    async fn stop(&mut self) {
        self.shutdown.cancel();
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS + 1),
                &mut self.handle,
            )
            .await,
            Ok(Ok(Ok(())))
        ));
    }
}

impl Drop for ProbeWorker {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.handle.abort();
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn panicking_job_is_charged_and_arena_worker_continues(pool: PgPool) -> cja::Result<()> {
    let panic_id = uuid::Uuid::new_v4();
    let healthy_id = uuid::Uuid::new_v4();
    for (id, name, priority) in [
        (panic_id, "PanicProbeJob", 10),
        (healthy_id, "HealthyProbeJob", 0),
    ] {
        sqlx::query(
            "INSERT INTO jobs (job_id, name, payload, context, priority)
             VALUES ($1, $2, 'null', 'lease-probe', $3)",
        )
        .bind(id)
        .bind(name)
        .bind(priority)
        .execute(&pool)
        .await?;
    }
    let mut worker = ProbeWorker::start(pool.clone(), DEFAULT_MAX_RETRIES);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row: (
                i32,
                Option<String>,
                Option<chrono::DateTime<chrono::Utc>>,
                Option<String>,
            ) = sqlx::query_as(
                "SELECT error_count, locked_by, locked_at, last_error_message
                     FROM jobs WHERE job_id = $1",
            )
            .bind(panic_id)
            .fetch_one(&pool)
            .await?;
            let healthy_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE job_id = $1")
                    .bind(healthy_id)
                    .fetch_one(&pool)
                    .await?;
            if row.0 == 1 && row.1.is_none() && row.2.is_none() && healthy_count == 0 {
                assert!(row.3.as_deref().is_some_and(|m| m.contains("poison job")));
                break Ok::<(), cja::color_eyre::Report>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert!(!worker.handle.is_finished(), "panic ended Arena's worker");
    worker.stop().await;
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn arena_job_lease_uses_240_second_reclaim_window(pool: PgPool) -> cja::Result<()> {
    let stale_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (job_id, name, payload, context, priority, locked_by, locked_at)
         VALUES ($1, 'HealthyProbeJob', '{\"probe\":true}', 'lease-probe', 0,
                 'lease:dead-worker', NOW() - interval '5 minutes')",
    )
    .bind(stale_id)
    .execute(&pool)
    .await?;
    let mut worker = ProbeWorker::start(pool.clone(), 0);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row: Option<(i32, String, serde_json::Value)> = sqlx::query_as(
                "SELECT error_count, last_error_message, payload
                 FROM dead_letter_jobs WHERE original_job_id = $1",
            )
            .bind(stale_id)
            .fetch_optional(&pool)
            .await?;
            if let Some((count, message, payload)) = row {
                assert_eq!(count, 1);
                assert!(message.contains("lease:dead-worker"));
                assert_eq!(payload, serde_json::json!({"probe": true}));
                break Ok::<(), cja::color_eyre::Report>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let active_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE job_id = $1")
        .bind(stale_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(active_count, 0);
    worker.stop().await;

    let live_id = uuid::Uuid::new_v4();
    let locked_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "INSERT INTO jobs (job_id, name, payload, context, priority, locked_by, locked_at)
         VALUES ($1, 'HealthyProbeJob', 'null', 'lease-probe', 0,
                 'lease:live-elsewhere', NOW() - interval '180 seconds')
         RETURNING locked_at",
    )
    .bind(live_id)
    .fetch_one(&pool)
    .await?;
    let mut worker = ProbeWorker::start(pool.clone(), 0);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let row: (String, chrono::DateTime<chrono::Utc>, i32) =
        sqlx::query_as("SELECT locked_by, locked_at, error_count FROM jobs WHERE job_id = $1")
            .bind(live_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(row, ("lease:live-elsewhere".to_string(), locked_at, 0));
    let archived_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM dead_letter_jobs WHERE original_job_id = $1")
            .bind(live_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(archived_count, 0);
    worker.stop().await;
    Ok(())
}

#[test]
fn cja_migrations_are_present_and_unchanged() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version=1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata to locate the pinned cja dependency");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let manifests: Vec<_> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package["name"] == "cja")
        .map(|package| package["manifest_path"].as_str().unwrap())
        .collect();
    assert_eq!(manifests.len(), 1, "expected exactly one cja dependency");
    let upstream = Path::new(manifests[0]).parent().unwrap().join("migrations");
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../migrations");
    let mut checked = 0;
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(upstream).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if path.extension().is_none_or(|extension| extension != "sql") {
            continue;
        }
        // Arena owns its session schema and does not use cja's AppSession.
        if matches!(
            name,
            "20250413182934_AddSessions.up.sql" | "20250413182934_AddSessions.down.sql"
        ) {
            continue;
        }
        checked += 1;
        let mut copy = local.join(name);
        if !copy.exists() && !name.ends_with(".up.sql") && !name.ends_with(".down.sql") {
            copy = local.join(format!("{}.up.sql", name.strip_suffix(".sql").unwrap()));
        }
        match std::fs::read(&copy) {
            Ok(bytes) if bytes == std::fs::read(&path).unwrap() => {}
            Ok(_) => problems.push(format!("differs from cja: {name}")),
            Err(error) => problems.push(format!("missing/unreadable {name}: {error}")),
        }
    }
    assert!(
        checked > 0,
        "no cja migrations found; check dependency layout"
    );
    assert!(
        problems.is_empty(),
        "Copy cja migrations unmodified into migrations/ (a .up.sql suffix is supported). \
         Never edit an applied migration.\n{}",
        problems.join("\n")
    );
}
