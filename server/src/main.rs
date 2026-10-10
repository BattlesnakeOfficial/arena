#![allow(dead_code)]

use std::time::Duration;

use cja::{
    server::run_server_until,
    setup::{TracingConfig, setup_sentry},
    tasks::{ShutdownBudget, Supervisor},
};
use color_eyre::eyre::{Context as _, eyre};
use state::AppState;
use tracing::info;

mod activity;
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
mod game_progress;
mod game_runner;
mod github;
mod jobs;
mod leaderboard_matchmaker;
mod leaderboard_ratings;
mod models;
mod moderation;
mod observability;
mod og;
mod placement;
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
mod watched_games;
mod wire;

/// Frontend UI components only - do not place backend logic here
mod components {
    pub mod avatar;
    pub mod flash;
    pub mod latency_chart;
    pub mod live_refresh;
    pub mod page;
    pub mod page_factory;
    pub mod snake_board;
    pub mod snake_tags;
}

fn main() -> color_eyre::Result<()> {
    // Hidden one-shot subcommand: the Head & Tail Studio's upload worker, a child of the
    // server (see `arena::studio_worker`). It runs before anything else on purpose: no
    // Sentry, config, telemetry or database, just stdin to stdout.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(arena::studio_worker::SUBCOMMAND) {
        std::process::exit(arena::studio_worker::worker_main(args));
    }

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

    if std::env::args().nth(1).as_deref() == Some("export-play-regions") {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(play_import::export_play_regions());
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

#[derive(Debug, PartialEq, Eq)]
enum ProcessTask {
    Server,
    Jobs(usize),
    Cron,
}

fn scheduled_tasks(features: config::FeatureFlags, job_workers: usize) -> Vec<ProcessTask> {
    let mut tasks = Vec::new();
    if features.server {
        tasks.push(ProcessTask::Server);
    }
    if features.jobs {
        tasks.extend((0..job_workers).map(ProcessTask::Jobs));
    }
    if features.cron {
        tasks.push(ProcessTask::Cron);
    }
    tasks
}

async fn run_instrumented_application(
    config: config::AppConfig,
    identity: eyes_subscriber::ProcessIdentity,
) -> cja::Result<()> {
    let app_state = AppState::from_config(config).await?;
    let mut supervisor = Supervisor::new(ShutdownBudget {
        job_drain: Duration::from_secs(app_state.config.job.shutdown_drain_secs),
        exit_grace: SHUTDOWN_EXIT_GRACE,
    })?;
    let heartbeat = spawn_application_tasks(app_state, identity, &mut supervisor).await?;
    let result = supervisor.run().await;
    if let Some(handle) = heartbeat
        && let Err(error) = handle.shutdown().await
    {
        tracing::warn!(%error, "Eyes process shutdown signal failed");
    }
    result
}

/// Spawn all application background tasks
async fn spawn_application_tasks(
    app_state: AppState,
    identity: eyes_subscriber::ProcessIdentity,
    supervisor: &mut Supervisor,
) -> cja::Result<Option<eyes_subscriber::ProcessHeartbeatHandle>> {
    let shutdown = supervisor.shutdown_token();
    let features = app_state.config.features;
    let job = &app_state.config.job;

    // Build the registry once; the manifest and worker both use it when
    // this process has CRON enabled.
    let cron_registry = cron::cron_registry();

    let manifest = observability::manifest(&cron_registry, identity, features)
        .map_err(|error| eyre!("Invalid Arena observability declarations: {error}"))?;

    if scheduled_tasks(features, job.workers).contains(&ProcessTask::Server) {
        info!("Server Enabled");
        supervisor.spawn(
            "watched-games",
            watched_games::run_watched_games(
                app_state.db.clone(),
                app_state
                    .config
                    .database_url
                    .parse()
                    .wrap_err("Invalid listener database URL")?,
                app_state.watched_games.clone(),
                shutdown.clone(),
            ),
        );
        supervisor.spawn(
            "server",
            run_server_until(
                routes::routes(app_state.clone()),
                shutdown.clone().cancelled_owned(),
            ),
        );
    } else {
        info!("Server Disabled");
    }

    if features.jobs {
        info!("Jobs Enabled");
        info!("Job poll interval: {}ms", job.poll_interval_ms);
        info!("Job heartbeat interval: {}s", job.heartbeat_interval_secs);
        info!("Job reclaim window: {}s", job.reclaim_window_secs);
        info!("Job max retries: {}", job.max_retries);
        info!("Job workers: {}", job.workers);
        info!("Job shutdown drain: {}s", job.shutdown_drain_secs);

        for i in scheduled_tasks(features, job.workers)
            .into_iter()
            .filter_map(|task| {
                if let ProcessTask::Jobs(i) = task {
                    Some(i)
                } else {
                    None
                }
            })
        {
            let name: &'static str = Box::leak(format!("jobs-{i}").into_boxed_str());
            supervisor.spawn(
                name,
                cja::jobs::worker::job_worker(
                    app_state.clone(),
                    jobs::Jobs,
                    Duration::from_millis(job.poll_interval_ms),
                    job.max_retries,
                    shutdown.clone(),
                    job.worker_config(supervisor.budget().job_drain),
                ),
            );
        }
    } else {
        info!("Jobs Disabled");
    }

    if scheduled_tasks(features, job.workers).contains(&ProcessTask::Cron) {
        info!("Cron Enabled");
        supervisor.spawn(
            "cron",
            cron::run_cron(app_state.clone(), cron_registry, shutdown.clone()),
        );
    } else {
        info!("Cron Disabled");
    }

    info!("All application tasks spawned successfully");
    // Workers are already serving while the bounded registration runs.
    Ok(observability::start(&app_state.config, &manifest).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_roles_schedule_only_their_components() {
        let flags = |server, jobs, cron| config::FeatureFlags { server, jobs, cron };
        assert_eq!(
            scheduled_tasks(flags(true, true, true), 2),
            vec![
                ProcessTask::Server,
                ProcessTask::Jobs(0),
                ProcessTask::Jobs(1),
                ProcessTask::Cron,
            ]
        );
        assert_eq!(
            scheduled_tasks(flags(true, false, false), 2),
            vec![ProcessTask::Server]
        );
        assert_eq!(
            scheduled_tasks(flags(false, true, false), 2),
            vec![ProcessTask::Jobs(0), ProcessTask::Jobs(1)]
        );
        assert_eq!(
            scheduled_tasks(flags(false, false, true), 2),
            vec![ProcessTask::Cron]
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
