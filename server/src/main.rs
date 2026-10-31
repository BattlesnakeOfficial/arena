#![allow(dead_code)]

use std::time::Duration;

use cja::{
    server::run_server_until,
    setup::TracingConfig,
    tasks::{ShutdownBudget, Supervisor},
};
use color_eyre::eyre::{Context as _, eyre};
use state::AppState;
use tracing::info;

mod activity;
mod archive;
mod backup;
mod cache;
mod config;
mod countries;
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
mod native_backup;
mod notifications;
mod object_store;
mod observability;
mod og;
mod placement;
mod play_import;
mod routes;
mod scoring;
mod secrets;
mod snake_client;
#[cfg(test)]
mod snake_client_contract_tests;
mod snake_health;
mod snake_health_sweeper;
mod state;
cja::include_assets!(arena_assets);
mod stuck_game_sweeper;
mod telemetry;
mod tournament_bracket;
mod tournament_match;
mod watched_games;
mod wire;

fn job_worker_future<R>(
    app_state: AppState,
    registry: R,
    shutdown: tokio_util::sync::CancellationToken,
    job_drain: Duration,
) -> impl std::future::Future<Output = cja::Result<()>>
where
    R: cja::jobs::registry::JobRegistry<AppState>,
{
    let job = &app_state.config.job;
    cja::jobs::worker::job_worker(
        app_state.clone(),
        registry,
        Duration::from_millis(job.poll_interval_ms),
        job.max_retries,
        shutdown,
        job.worker_config(job_drain, app_state.idle_poll_gate.clone()),
    )
}

/// Frontend UI components only - do not place backend logic here
mod components {
    pub mod avatar;
    pub mod flag;
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

    // One-shot subcommand: copy play's DB into the migration staging
    // tables, then exit (no server, no job workers).
    if std::env::args().nth(1).as_deref() == Some("import-play") {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async { play_import::run_import().await });
    }

    if std::env::args().nth(1).as_deref() == Some("convert-play-points") {
        let trailing: Vec<String> = std::env::args().skip(2).collect();
        let dry_run = match trailing.as_slice() {
            [] => false,
            [flag] if flag == "--dry-run" => true,
            _ => {
                return Err(color_eyre::eyre::eyre!(
                    "usage: arena convert-play-points [--dry-run]"
                ));
            }
        };
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .wrap_err("Failed to build point conversion runtime")?
            .block_on(play_import::run_point_conversion(dry_run))
            .wrap_err("Play point conversion failed");
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

    // Secrets load BEFORE telemetry init: the Eyes layer reads its token at
    // build time, and the token lives in setec (DEV-1966).
    let secrets = load_secrets_before_telemetry(&config).await?;
    let eyes_token = secrets::resolve_eyes_token(
        secrets.as_ref().and_then(|(s, _)| s.eyes_token.as_ref()),
        config.eyes.as_ref().and_then(|eyes| eyes.token.clone()),
    );

    let eyes_shutdown_handle = if config.gcp_logging {
        telemetry::setup_gcp_tracing(
            &config.rust_log,
            config.stdout_log.as_deref(),
            config.eyes.as_ref(),
            &identity,
            eyes_token.as_deref(),
        )?
    } else {
        TracingConfig::new("arena")
            .process(identity.clone())
            .init()?
    };
    let result = run_instrumented_application(config, identity, secrets, eyes_token).await;
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

/// `secrets::load_from_env` under a temporary stdout subscriber, so boot logs
/// emitted before telemetry init ("Joined tailnet", "Arena secrets loaded")
/// still reach stdout / Cloud Logging. Logs from tasks spawned inside the
/// boot window (tailscale-rs/setec internals) go to the no-op global
/// dispatcher until the real subscriber is installed. Fail-fast unchanged: on
/// error the full chain goes to stderr and the process exits non-zero.
async fn load_secrets_before_telemetry(
    config: &config::AppConfig,
) -> cja::Result<Option<(secrets::AppSecrets, cja_secret::Refresher)>> {
    use tracing::instrument::WithSubscriber as _;

    // `.with_ansi(false)`: the `ansi` feature is a default feature of
    // tracing-subscriber 0.3, so the builder colourizes by default — Cloud
    // Logging would ingest the escape codes verbatim as textPayload. The
    // real stdout layer makes the same choice (telemetry.rs).
    let boot_subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&config.rust_log))
        .with_ansi(false)
        .finish();

    match secrets::load_from_env()
        .with_subscriber(boot_subscriber)
        .await
    {
        Ok(secrets) => Ok(secrets),
        Err(error) => {
            eprintln!("Failed to load arena secrets from setec: {error:#}");
            Err(error)
        }
    }
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
    secrets: Option<(secrets::AppSecrets, cja_secret::Refresher)>,
    eyes_token: Option<String>,
) -> cja::Result<()> {
    let app_state = AppState::from_config(config, secrets.as_ref().map(|(s, _)| s)).await?;
    let mut supervisor = Supervisor::new(ShutdownBudget {
        job_drain: Duration::from_secs(app_state.config.job.shutdown_drain_secs),
        exit_grace: SHUTDOWN_EXIT_GRACE,
    })?;
    if let Some((_, refresher)) = secrets {
        supervisor.spawn("secret-refresh", refresher.run(supervisor.shutdown_token()));
    }
    let heartbeat =
        spawn_application_tasks(app_state, identity, &mut supervisor, eyes_token.as_deref())
            .await?;
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
    eyes_token: Option<&str>,
) -> cja::Result<Option<eyes_subscriber::ProcessHeartbeatHandle>> {
    let shutdown = supervisor.shutdown_token();
    let features = app_state.config.features;
    let job = &app_state.config.job;

    // Build the registry once; the manifest and worker both use it when
    // this process has CRON enabled.
    let cron_registry = cron::cron_registry(&app_state.config);

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
                routes::routes(app_state.clone())?,
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
        info!("Job idle pollers: {}", job.idle_pollers);
        info!(
            "Estimated idle claims: {:.1}/s",
            job.idle_pollers as f64 * 1_000.0 / job.poll_interval_ms.max(1) as f64
        );
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
                job_worker_future(
                    app_state.clone(),
                    jobs::Jobs,
                    shutdown.clone(),
                    supervisor.budget().job_drain,
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
    Ok(observability::start(&app_state.config, &manifest, eyes_token).await)
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
        let registry = cron::cron_registry(&config::AppConfig::test_default());
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
