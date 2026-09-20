#![allow(dead_code)]

use std::time::Duration;

use cja::{
    server::run_server_until,
    setup::{TracingConfig, setup_sentry},
};
use color_eyre::eyre::{Context as _, eyre};
use futures::stream::{FuturesUnordered, StreamExt as _};
use state::AppState;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod backup;
mod cache;
mod config;
mod cron;
mod customizations;
mod discord;
mod django_password;
mod email;
mod engine;
mod engine_models;
mod errors;
mod flasher;
mod game_channels;
mod game_progress;
mod game_runner;
mod github;
mod jobs;
mod leaderboard_matchmaker;
mod leaderboard_ratings;
mod models;
mod moderation;
mod observability;
mod play_import;
mod routes;
mod scoring;
mod snake_client;
mod snake_health;
mod snake_health_sweeper;
mod state;
mod static_assets;
mod stuck_game_sweeper;
mod telemetry;
mod tournament_bracket;
mod tournament_match;
mod wire;

/// Frontend UI components only - do not place backend logic here
mod components {
    pub mod avatar;
    pub mod flash;
    pub mod live_refresh;
    pub mod page;
    pub mod page_factory;
    pub mod snake_tags;
}

fn main() -> color_eyre::Result<()> {
    // Initialize Sentry for error tracking
    let _sentry_guard = setup_sentry();

    // One-shot subcommand: copy play's DB into the migration staging
    // tables, then exit (no server, no job workers).
    if std::env::args().nth(1).as_deref() == Some("import-play") {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async { play_import::run_import().await });
    }

    // Read all configuration once, here, before anything else. Downstream
    // code takes values from this struct (via AppState) rather than
    // reaching for the environment itself.
    let config = config::AppConfig::from_env()?;

    // Configure tokio worker threads as a multiplier on CPU core count.
    // Since game execution is I/O-bound (snake API calls ~500ms each),
    // we want many more threads than cores to maximize throughput.
    let core_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let worker_threads = core_count * config.tokio_worker_multiplier;
    eprintln!(
        "Tokio workers: {worker_threads} ({core_count} cores x {} multiplier)",
        config.tokio_worker_multiplier
    );

    // Create and run the tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()?
        .block_on(async { run_application(config).await })
}

async fn run_application(config: config::AppConfig) -> cja::Result<()> {
    let identity = eyes_subscriber::ProcessIdentity::new(observability::ROLE);
    let eyes_shutdown_handle = if config.gcp_logging {
        telemetry::setup_gcp_tracing(&config.rust_log, config.eyes.as_ref(), &identity)?
    } else {
        TracingConfig::new("arena")
            .process(identity.clone())
            .init()?
    };
    let result = run_instrumented_application(config, identity).await;
    // Preserve the final error before flushing, including startup failures.
    if let Err(error) = &result {
        tracing::error!(error = %format!("{error:#}"), "Arena process exiting after failure");
    }
    if let Some(handle) = eyes_shutdown_handle
        && let Err(error) = handle.shutdown().await
    {
        tracing::warn!(%error, "Error shutting down Eyes");
    }
    result
}

/// Time tasks get to exit after the job drain ends: job workers release
/// their locks, the HTTP server closes connections. Together with
/// `JobConfig::shutdown_drain_secs` this must stay under Cloud Run's fixed
/// 10 second SIGTERM-to-SIGKILL window, with room left to flush telemetry.
pub(crate) const SHUTDOWN_EXIT_GRACE: Duration = Duration::from_secs(3);

/// Cloud Run sends SIGKILL this long after SIGTERM; not configurable.
const CLOUD_RUN_TERMINATION_WINDOW: Duration = Duration::from_secs(10);

async fn run_instrumented_application(
    config: config::AppConfig,
    identity: eyes_subscriber::ProcessIdentity,
) -> cja::Result<()> {
    let app_state = AppState::from_config(config).await?;
    let shutdown = CancellationToken::new();
    let exit_deadline =
        Duration::from_secs(app_state.config.job.shutdown_drain_secs) + SHUTDOWN_EXIT_GRACE;
    if app_state.config.gcp_logging && exit_deadline >= CLOUD_RUN_TERMINATION_WINDOW {
        tracing::warn!(
            deadline_secs = exit_deadline.as_secs(),
            "Shutdown deadline exceeds Cloud Run's 10s termination window; \
             job locks will be stranded on deploy. Lower ARENA_JOB_SHUTDOWN_DRAIN_SECS"
        );
    }
    let signal = shutdown_signal()?;
    let (tasks, heartbeat) = spawn_application_tasks(app_state, identity, shutdown.clone()).await?;
    let result = if tasks.is_empty() {
        Ok(())
    } else {
        supervise(tasks, shutdown, signal, exit_deadline).await
    };
    if let Some(handle) = heartbeat
        && let Err(error) = handle.shutdown().await
    {
        tracing::warn!(%error, "Eyes process shutdown signal failed");
    }
    result
}

/// Resolves with the name of the first shutdown signal received. Handlers
/// are registered eagerly so a signal arriving during startup is not lost.
fn shutdown_signal() -> cja::Result<impl Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).wrap_err("Failed to register SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).wrap_err("Failed to register SIGINT")?;
    Ok(async move {
        tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        }
    })
}

struct NamedTask {
    name: &'static str,
    handle: tokio::task::JoinHandle<cja::Result<()>>,
}

impl NamedTask {
    fn spawn<F>(name: &'static str, future: F) -> Self
    where
        F: std::future::Future<Output = cja::Result<()>> + Send + 'static,
    {
        Self {
            name,
            handle: tokio::spawn(future),
        }
    }
}

/// Run until a shutdown signal arrives or any task exits on its own.
///
/// Either way every task is then told to stop through `shutdown` and given
/// `exit_deadline` to finish: job workers drain what they can and release
/// their locks so another instance picks the rest up immediately, instead of
/// after the lock timeout. Tasks still running at the deadline are aborted.
///
/// A signal is a clean exit. A task exiting by itself is always an error,
/// because every task is meant to run for the life of the process.
async fn supervise(
    tasks: Vec<NamedTask>,
    shutdown: CancellationToken,
    signal: impl Future<Output = &'static str>,
    exit_deadline: Duration,
) -> cja::Result<()> {
    let aborts: Vec<_> = tasks
        .iter()
        .map(|task| (task.name, task.handle.abort_handle()))
        .collect();
    let mut running: FuturesUnordered<_> = tasks
        .into_iter()
        .map(|task| async move { (task.name, task.handle.await) })
        .collect();

    let result = tokio::select! {
        signal = signal => {
            info!(signal, deadline_secs = exit_deadline.as_secs(), "Shutdown signal received; draining");
            Ok(())
        }
        Some((name, result)) = running.next() => match result {
            Ok(Ok(())) => Err(eyre!("Task '{}' exited unexpectedly", name)),
            Ok(Err(error)) => Err(error.wrap_err(format!("Task '{name}' failed"))),
            Err(error) => Err(eyre!("Task '{}' panicked: {}", name, error)),
        },
    };

    shutdown.cancel();
    let drained = tokio::time::timeout(exit_deadline, async {
        while let Some((name, result)) = running.next().await {
            match result {
                Ok(Ok(())) => tracing::debug!(task = name, "Task stopped"),
                Ok(Err(error)) => {
                    tracing::warn!(task = name, error = %format!("{error:#}"), "Task failed while stopping");
                }
                Err(error) => tracing::warn!(task = name, %error, "Task panicked while stopping"),
            }
        }
    })
    .await;
    if drained.is_err() {
        let stragglers: Vec<_> = aborts
            .iter()
            .filter(|(_, handle)| !handle.is_finished())
            .map(|(name, _)| *name)
            .collect();
        tracing::warn!(
            ?stragglers,
            "Shutdown deadline reached; aborting remaining tasks"
        );
        for (_, handle) in &aborts {
            handle.abort();
        }
    } else {
        info!("All tasks stopped");
    }

    result
}

/// Spawn all application background tasks
async fn spawn_application_tasks(
    app_state: AppState,
    identity: eyes_subscriber::ProcessIdentity,
    shutdown: CancellationToken,
) -> cja::Result<(
    Vec<NamedTask>,
    Option<eyes_subscriber::ProcessHeartbeatHandle>,
)> {
    let mut tasks = vec![];
    let features = app_state.config.features;
    let job = &app_state.config.job;

    // Build the registry once; the manifest and worker both use it when
    // this process has CRON enabled.
    let cron_registry = cron::cron_registry();

    let manifest = observability::manifest(&cron_registry, identity, features)
        .map_err(|error| eyre!("Invalid Arena observability declarations: {error}"))?;

    if features.server {
        info!("Server Enabled");
        tasks.push(NamedTask::spawn(
            "server",
            run_server_until(
                routes::routes(app_state.clone()),
                shutdown.clone().cancelled_owned(),
            ),
        ));
    } else {
        info!("Server Disabled");
    }

    if features.jobs {
        info!("Jobs Enabled");
        info!("Job poll interval: {}ms", job.poll_interval_ms);
        info!("Job lock timeout: {}s", job.lock_timeout_secs);
        info!("Job max retries: {}", job.max_retries);
        info!("Job workers: {}", job.workers);
        info!("Job shutdown drain: {}s", job.shutdown_drain_secs);

        for i in 0..job.workers {
            let name: &'static str = Box::leak(format!("jobs-{i}").into_boxed_str());
            tasks.push(NamedTask::spawn(
                name,
                cja::jobs::worker::job_worker_with_shutdown_drain(
                    app_state.clone(),
                    jobs::Jobs,
                    Duration::from_millis(job.poll_interval_ms),
                    job.max_retries,
                    shutdown.clone(),
                    Duration::from_secs(job.lock_timeout_secs),
                    Duration::from_secs(job.shutdown_drain_secs),
                ),
            ));
        }
    } else {
        info!("Jobs Disabled");
    }

    if features.cron {
        info!("Cron Enabled");
        tasks.push(NamedTask::spawn(
            "cron",
            cron::run_cron(app_state.clone(), cron_registry, shutdown.clone()),
        ));
    } else {
        info!("Cron Disabled");
    }

    info!("All application tasks spawned successfully");
    // Workers are already serving while the bounded registration runs.
    let heartbeat = observability::start(&app_state.config, &manifest).await;
    Ok((tasks, heartbeat))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task that stops as soon as it is asked to, like the real workers.
    fn cooperative(name: &'static str, shutdown: &CancellationToken) -> NamedTask {
        let shutdown = shutdown.clone();
        NamedTask::spawn(name, async move {
            shutdown.cancelled().await;
            Ok(())
        })
    }

    #[tokio::test(start_paused = true)]
    async fn signal_stops_every_task_and_exits_cleanly() {
        let shutdown = CancellationToken::new();
        let tasks = vec![
            cooperative("jobs-0", &shutdown),
            cooperative("cron", &shutdown),
        ];

        let result = supervise(
            tasks,
            shutdown.clone(),
            async { "SIGTERM" },
            Duration::from_secs(8),
        )
        .await;

        assert!(result.is_ok(), "a signal is a clean exit: {result:?}");
        assert!(shutdown.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn task_ignoring_shutdown_is_aborted_at_the_deadline() {
        let shutdown = CancellationToken::new();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = finished.clone();
        let tasks = vec![
            cooperative("jobs-0", &shutdown),
            NamedTask::spawn("server", async move {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        ];

        let started = tokio::time::Instant::now();
        let result = supervise(tasks, shutdown, async { "SIGTERM" }, Duration::from_secs(8)).await;

        assert!(
            result.is_ok(),
            "stragglers do not turn a signal into a failure"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(8));
        tokio::time::sleep(Duration::from_secs(7200)).await;
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the straggler must be aborted, not left running"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn task_exiting_on_its_own_fails_the_process_but_still_drains_the_rest() {
        let shutdown = CancellationToken::new();
        let drained = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = drained.clone();
        let worker_shutdown = shutdown.clone();
        let tasks = vec![
            NamedTask::spawn("cron", async { Err(eyre!("database went away")) }),
            NamedTask::spawn("jobs-0", async move {
                worker_shutdown.cancelled().await;
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        ];

        let error = supervise(
            tasks,
            shutdown,
            std::future::pending(),
            Duration::from_secs(8),
        )
        .await
        .expect_err("a task exiting by itself is a failure");

        let rendered = format!("{error:#}");
        assert!(rendered.contains("Task 'cron' failed"), "{rendered}");
        assert!(rendered.contains("database went away"), "{rendered}");
        assert!(
            drained.load(std::sync::atomic::Ordering::SeqCst),
            "surviving workers must get the chance to release their locks"
        );
    }

    #[test]
    fn boot_manifest_declares_exactly_one_health_monitor() {
        let registry = cron::cron_registry();
        let manifest = observability::manifest(
            &registry,
            eyes_subscriber::ProcessIdentity::new(observability::ROLE),
            config::FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        )
        .unwrap();

        assert_eq!(
            manifest.base_url.as_deref(),
            Some(config::ARENA_PUBLIC_BASE_URL)
        );

        let monitors = manifest.monitors.expect("monitors declared");
        assert_eq!(monitors.len(), 1);
        let monitor = &monitors[0];
        assert_eq!(monitor.id, "health");
        assert!(monitor.enabled);
        assert_eq!(monitor.method, cja::eyes_manifest::HttpMethod::Get);

        let resolved =
            eyes_subscriber::resolve_monitor_target(manifest.base_url.as_deref(), &monitor.target)
                .expect("monitor target resolves against base_url");
        assert_eq!(resolved.as_str(), "https://arena.battlesnake.com/health");
    }
}
