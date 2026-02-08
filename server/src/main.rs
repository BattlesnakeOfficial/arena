#![allow(dead_code)]

use cja::{
    jobs::worker::{DEFAULT_LOCK_TIMEOUT, DEFAULT_MAX_RETRIES},
    server::run_server,
    setup::{setup_sentry, setup_tracing},
};
use color_eyre::eyre::eyre;
use state::AppState;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod backup;
mod cron;
mod engine;
mod engine_models;
mod errors;
mod flasher;
mod game_channels;
mod game_runner;
mod github;
mod jobs;
mod models;
mod routes;
mod snake_client;
mod state;
mod static_assets;
mod telemetry;

/// Frontend UI components only - do not place backend logic here
mod components {
    pub mod flash;
    pub mod page;
    pub mod page_factory;
}

fn main() -> color_eyre::Result<()> {
    // Initialize Sentry for error tracking
    let _sentry_guard = setup_sentry();

    // Configure tokio worker threads as a multiplier on CPU core count.
    // Since game execution is I/O-bound (snake API calls ~500ms each),
    // we want many more threads than cores to maximize throughput.
    let core_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let multiplier: usize = std::env::var("ARENA_TOKIO_WORKER_MULTIPLIER")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let worker_threads = core_count * multiplier;
    eprintln!("Tokio workers: {worker_threads} ({core_count} cores x {multiplier} multiplier)");

    // Create and run the tokio runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()?
        .block_on(async { run_application().await })
}

async fn run_application() -> cja::Result<()> {
    // Initialize tracing (returns Eyes shutdown handle if configured)
    let eyes_shutdown_handle = if std::env::var("GCP_LOGGING").is_ok() {
        telemetry::setup_gcp_tracing()?
    } else {
        setup_tracing("arent")?
    };

    let app_state = AppState::from_env().await?;

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

    if is_feature_enabled("SERVER") {
        info!("Server Enabled");
        tasks.push(NamedTask::spawn(
            "server",
            run_server(routes::routes(app_state.clone())),
        ));
    } else {
        info!("Server Disabled");
    }

    if is_feature_enabled("JOBS") {
        info!("Jobs Enabled");

        // Job poll interval in milliseconds (default: 60000ms = 60 seconds)
        let job_poll_interval_ms: u64 = std::env::var("ARENA_JOB_POLL_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000);
        info!("Job poll interval: {}ms", job_poll_interval_ms);

        // Job lock timeout in seconds (default: 2 hours)
        let job_lock_timeout_secs: u64 = std::env::var("ARENA_JOB_LOCK_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_LOCK_TIMEOUT.as_secs());
        info!("Job lock timeout: {}s", job_lock_timeout_secs);

        // Max retries before job is deleted (default: 20)
        let job_max_retries: i32 = std::env::var("ARENA_JOB_MAX_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_RETRIES);
        info!("Job max retries: {}", job_max_retries);

        // Number of concurrent job workers (default: 1)
        let job_workers: usize = std::env::var("ARENA_JOB_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .max(1);
        info!("Job workers: {}", job_workers);

        for i in 0..job_workers {
            let name: &'static str = Box::leak(format!("jobs-{i}").into_boxed_str());
            tasks.push(NamedTask::spawn(
                name,
                cja::jobs::worker::job_worker(
                    app_state.clone(),
                    jobs::Jobs,
                    std::time::Duration::from_millis(job_poll_interval_ms),
                    job_max_retries,
                    CancellationToken::new(),
                    std::time::Duration::from_secs(job_lock_timeout_secs),
                ),
            ));
        }
    } else {
        info!("Jobs Disabled");
    }

    if is_feature_enabled("CRON") {
        info!("Cron Enabled");
        tasks.push(NamedTask::spawn("cron", cron::run_cron(app_state.clone())));
    } else {
        info!("Cron Disabled");
    }

    info!("All application tasks spawned successfully");
    Ok(tasks)
}

/// Check if a feature is enabled based on environment variables
fn is_feature_enabled(feature: &str) -> bool {
    let env_var_name = format!("{}_DISABLED", feature);
    let value = std::env::var(&env_var_name).unwrap_or_else(|_| "false".to_string());

    value != "true"
}
