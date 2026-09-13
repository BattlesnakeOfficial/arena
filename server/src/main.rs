#![allow(dead_code)]

use cja::{
    server::run_server,
    setup::{TracingConfig, setup_sentry},
};
use color_eyre::eyre::eyre;
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

async fn run_instrumented_application(
    config: config::AppConfig,
    identity: eyes_subscriber::ProcessIdentity,
) -> cja::Result<()> {
    let app_state = AppState::from_config(config).await?;
    let (tasks, heartbeat) = spawn_application_tasks(app_state, identity).await?;
    let result = if tasks.is_empty() {
        Ok(())
    } else {
        let (name, result) = wait_for_first_task(tasks).await;
        match result {
            Ok(Ok(())) => Err(eyre!("Task '{}' exited unexpectedly", name)),
            Ok(Err(error)) => Err(error.wrap_err(format!("Task '{name}' failed"))),
            Err(error) => Err(eyre!("Task '{}' panicked: {}", name, error)),
        }
    };
    if let Some(handle) = heartbeat
        && let Err(error) = handle.shutdown().await
    {
        tracing::warn!(%error, "Eyes process shutdown signal failed");
    }
    result
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

/// Wait for the first task to complete and return its name and result
async fn wait_for_first_task(
    tasks: Vec<NamedTask>,
) -> (
    &'static str,
    Result<cja::Result<()>, tokio::task::JoinError>,
) {
    let (handles, names): (Vec<_>, Vec<_>) = tasks.into_iter().map(|t| (t.handle, t.name)).unzip();

    let (result, index, remaining) = futures::future::select_all(handles).await;
    for handle in &remaining {
        handle.abort();
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures::future::join_all(remaining),
    )
    .await;
    (names[index], result)
}

/// Spawn all application background tasks
async fn spawn_application_tasks(
    app_state: AppState,
    identity: eyes_subscriber::ProcessIdentity,
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
            run_server(routes::routes(app_state.clone())),
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

        for i in 0..job.workers {
            let name: &'static str = Box::leak(format!("jobs-{i}").into_boxed_str());
            tasks.push(NamedTask::spawn(
                name,
                cja::jobs::worker::job_worker(
                    app_state.clone(),
                    jobs::Jobs,
                    std::time::Duration::from_millis(job.poll_interval_ms),
                    job.max_retries,
                    CancellationToken::new(),
                    std::time::Duration::from_secs(job.lock_timeout_secs),
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
            cron::run_cron(app_state.clone(), cron_registry),
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
