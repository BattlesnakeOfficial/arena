use std::{path::Path, process::Command, time::Duration};

use cja::jobs::{
    Job,
    worker::{DEFAULT_MAX_RETRIES, job_worker},
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use super::{AppState, Jobs, SendNotificationEmailJob};
use crate::config::{AppConfig, DEFAULT_JOB_SHUTDOWN_DRAIN_SECS};

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
        AppConfig::test_default()
            .job
            .worker_config(Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS)),
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
            AppConfig::test_default()
                .job
                .worker_config(Duration::from_secs(DEFAULT_JOB_SHUTDOWN_DRAIN_SECS)),
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
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673001, 'notification-job-test', '') RETURNING user_id").fetch_one(&pool).await?;
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
                recipient: EmailRecipient::Override("owner@example.com".into()),
                claim_imported_account_id: None,
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
async fn send_time_opt_out_keeps_in_app_event(pool: PgPool) -> cja::Result<()> {
    use crate::notifications::{
        EmailRecipient, NewEmail, NewNotification, NotificationChannel, NotificationKind,
    };
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673002, 'notification-optout-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let receipt = crate::notifications::record(
        &pool,
        NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingResumed,
            title: "Resumed".into(),
            body: "Recovered".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Resumed".into()),
                text: Some("Recovered".into()),
                recipient: EmailRecipient::Override("owner@example.com".into()),
                claim_imported_account_id: None,
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
        notification_id: receipt.notification_id,
    }
    .run(AppState::test_from_pool(pool.clone()))
    .await?;
    let row = sqlx::query!("SELECT n.in_app_visible, e.outcome FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.notification_id = $1", receipt.notification_id).fetch_one(&pool).await?;
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
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673006, 'in-app-off-test', '') RETURNING user_id").fetch_one(&pool).await?;
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
                recipient: EmailRecipient::Override("owner@example.com".into()),
                claim_imported_account_id: None,
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
                recipient: EmailRecipient::Override("play@example.com".into()),
                claim_imported_account_id: Some(account_id),
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
    job.run(state).await?;
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
    let delivered_text = form.get("text").expect("verification text");
    let token = delivered_text
        .split("/claim/email/verify?token=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("delivered bearer token");
    assert_eq!(token.len(), 64);
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
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673004, 'terminal-job-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let make = |recipient| NewNotification {
        user_id,
        kind: NotificationKind::SnakeMatchmakingPaused,
        title: "Paused".into(),
        body: "Unhealthy".into(),
        link: None,
        email: Some(NewEmail {
            subject: Some("Paused".into()),
            text: Some("body".into()),
            recipient,
            claim_imported_account_id: None,
        }),
    };
    let disabled = crate::notifications::record(
        &pool,
        make(EmailRecipient::Override("owner@example.com".into())),
    )
    .await?;
    let missing =
        crate::notifications::record(&pool, make(EmailRecipient::Snake(uuid::Uuid::new_v4())))
            .await?;
    let limited = crate::notifications::record(
        &pool,
        make(EmailRecipient::Override("other@example.com".into())),
    )
    .await?;
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
    .run(state)
    .await?;
    for (id, expected) in [
        (disabled.notification_id, "transport_disabled"),
        (missing.notification_id, "no_recipient"),
        (limited.notification_id, "rate_limited"),
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
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (1673005, 'semaphore-job-test', '') RETURNING user_id").fetch_one(&pool).await?;
    let mut ids = Vec::new();
    for address in ["first@example.com", "second@example.com"] {
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
                    recipient: EmailRecipient::Override(address.into()),
                    claim_imported_account_id: None,
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
