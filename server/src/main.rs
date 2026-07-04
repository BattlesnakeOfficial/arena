#![allow(dead_code)]

use cja::{
    server::run_server,
    setup::{setup_sentry, setup_tracing},
};
use color_eyre::eyre::eyre;
use state::AppState;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod backup;
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
mod game_runner;
mod github;
mod jobs;
mod leaderboard_matchmaker;
mod leaderboard_ratings;
mod models;
mod play_import;
mod routes;
mod scoring;
mod snake_client;
mod snake_health;
mod state;
mod static_assets;
mod telemetry;
mod tournament_bracket;
mod tournament_match;
mod wire;

/// Frontend UI components only - do not place backend logic here
mod components {
    pub mod flash;
    pub mod page;
    pub mod page_factory;
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
    // Initialize tracing (returns Eyes shutdown handle if configured)
    let eyes_shutdown_handle = if config.gcp_logging {
        telemetry::setup_gcp_tracing(&config.rust_log)?
    } else {
        setup_tracing("arent")?
    };

    let app_state = AppState::from_config(config).await?;

    // Spawn application tasks
    info!("Spawning application tasks");
    let tasks = spawn_application_tasks(app_state).await?;

    // Wait for any task to complete - they all run forever, so if one exits it's an error
    if !tasks.is_empty() {
        let (name, result) = wait_for_first_task(tasks).await;

        match result {
            Ok(Ok(())) => {
                tracing::error!(task = name, "Task exited unexpectedly");
                return Err(eyre!("Task '{}' exited unexpectedly", name));
            }
            Ok(Err(e)) => {
                tracing::error!(task = name, error = ?e, "Task failed with error");
                return Err(e);
            }
            Err(join_error) => {
                tracing::error!(task = name, error = ?join_error, "Task panicked");
                return Err(eyre!("Task '{}' panicked: {}", name, join_error));
            }
        }
    }

    // Graceful shutdown of Eyes tracing if configured
    if let Some(handle) = eyes_shutdown_handle {
        info!("Shutting down Eyes tracing...");
        if let Err(e) = handle.shutdown().await {
            tracing::warn!("Error shutting down Eyes: {e}");
        }
    }

    Ok(())
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

    let (result, index, _remaining) = futures::future::select_all(handles).await;
    (names[index], result)
}

/// Spawn all application background tasks
async fn spawn_application_tasks(app_state: AppState) -> cja::Result<Vec<NamedTask>> {
    let mut tasks = vec![];
    let features = app_state.config.features;
    let job = &app_state.config.job;

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
        tasks.push(NamedTask::spawn("cron", cron::run_cron(app_state.clone())));
    } else {
        info!("Cron Disabled");
    }

    info!("All application tasks spawned successfully");
    Ok(tasks)
}
