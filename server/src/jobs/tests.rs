use std::{path::Path, process::Command, time::Duration};

use cja::jobs::{
    Job,
    worker::{DEFAULT_MAX_RETRIES, job_worker},
};
use sqlx::PgPool;
use std::{num::NonZeroUsize, sync::Arc};
use tokio_util::sync::CancellationToken;

use super::{AppState, Jobs, SendNotificationEmailJob};
use crate::config::{AppConfig, DEFAULT_JOB_SHUTDOWN_DRAIN_SECS};

fn assert_html_links_in_text(form: &std::collections::HashMap<String, String>) {
    let html = form.get("html").expect("HTML part");
    let text = form.get("text").expect("text part");
    for href in html
        .split("href=\"")
        .skip(1)
        .map(|part| part.split('"').next().unwrap())
    {
        let href = href
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        assert!(text.contains(&href), "HTML link missing from text: {href}");
    }
}

#[test]
fn html_link_parity_decodes_ampersands() {
    let form = std::collections::HashMap::from([
        (
            "html".to_string(),
            "<a href=\"https://arena.example/claim?a=1&amp;b=2\">Claim</a>".to_string(),
        ),
        (
            "text".to_string(),
            "https://arena.example/claim?a=1&b=2".to_string(),
        ),
    ]);
    assert_html_links_in_text(&form);
}

mod idle_gate_probes {
    use super::AppState;
    use cja::jobs::Job;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct StartedJob {
        pub id: uuid::Uuid,
    }

    #[async_trait::async_trait]
    impl Job<AppState> for StartedJob {
        const NAME: &'static str = "StartedJob";

        async fn run(&self, state: AppState) -> cja::Result<()> {
            sqlx::query(
                "INSERT INTO job_start_markers (id, started_at) VALUES ($1, clock_timestamp())",
            )
            .bind(self.id)
            .execute(&state.db)
            .await?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            sqlx::query(
                "UPDATE job_start_markers SET finished_at = clock_timestamp() WHERE id = $1",
            )
            .bind(self.id)
            .execute(&state.db)
            .await?;
            Ok(())
        }
    }

    use std::time::Duration;
    cja::impl_job_registry!(AppState, StartedJob);
}

async fn run_idle_gate_burst(pool: PgPool, via_enqueue: bool) -> cja::Result<()> {
    use idle_gate_probes::StartedJob;

    sqlx::query("CREATE TABLE job_start_markers (id uuid PRIMARY KEY, started_at timestamptz NOT NULL, finished_at timestamptz)")
        .execute(&pool)
        .await?;
    let worker_pool = sqlx::postgres::PgPoolOptions::new()
        // CI's PostgreSQL service allows 100 clients; two burst tests may run
        // together, while each job releases its connection before sleeping.
        .max_connections(30)
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;
    let mut state = AppState::test_from_pool(worker_pool);
    let job = &mut Arc::make_mut(&mut state.config).job;
    job.workers = 100;
    job.idle_pollers = 2;
    job.poll_interval_ms = 200;
    state.idle_poll_gate =
        cja::jobs::worker::IdlePollGate::new(NonZeroUsize::new(2).expect("nonzero"));

    let shutdown = CancellationToken::new();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..state.config.job.workers {
        workers.spawn(crate::job_worker_future(
            state.clone(),
            idle_gate_probes::Jobs,
            shutdown.clone(),
            Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
        ));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let result: cja::Result<()> = async {
        for _ in 0..32 {
            let item = StartedJob { id: uuid::Uuid::new_v4() };
            if via_enqueue {
                item.enqueue(state.clone(), "idle-gate-burst".to_owned(), None).await?;
            } else {
                sqlx::query("INSERT INTO jobs (job_id, name, payload, context, priority) VALUES ($1, $2, $3, 'idle-gate-burst', 0)")
                    .bind(uuid::Uuid::new_v4())
                    .bind(StartedJob::NAME)
                    .bind(serde_json::to_value(item)?)
                    .execute(&pool)
                    .await?;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let started: i64 = sqlx::query_scalar("SELECT count(*) FROM job_start_markers")
                    .fetch_one(&pool)
                    .await?;
                if started == 32 { break Ok::<(), cja::color_eyre::Report>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let latest_start: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT max(started_at) FROM job_start_markers")
            .fetch_one(&pool).await?;
        let earliest_start: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT min(started_at) FROM job_start_markers")
            .fetch_one(&pool).await?;
        cja::color_eyre::eyre::ensure!(
            latest_start - earliest_start < chrono::Duration::seconds(2),
            "burst took too long to enter job bodies"
        );
        Ok(())
    }.await;

    shutdown.cancel();
    let drained = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(worker) = workers.join_next().await {
            worker??;
        }
        Ok::<(), cja::color_eyre::Report>(())
    })
    .await;
    if drained.is_err() {
        workers.abort_all();
    }
    drained??;
    let locked: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE locked_by IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    cja::color_eyre::eyre::ensure!(locked == 0, "worker shutdown left {locked} locks");
    let active_overlap: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job_start_markers WHERE started_at < (SELECT min(finished_at) FROM job_start_markers)"
    ).fetch_one(&pool).await?;
    cja::color_eyre::eyre::ensure!(
        active_overlap >= 32,
        "only {active_overlap} jobs were concurrently active"
    );
    result
}

#[sqlx::test(migrations = "../migrations")]
async fn gated_workers_pick_up_local_enqueue_burst(pool: PgPool) -> cja::Result<()> {
    run_idle_gate_burst(pool, true).await
}

#[sqlx::test(migrations = "../migrations")]
async fn gated_workers_pick_up_sql_insert_burst(pool: PgPool) -> cja::Result<()> {
    run_idle_gate_burst(pool, false).await
}

#[sqlx::test(migrations = "../migrations")]
async fn free_worker_takes_game_before_backup_backlog(pool: PgPool) -> cja::Result<()> {
    use idle_gate_probes::StartedJob;

    sqlx::query("CREATE TABLE job_start_markers (id uuid PRIMARY KEY, started_at timestamptz NOT NULL, finished_at timestamptz)")
        .execute(&pool).await?;
    let mut state = AppState::test_from_pool(pool.clone());
    Arc::make_mut(&mut state.config).job.poll_interval_ms = 200;
    let shutdown = CancellationToken::new();
    let mut worker = tokio::spawn(crate::job_worker_future(
        state.clone(),
        idle_gate_probes::Jobs,
        shutdown.clone(),
        Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
    ));
    let result: cja::Result<()> = async {
        let running = StartedJob {
            id: uuid::Uuid::new_v4(),
        };
        running
            .clone()
            .enqueue(state.clone(), "priority-test".into(), Some(0))
            .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_start_markers")
                    .fetch_one(&pool)
                    .await?;
                if count == 1 {
                    break Ok::<(), cja::color_eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let backup = StartedJob {
            id: uuid::Uuid::new_v4(),
        };
        let game = StartedJob {
            id: uuid::Uuid::new_v4(),
        };
        backup
            .clone()
            .enqueue(state.clone(), "priority-test".into(), Some(-10))
            .await?;
        game.clone()
            .enqueue(state.clone(), "priority-test".into(), Some(0))
            .await?;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM job_start_markers")
                    .fetch_one(&pool)
                    .await?;
                if count == 3 {
                    break Ok::<(), cja::color_eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let first: uuid::Uuid = sqlx::query_scalar(
            "SELECT id FROM job_start_markers WHERE id IN ($1, $2) ORDER BY started_at LIMIT 1",
        )
        .bind(game.id)
        .bind(backup.id)
        .fetch_one(&pool)
        .await?;
        cja::color_eyre::eyre::ensure!(first == game.id, "backup claimed before game");
        Ok(())
    }
    .await;
    shutdown.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(8), &mut worker).await;
    if joined.is_err() {
        worker.abort();
    }
    joined???;
    let locked: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE locked_by IS NOT NULL")
        .fetch_one(&pool)
        .await?;
    cja::color_eyre::eyre::ensure!(locked == 0, "shutdown left {locked} locks");
    result
}

/// Run manually against an isolated PostgreSQL cluster with at least 200
/// available connections. This measures the same table counters used in prod.
#[sqlx::test(migrations = "../migrations")]
#[ignore = "60-second before/after idle poll measurement"]
async fn idle_claim_scan_rate_before_and_after_gate(pool: PgPool) -> cja::Result<()> {
    for gated in [false, true] {
        let worker_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(200)
            .connect_with(pool.connect_options().as_ref().clone())
            .await?;
        let mut state = AppState::test_from_pool(worker_pool);
        let job = &mut Arc::make_mut(&mut state.config).job;
        job.workers = 100;
        job.idle_pollers = 2;
        job.poll_interval_ms = 200;
        state.idle_poll_gate =
            cja::jobs::worker::IdlePollGate::new(NonZeroUsize::new(2).expect("nonzero"));
        let shutdown = CancellationToken::new();
        let mut workers = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let worker_config = state.config.job.worker_config(
                Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
                state.idle_poll_gate.clone(),
            );
            let worker_config = if gated {
                worker_config
            } else {
                cja::jobs::worker::JobWorkerConfig {
                    idle_poll_gate: None,
                    ..worker_config
                }
            };
            workers.spawn(job_worker(
                state.clone(),
                idle_gate_probes::Jobs,
                Duration::from_millis(200),
                DEFAULT_MAX_RETRIES,
                shutdown.clone(),
                worker_config,
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut stats = pool.acquire().await?;
        sqlx::query("SET default_transaction_read_only = on")
            .execute(&mut *stats)
            .await?;
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *stats)
            .await?;
        let before: i64 = sqlx::query_scalar(
            "SELECT seq_scan + idx_scan FROM pg_stat_user_tables WHERE schemaname = 'public' AND relname = 'jobs'"
        ).fetch_one(&mut *stats).await?;
        let started_at = chrono::Utc::now();
        let instant = std::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(60)).await;
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *stats)
            .await?;
        let after: i64 = sqlx::query_scalar(
            "SELECT seq_scan + idx_scan FROM pg_stat_user_tables WHERE schemaname = 'public' AND relname = 'jobs'"
        ).fetch_one(&mut *stats).await?;
        let elapsed = instant.elapsed().as_secs_f64();
        let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE locked_by IS NULL")
            .fetch_one(&pool)
            .await?;
        let running: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE locked_by IS NOT NULL")
                .fetch_one(&pool)
                .await?;
        println!(
            "gated={gated} started_at={started_at} ended_at={} workers=100 idle_pollers=2 interval_ms=200 before={before} after={after} delta={} elapsed_secs={elapsed:.3} scans_per_sec={:.2} queued={queued} running={running}",
            chrono::Utc::now(),
            after - before,
            (after - before) as f64 / elapsed
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(worker) = workers.join_next().await {
                worker??;
            }
            Ok::<(), cja::color_eyre::Report>(())
        })
        .await??;
    }
    Ok(())
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
    let state = AppState::test_from_pool(pool.clone());
    let worker = job_worker(
        state.clone(),
        Jobs,
        Duration::from_millis(10),
        DEFAULT_MAX_RETRIES,
        shutdown.clone(),
        AppConfig::test_default().job.worker_config(
            Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
            state.idle_poll_gate.clone(),
        ),
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
        let handle = tokio::spawn(job_worker(
            AppState::test_from_pool(pool),
            lease_probes::Jobs,
            Duration::from_millis(50),
            max_retries,
            shutdown.clone(),
            AppConfig::test_default().job.worker_config(
                Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS),
                cja::jobs::worker::IdlePollGate::new(std::num::NonZeroUsize::MIN),
            ),
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
        sqlx::query!(
            "INSERT INTO jobs (job_id, name, payload, context, priority)
             VALUES ($1, $2, 'null', 'lease-probe', $3)",
            id,
            name,
            priority,
        )
        .execute(&pool)
        .await?;
    }
    let mut worker = ProbeWorker::start(pool.clone(), DEFAULT_MAX_RETRIES);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = sqlx::query!(
                "SELECT error_count, locked_by, locked_at, last_error_message
                     FROM jobs WHERE job_id = $1",
                panic_id,
            )
            .fetch_one(&pool)
            .await?;
            let healthy_count =
                sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE job_id = $1", healthy_id)
                    .fetch_one(&pool)
                    .await?;
            if row.error_count >= 1
                && row.locked_by.is_none()
                && row.locked_at.is_none()
                && row
                    .last_error_message
                    .as_deref()
                    .is_some_and(|m| m.contains("poison job"))
                && healthy_count == Some(0)
            {
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
    sqlx::query!(
        "INSERT INTO jobs (job_id, name, payload, context, priority, locked_by, locked_at)
         VALUES ($1, 'HealthyProbeJob', '{\"probe\":true}', 'lease-probe', 0,
                 'lease:dead-worker', NOW() - interval '5 minutes')",
        stale_id,
    )
    .execute(&pool)
    .await?;
    let mut worker = ProbeWorker::start(pool.clone(), 0);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = sqlx::query!(
                "SELECT error_count, last_error_message, payload
                 FROM dead_letter_jobs WHERE original_job_id = $1",
                stale_id,
            )
            .fetch_optional(&pool)
            .await?;
            if let Some(row) = row {
                assert_eq!(row.error_count, 1);
                assert!(
                    row.last_error_message
                        .as_deref()
                        .is_some_and(|m| m.contains("lease:dead-worker"))
                );
                assert_eq!(row.payload, serde_json::json!({"probe": true}));
                break Ok::<(), cja::color_eyre::Report>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let active_count = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE job_id = $1", stale_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(active_count, Some(0));
    worker.stop().await;

    let live_id = uuid::Uuid::new_v4();
    let locked_at = sqlx::query_scalar!(
        "INSERT INTO jobs (job_id, name, payload, context, priority, locked_by, locked_at)
         VALUES ($1, 'HealthyProbeJob', 'null', 'lease-probe', 0,
                 'lease:live-elsewhere', NOW() - interval '180 seconds')
         RETURNING locked_at",
        live_id,
    )
    .fetch_one(&pool)
    .await?;
    let mut worker = ProbeWorker::start(pool.clone(), 0);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let row = sqlx::query!(
        "SELECT locked_by, locked_at, error_count FROM jobs WHERE job_id = $1",
        live_id
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.locked_by.as_deref(), Some("lease:live-elsewhere"));
    assert_eq!(row.locked_at, locked_at);
    assert_eq!(row.error_count, 0);
    let archived_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM dead_letter_jobs WHERE original_job_id = $1",
        live_id
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(archived_count, Some(0));
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
async fn wait_for_notification_row_waiters(pool: &PgPool, expected: i64) -> cja::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%notification_emails%'",
            )
            .fetch_one(pool)
            .await?;
            if blocked >= expected {
                break Ok::<_, color_eyre::Report>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn notification_email_is_sent_once_even_for_concurrent_runs(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1673001, 'notification-job-test', '', 'owner@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingPaused,
            title: "Paused".into(),
            body: "Health checks failed".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Paused".into()),
                text: Some("Message".into()),
                html_body: Some("<p>body</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let job = SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    };
    // Separate Mailers model separate server processes: their semaphores do
    // not coordinate, so the outbox row lock must serialize delivery.
    let mut other_state = AppState::test_from_pool(pool.clone());
    other_state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let mut holder = pool.begin().await?;
    sqlx::query(
        "SELECT notification_id FROM notification_emails WHERE notification_id = $1 FOR UPDATE",
    )
    .bind(receipt.notification_id)
    .fetch_one(&mut *holder)
    .await?;
    let first_job = job.clone();
    let first_state = state.clone();
    let first = tokio::spawn(async move { first_job.run(first_state).await });
    wait_for_notification_row_waiters(&pool, 1).await?;
    let second_job = job.clone();
    let second = tokio::spawn(async move { second_job.run(other_state).await });
    wait_for_notification_row_waiters(&pool, 2).await?;
    holder.commit().await?;
    first.await??;
    second.await??;
    job.run(state).await?;
    let outcome = sqlx::query_scalar!(
        "SELECT outcome FROM notification_emails WHERE notification_id = $1",
        receipt.notification_id
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(outcome.as_deref(), Some("sent"));
    let purpose = sqlx::query_scalar!("SELECT purpose FROM email_log LIMIT 1")
        .fetch_one(&pool)
        .await?;
    assert_eq!(purpose, NotificationKind::SnakeMatchmakingPaused.key());
    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| color_eyre::eyre::eyre!("wiremock did not record requests"))?;
    assert_eq!(requests.len(), 1);
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect();
    assert_html_links_in_text(&form);
    let delivered_html = form.get("html").expect("HTML part");
    let delivered_text = form.get("text").expect("text part");
    assert!(delivered_html.contains("Why you got this email"));
    assert!(delivered_text.contains("Why you got this email"));
    assert!(delivered_html.contains("/me"));
    assert!(delivered_text.contains("/me"));
    assert!(delivered_html.contains("/notifications/unsubscribe/"));
    assert!(delivered_text.contains("/notifications/unsubscribe/"));
    assert_eq!(
        form.get("h:List-Unsubscribe-Post").map(String::as_str),
        Some("List-Unsubscribe=One-Click")
    );
    assert!(
        form.get("h:List-Unsubscribe")
            .is_some_and(|value| value.starts_with('<') && value.ends_with('>'))
    );
    assert!(
        form.get("text").is_some_and(
            |value| value.contains("Unsubscribe from Snake matchmaking paused emails:")
        )
    );
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn achievement_email_uses_confirmed_player_address_and_one_layout(
    pool: PgPool,
) -> cja::Result<()> {
    use crate::customizations::achievements::{Achievement, achievement_unlock};
    use crate::email::{Mailer, MailgunConfig, messages};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1669008, 'achievement-mail-test', '', 'github@example.com') RETURNING user_id").fetch_one(&pool).await?;
    sqlx::query!(
        "UPDATE users SET notification_email = 'confirmed@example.com' WHERE user_id = $1",
        user_id
    )
    .execute(&pool)
    .await?;
    let pending_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO notification_email_requests (user_id, email) VALUES ($1, 'pending@example.com') RETURNING request_id",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await?;
    sqlx::query("UPDATE users SET pending_notification_email_request_id = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(pending_id)
        .execute(&pool)
        .await?;
    let url = "https://arena.example/customizations/achievements";
    let content = messages::achievement_earned(
        &[
            achievement_unlock(Achievement::FirstWin, true, false)?,
            achievement_unlock(Achievement::CrowdPleaser, true, true)?,
        ],
        url,
    );
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::AchievementEarned,
            title: content.subject.clone(),
            body: "First Win: head frog\nCrowd Pleaser: head monkey, tail monkey".into(),
            link: Some("/customizations/achievements".into()),
            email: Some(NewEmail {
                subject: Some(content.subject),
                text: Some(content.text),
                html_body: Some(content.html_body),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let job = SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    };
    job.run(state.clone()).await?;
    job.run(state).await?;
    let outcome = sqlx::query_scalar!(
        "SELECT outcome FROM notification_emails WHERE notification_id = $1",
        receipt.notification_id
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(outcome.as_deref(), Some("sent"));
    let requests = server.received_requests().await.expect("wiremock requests");
    assert_eq!(requests.len(), 1);
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect();
    assert_eq!(form["to"], "confirmed@example.com");
    assert_eq!(form["html"].matches("<html ").count(), 1);
    // 3 customization images + the 2 wordmark header images.
    assert_eq!(form["html"].matches("<img ").count(), 5);
    assert_eq!(
        form["html"]
            .matches("Unsubscribe from Achievement earned emails")
            .count(),
        1
    );
    assert!(form["text"].contains("set head to \"frog\""));
    assert!(form["text"].contains("set tail to \"monkey\""));
    assert!(form["text"].contains(url));
    assert_html_links_in_text(&form);
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn legacy_text_row_gets_escaped_html(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1675001, 'legacy-html-test', '', 'owner@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let notification_id = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, in_app_visible) VALUES ($1, 'snake_matchmaking_paused', 'Legacy', 'Legacy', true) RETURNING notification_id", user_id).fetch_one(&pool).await?;
    sqlx::query!("INSERT INTO notification_emails (notification_id, subject, text, recipient_user) VALUES ($1, 'Legacy', $2, true)", notification_id, "First line\n<script>alert(1)</script>\nLast line").execute(&pool).await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    SendNotificationEmailJob { notification_id }
        .run(state)
        .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect();
    assert!(form["html"].contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!form["html"].contains("<script>"));
    assert!(form["html"].find("First line").unwrap() < form["html"].find("Last line").unwrap());
    assert!(form["text"].contains("First line\n<script>alert(1)</script>\nLast line"));
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn send_time_opt_out_keeps_in_app_event(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{
        EmailRecipient, NewEmail, NewNotification, NotificationChannel, NotificationKind,
    };
    use wiremock::MockServer;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673002, 'notification-optout-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::AchievementEarned,
            title: "Achievement unlocked: First Win".into(),
            body: "First Win: head frog".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Achievement unlocked: First Win".into()),
                text: Some("First Win: head frog".into()),
                html_body: Some("<p>body</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut connection = pool.acquire().await?;
    crate::models::notification::set_preference(
        &mut connection,
        user_id,
        NotificationKind::AchievementEarned,
        NotificationChannel::Email,
        false,
    )
    .await?;
    drop(connection);
    SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    }
    .run(AppState::test_from_pool(pool.clone()))
    .await?;
    let row = sqlx::query!("SELECT n.in_app_visible, e.outcome FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.notification_id = $1", receipt.notification_id).fetch_one(&pool).await?;
    assert!(row.in_app_visible);
    assert_eq!(row.outcome.as_deref(), Some("opted_out"));
    let mut connection = pool.acquire().await?;
    crate::models::notification::set_preference(
        &mut connection,
        user_id,
        NotificationKind::AchievementEarned,
        NotificationChannel::Email,
        true,
    )
    .await?;
    drop(connection);
    let no_address = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::AchievementEarned,
            title: "Achievement unlocked: First Win".into(),
            body: "First Win: head frog".into(),
            link: Some("/customizations/achievements".into()),
            email: Some(NewEmail {
                subject: Some("Achievement unlocked: First Win".into()),
                text: Some("First Win: head frog".into()),
                html_body: Some("<p>Unlocked: Head Frog</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let server = MockServer::start().await;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    SendNotificationEmailJob {
        notification_id: no_address.notification_id,
    }
    .run(state)
    .await?;
    let row = sqlx::query!("SELECT n.in_app_visible, e.outcome FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.notification_id = $1", no_address.notification_id).fetch_one(&pool).await?;
    assert!(row.in_app_visible);
    assert_eq!(row.outcome.as_deref(), Some("no_recipient"));
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock requests")
            .is_empty()
    );
    let matchmaking = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingResumed,
            title: "Matchmaking resumed".into(),
            body: "Your snake is back on the ladder.".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Matchmaking resumed".into()),
                text: Some("Your snake is back on the ladder.".into()),
                html_body: Some("<p>Matchmaking resumed</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut connection = pool.acquire().await?;
    crate::models::notification::set_preference(
        &mut connection,
        user_id,
        NotificationKind::SnakeMatchmakingResumed,
        NotificationChannel::Email,
        false,
    )
    .await?;
    drop(connection);
    SendNotificationEmailJob {
        notification_id: matchmaking.notification_id,
    }
    .run(AppState::test_from_pool(pool.clone()))
    .await?;
    let row = sqlx::query!("SELECT n.in_app_visible, e.outcome FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.notification_id = $1", matchmaking.notification_id).fetch_one(&pool).await?;
    assert!(row.in_app_visible);
    assert_eq!(row.outcome.as_deref(), Some("opted_out"));
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn in_app_opt_out_at_raise_does_not_suppress_email(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{
        EmailRecipient, NewEmail, NewNotification, NotificationChannel, NotificationKind,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1673006, 'in-app-off-test', '', 'owner@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let mut connection = pool.acquire().await?;
    crate::models::notification::set_preference(
        &mut connection,
        user_id,
        NotificationKind::SnakeMatchmakingPaused,
        NotificationChannel::InApp,
        false,
    )
    .await?;
    drop(connection);
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingPaused,
            title: "Paused".into(),
            body: "Unhealthy".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Paused".into()),
                text: Some("body".into()),
                html_body: Some("<p>body</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    }
    .run(state)
    .await?;
    let row = sqlx::query!("SELECT n.in_app_visible, e.outcome FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.notification_id = $1", receipt.notification_id).fetch_one(&pool).await?;
    assert!(!row.in_app_visible);
    assert_eq!(row.outcome.as_deref(), Some("sent"));
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn verification_ignores_preferences_and_retries_transport_failure(
    pool: PgPool,
) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(
            move |_: &wiremock::Request| match counter.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(500),
                1 => ResponseTemplate::new(200).set_delay(Duration::from_secs(11)),
                _ => ResponseTemplate::new(200),
            },
        )
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673003, 'verification-job-test', '') RETURNING user_id").fetch_one(&pool).await?;
    sqlx::query!(
        "UPDATE users SET notification_email = 'confirmed@example.com' WHERE user_id = $1",
        user_id
    )
    .execute(&pool)
    .await?;
    let account_id = sqlx::query_scalar!("INSERT INTO imported_accounts (play_user_id, play_account_id, email, username) VALUES ('1673play', '1673account', 'play@example.com', 'player') RETURNING imported_account_id").fetch_one(&pool).await?;
    sqlx::query!("INSERT INTO notification_preferences (user_id, kind, channel, enabled) VALUES ($1, 'claim_verification', 'email', false)", user_id).execute(&pool).await?;
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::ClaimVerification,
            title: "Finish claiming your Battlesnake account".into(),
            body: "Verification requested".into(),
            link: None,
            email: Some(NewEmail {
                subject: None,
                text: None,
                html_body: None,
                recipient: EmailRecipient::Override("play@example.com".into()),
                claim_imported_account_id: Some(account_id),
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let job = SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    };
    assert!(job.run(state.clone()).await.is_err());
    let outcome = sqlx::query_scalar!(
        "SELECT outcome FROM notification_emails WHERE notification_id = $1",
        receipt.notification_id
    )
    .fetch_one(&pool)
    .await?;
    assert!(outcome.is_none());
    let tokens = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM claim_email_tokens")
        .fetch_one(&pool)
        .await?;
    let logs = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log")
        .fetch_one(&pool)
        .await?;
    assert_eq!((tokens, logs), (0, 0));
    let started = std::time::Instant::now();
    assert!(job.run(state.clone()).await.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(11),
        "request timeout was not enforced"
    );
    let tokens = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM claim_email_tokens")
        .fetch_one(&pool)
        .await?;
    let logs = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log")
        .fetch_one(&pool)
        .await?;
    assert_eq!((tokens, logs), (0, 0));
    job.run(state.clone()).await?;
    let outcome = sqlx::query_scalar!(
        "SELECT outcome FROM notification_emails WHERE notification_id = $1",
        receipt.notification_id
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(outcome.as_deref(), Some("sent"));
    let tokens = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM claim_email_tokens")
        .fetch_one(&pool)
        .await?;
    assert_eq!(tokens, 1);
    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| color_eyre::eyre::eyre!("wiremock did not record requests"))?;
    assert_eq!(requests.len(), 3);
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[2].body)
        .into_owned()
        .collect();
    assert_html_links_in_text(&form);
    assert_eq!(form.get("to").map(String::as_str), Some("play@example.com"));
    let delivered_text = form.get("text").expect("verification text");
    let delivered_html = form.get("html").expect("verification HTML");
    let token = delivered_text
        .split("/claim/email/verify?token=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("delivered bearer token");
    assert_eq!(token.len(), 64);
    assert!(delivered_html.contains(token));
    assert!(delivered_html.contains("/me"));
    let persisted: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(n)::text FROM notifications n
         UNION ALL SELECT row_to_json(e)::text FROM notification_emails e
         UNION ALL SELECT payload::text FROM jobs",
    )
    .fetch_all(&pool)
    .await?;
    assert!(
        persisted.iter().all(|row| !row.contains(token)),
        "raw claim token leaked into notification or job storage"
    );
    assert!(!form.contains_key("h:List-Unsubscribe"));
    assert!(!form.contains_key("h:List-Unsubscribe-Post"));
    assert!(
        !form
            .get("text")
            .is_some_and(|value| value.contains("Unsubscribe"))
    );
    let claimed = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::PlayAccountClaimed,
            title: "Claimed".into(),
            body: "Body".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Claimed".into()),
                text: Some("Body".into()),
                html_body: Some("<p>body</p>".into()),
                recipient: EmailRecipient::Override("play@example.com".into()),
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await?;
    SendNotificationEmailJob {
        notification_id: claimed.notification_id,
    }
    .run(state)
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[3].body)
        .into_owned()
        .collect();
    assert_eq!(form.get("to").map(String::as_str), Some("play@example.com"));
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn terminal_delivery_conditions_do_not_retry(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1673004, 'terminal-job-test', '', 'owner@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let make = |recipient| NewNotification {
        user_id,
        kind: NotificationKind::SnakeMatchmakingPaused,
        title: "Paused".into(),
        body: "Unhealthy".into(),
        link: None,
        email: Some(NewEmail {
            subject: Some("Paused".into()),
            text: Some("body".into()),
            html_body: Some("<p>body</p>".into()),
            recipient,
            claim_imported_account_id: None,
            notification_email_request_id: None,
        }),
    };
    let disabled = crate::notifications::record(&pool, make(EmailRecipient::User)).await?;
    let missing =
        crate::notifications::record(&pool, make(EmailRecipient::Snake(uuid::Uuid::new_v4())))
            .await?;
    let limited = crate::notifications::record(&pool, make(EmailRecipient::User)).await?;
    SendNotificationEmailJob {
        notification_id: disabled.notification_id,
    }
    .run(AppState::test_from_pool(pool.clone()))
    .await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    SendNotificationEmailJob {
        notification_id: missing.notification_id,
    }
    .run(state.clone())
    .await?;
    let mut config = (*state.config).clone();
    config.email_per_recipient_hourly_limit = 0;
    state.config = std::sync::Arc::new(config);
    SendNotificationEmailJob {
        notification_id: limited.notification_id,
    }
    .run(state.clone())
    .await?;
    let legacy_id = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, in_app_visible) VALUES ($1, 'snake_matchmaking_paused', 'Legacy', 'Body', true) RETURNING notification_id", user_id).fetch_one(&pool).await?;
    sqlx::query!("INSERT INTO notification_emails (notification_id, subject, text, recipient_override) VALUES ($1, 'Legacy', 'Body', 'legacy@example.com')", legacy_id).execute(&pool).await?;
    SendNotificationEmailJob {
        notification_id: legacy_id,
    }
    .run(state.clone())
    .await?;
    let snake_id = sqlx::query_scalar!("INSERT INTO battlesnakes (user_id, name, url, deleted_at) VALUES ($1, 'deleted-snake', 'https://example.com', NOW()) RETURNING battlesnake_id", user_id).fetch_one(&pool).await?;
    let deleted =
        crate::notifications::record(&pool, make(EmailRecipient::Snake(snake_id))).await?;
    SendNotificationEmailJob {
        notification_id: deleted.notification_id,
    }
    .run(state)
    .await?;
    for (id, expected) in [
        (disabled.notification_id, "transport_disabled"),
        (missing.notification_id, "no_recipient"),
        (limited.notification_id, "rate_limited"),
        (legacy_id, "no_recipient"),
        (deleted.notification_id, "no_recipient"),
    ] {
        let outcome = sqlx::query_scalar!(
            "SELECT outcome FROM notification_emails WHERE notification_id = $1",
            id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(outcome.as_deref(), Some(expected));
    }
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn waiting_email_jobs_do_not_hold_pool_connections(pool: PgPool) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use sqlx::postgres::PgPoolOptions;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .expect(2)
        .mount(&server)
        .await;
    let options = pool
        .connect_options()
        .as_ref()
        .clone()
        .application_name("notification-delivery-test");
    let delivery_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1673005, 'semaphore-job-test', '', 'owner@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let mut ids = Vec::new();
    for _address in ["first@example.com", "second@example.com"] {
        let receipt = crate::notifications::record(
            &pool,
            NewNotification {
                user_id,
                kind: NotificationKind::SnakeMatchmakingPaused,
                title: "Paused".into(),
                body: "Unhealthy".into(),
                link: None,
                email: Some(NewEmail {
                    subject: Some("Paused".into()),
                    text: Some("body".into()),
                    html_body: Some("<p>body</p>".into()),
                    recipient: EmailRecipient::User,
                    claim_imported_account_id: None,
                    notification_email_request_id: None,
                }),
            },
        )
        .await?;
        ids.push(receipt.notification_id);
    }
    let mut state = AppState::test_from_pool(delivery_pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let first_id = ids[0];
    let first_state = state.clone();
    let first = tokio::spawn(async move {
        SendNotificationEmailJob {
            notification_id: first_id,
        }
        .run(first_state)
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'notification-delivery-test' AND state = 'idle in transaction'").fetch_one(&pool).await?;
            if count == 1 { break Ok::<_, color_eyre::Report>(()); }
            tokio::task::yield_now().await;
        }
    }).await??;
    let second_id = ids[1];
    let second = tokio::spawn(async move {
        SendNotificationEmailJob {
            notification_id: second_id,
        }
        .run(state)
        .await
    });
    let unrelated = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_scalar!("SELECT 1 AS \"one!\"").fetch_one(&delivery_pool),
    )
    .await??;
    assert_eq!(unrelated, 1);
    while !first.is_finished() {
        let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'notification-delivery-test' AND state = 'idle in transaction'").fetch_one(&pool).await?;
        assert!(
            count <= 1,
            "only the active delivery may hold a transaction"
        );
        tokio::task::yield_now().await;
    }
    first.await??;
    second.await??;
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn confirmation_uses_hashed_single_use_link_and_user_mail_uses_confirmed_address(
    pool: PgPool,
) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::models::notification_email::{self, ConfirmationRequestResult};
    use crate::notifications::{EmailRecipient, NewEmail, NewNotification, NotificationKind};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(6)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token, github_email) VALUES (1674091, 'confirmation-job-test', '', 'public@example.com') RETURNING user_id").fetch_one(&pool).await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let mut tx = pool.begin().await?;
    let (request_id, confirmation_id) =
        match notification_email::request_confirmation(&mut tx, user_id, "confirmed@example.com")
            .await?
        {
            ConfirmationRequestResult::Requested {
                request_id,
                receipt,
            } => (request_id, receipt.notification_id),
            _ => panic!("first request must enqueue"),
        };
    tx.commit().await?;
    SendNotificationEmailJob {
        notification_id: confirmation_id,
    }
    .run(state.clone())
    .await?;
    SendNotificationEmailJob {
        notification_id: confirmation_id,
    }
    .run(state.clone())
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    assert_eq!(requests.len(), 1);
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[0].body)
        .into_owned()
        .collect();
    assert_html_links_in_text(&form);
    assert_eq!(
        form.get("to").map(String::as_str),
        Some("confirmed@example.com")
    );
    assert!(!form.contains_key("h:List-Unsubscribe"));
    let text = form.get("text").expect("confirmation body");
    let html = form.get("html").expect("confirmation HTML");
    assert!(html.contains("/me"));
    assert!(html.contains("24 hours"));
    assert!(html.contains("signed-out browser"));
    assert!(html.contains("you can ignore this email"));
    let secret = text
        .split("token=")
        .nth(1)
        .expect("confirmation token")
        .split_whitespace()
        .next()
        .expect("token end");
    let row = sqlx::query!("SELECT token_hash, expires_at, sent_at FROM notification_email_requests WHERE request_id = $1", request_id).fetch_one(&pool).await?;
    assert_eq!(
        row.token_hash.as_deref(),
        Some(notification_email::hash_secret(secret).as_str())
    );
    assert!(!format!("{row:?}").contains(secret));
    assert!(row.sent_at.is_some());
    let ttl = row.expires_at.expect("expiry") - chrono::Utc::now();
    assert!(ttl.num_hours() >= 23 && ttl.num_hours() <= 24);
    let send_user = |user_id| NewNotification {
        user_id,
        kind: NotificationKind::SnakeMatchmakingPaused,
        title: "Paused".into(),
        body: "Body".into(),
        link: None,
        email: Some(NewEmail {
            subject: Some("Paused".into()),
            text: Some("Body".into()),
            html_body: Some("<p>body</p>".into()),
            recipient: EmailRecipient::User,
            claim_imported_account_id: None,
            notification_email_request_id: None,
        }),
    };
    let pending = crate::notifications::record(&pool, send_user(user_id)).await?;
    SendNotificationEmailJob {
        notification_id: pending.notification_id,
    }
    .run(state.clone())
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[1].body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("to").map(String::as_str),
        Some("public@example.com")
    );
    assert!(notification_email::consume(&pool, user_id, secret).await?);
    assert!(!notification_email::consume(&pool, user_id, secret).await?);

    let receipt = crate::notifications::record(&pool, send_user(user_id)).await?;
    SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    }
    .run(state.clone())
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[2].body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("to").map(String::as_str),
        Some("confirmed@example.com")
    );

    let mut tx = pool.begin().await?;
    let replacement_id =
        match notification_email::request_confirmation(&mut tx, user_id, "replacement@example.com")
            .await?
        {
            ConfirmationRequestResult::Requested { receipt, .. } => receipt.notification_id,
            _ => panic!("replacement must enqueue"),
        };
    tx.commit().await?;
    let receipt = crate::notifications::record(&pool, send_user(user_id)).await?;
    SendNotificationEmailJob {
        notification_id: receipt.notification_id,
    }
    .run(state.clone())
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[3].body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("to").map(String::as_str),
        Some("confirmed@example.com")
    );
    let mut tx = pool.begin().await?;
    notification_email::clear(&mut tx, user_id).await?;
    tx.commit().await?;
    SendNotificationEmailJob {
        notification_id: replacement_id,
    }
    .run(state.clone())
    .await?;
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT outcome FROM notification_emails WHERE notification_id = $1",
            replacement_id
        )
        .fetch_one(&pool)
        .await?
        .as_deref(),
        Some("no_recipient")
    );
    let public = crate::notifications::record(&pool, send_user(user_id)).await?;
    SendNotificationEmailJob {
        notification_id: public.notification_id,
    }
    .run(state.clone())
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[4].body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("to").map(String::as_str),
        Some("public@example.com")
    );
    sqlx::query!(
        "UPDATE users SET github_email = NULL WHERE user_id = $1",
        user_id
    )
    .execute(&pool)
    .await?;
    sqlx::query!("INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, claimed_by_user_id) VALUES ('job-play', 'job-account', 'play@example.com', 'player', $1)", user_id).execute(&pool).await?;
    let play = crate::notifications::record(&pool, send_user(user_id)).await?;
    SendNotificationEmailJob {
        notification_id: play.notification_id,
    }
    .run(state)
    .await?;
    let requests = server.received_requests().await.expect("wiremock requests");
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&requests[5].body)
        .into_owned()
        .collect();
    assert_eq!(form.get("to").map(String::as_str), Some("play@example.com"));
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn confirmation_actual_send_budget_blocks_sixth_delayed_request(
    pool: PgPool,
) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::models::notification_email::{self, ConfirmationRequestResult};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(5)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1674092, 'confirmation-cap-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let mut config = (*state.config).clone();
    config.email_per_recipient_hourly_limit = 100;
    state.config = std::sync::Arc::new(config);
    for n in 0..6 {
        if n == 5 {
            sqlx::query!("UPDATE notification_email_requests SET created_at = NOW() - INTERVAL '25 hours' WHERE user_id = $1", user_id).execute(&pool).await?;
        }
        let mut tx = pool.begin().await?;
        let result = if n == 0 {
            notification_email::request_confirmation(&mut tx, user_id, "target@example.com").await?
        } else {
            notification_email::resend(&mut tx, user_id).await?
        };
        let notification_id = match result {
            ConfirmationRequestResult::Requested { receipt, .. } => receipt.notification_id,
            _ => panic!("request {n} should enqueue"),
        };
        tx.commit().await?;
        SendNotificationEmailJob { notification_id }
            .run(state.clone())
            .await?;
        let outcome = sqlx::query_scalar!(
            "SELECT outcome FROM notification_emails WHERE notification_id = $1",
            notification_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            outcome.as_deref(),
            Some(if n == 5 { "rate_limited" } else { "sent" })
        );
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("wiremock requests")
            .len(),
        5
    );
    Ok(())
}

#[sqlx::test(migrations = "../migrations")]
async fn confirmation_transport_error_rolls_back_secret_and_retry_mints_another(
    pool: PgPool,
) -> cja::Result<()> {
    use crate::email::{Mailer, MailgunConfig};
    use crate::models::notification_email::{self, ConfirmationRequestResult};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1674094, 'confirmation-retry-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let mut tx = pool.begin().await?;
    let (request_id, notification_id) =
        match notification_email::request_confirmation(&mut tx, user_id, "retry@example.com")
            .await?
        {
            ConfirmationRequestResult::Requested {
                request_id,
                receipt,
            } => (request_id, receipt.notification_id),
            _ => panic!("request must enqueue"),
        };
    tx.commit().await?;
    let mut state = AppState::test_from_pool(pool.clone());
    state.mailer = Mailer::new(
        Some(MailgunConfig {
            api_key: "test".into(),
            domain: "mg.example.com".into(),
            from: "Arena <reply@example.com>".into(),
            base_url: server.uri(),
        }),
        reqwest::Client::new(),
    );
    let job = SendNotificationEmailJob { notification_id };
    assert!(job.run(state.clone()).await.is_err());
    let row = sqlx::query!("SELECT token_hash, expires_at, sent_at FROM notification_email_requests WHERE request_id = $1", request_id).fetch_one(&pool).await?;
    assert!(row.token_hash.is_none() && row.expires_at.is_none() && row.sent_at.is_none());
    assert!(
        sqlx::query_scalar!(
            "SELECT outcome FROM notification_emails WHERE notification_id = $1",
            notification_id
        )
        .fetch_one(&pool)
        .await?
        .is_none()
    );
    job.run(state.clone()).await?;
    job.run(state).await?;
    let row = sqlx::query!("SELECT token_hash, expires_at, sent_at FROM notification_email_requests WHERE request_id = $1", request_id).fetch_one(&pool).await?;
    assert!(row.token_hash.is_some() && row.expires_at.is_some() && row.sent_at.is_some());
    let requests = server.received_requests().await.expect("wiremock requests");
    assert_eq!(requests.len(), 2);
    let tokens: Vec<_> = requests
        .iter()
        .map(|request| {
            let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&request.body)
                .into_owned()
                .collect();
            form["text"]
                .split("token=")
                .nth(1)
                .expect("token")
                .split_whitespace()
                .next()
                .expect("token end")
                .to_string()
        })
        .collect();
    assert_ne!(tokens[0], tokens[1]);
    assert_eq!(
        row.token_hash.as_deref(),
        Some(notification_email::hash_secret(&tokens[1]).as_str())
    );
    Ok(())
}
