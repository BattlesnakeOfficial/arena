use std::{path::Path, process::Command, time::Duration};

use cja::jobs::worker::{DEFAULT_LOCK_TIMEOUT, DEFAULT_MAX_RETRIES, job_worker};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use super::{AppState, Jobs};

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
    let worker = job_worker(
        AppState::test_from_pool(pool.clone()),
        Jobs,
        Duration::from_millis(10),
        DEFAULT_MAX_RETRIES,
        shutdown.clone(),
        DEFAULT_LOCK_TIMEOUT,
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
    assert_eq!(archived.error_count, DEFAULT_MAX_RETRIES);
    assert_eq!(
        archived.last_error_message.as_deref(),
        Some("Unknown job type: MigrationTestFailure")
    );
    assert_eq!(archived.created_at, created_at);
    assert!(archived.failed_at >= created_at);
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
