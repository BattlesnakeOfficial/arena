//! `engine proxy_server` — the Battlesnake regional engine proxy (DEV-1943).
//!
//! Boot shape (DD 1): tracing inside the runtime (the Eyes layer spawns its
//! exporter during `init()`), `Supervisor` created before state so the
//! shutdown token can label `shutdown_aborted` requests, then cja's
//! `run_server_until` for SIGTERM-aware graceful drain. No signal handling,
//! no subscriber, no `axum::serve` of our own — cja owns all of it.
//!
//! The binary is named `engine` (not `engine-proxy`) with a `proxy_server`
//! subcommand so the infrastructure bootstrap line `docker run ... --entrypoint
//! engine <image> proxy_server` runs this image unchanged.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "engine", about = "Battlesnake engine services", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the battlesnake regional proxy server.
    ProxyServer,
}

fn main() -> cja::Result<()> {
    color_eyre::install()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())
}

async fn run() -> cja::Result<()> {
    let _cli = Cli::parse();
    // MUST be inside the runtime: the Eyes layer tokio::spawns its exporter
    // during init() and panics with "there is no reactor running" if called
    // before block_on. "engine_proxy" with an UNDERSCORE: cja's default
    // RUST_LOG embeds "{crate_name}=trace", and a hyphenated name can never
    // match a tracing target (targets are module paths).
    let _eyes = cja::setup::TracingConfig::new("engine_proxy").init()?;
    let mut supervisor = cja::tasks::Supervisor::new(cja::tasks::ShutdownBudget::from_env())?;
    let shutdown = supervisor.shutdown_token();
    // Missing BATTLESNAKE_PROXY_AUTH_TOKEN => Err => main exits 1, like Go.
    let state = engine_proxy::app::ProxyState::from_env(shutdown.clone())?;
    let router = engine_proxy::app::router(state);
    supervisor.spawn(
        "server",
        cja::server::run_server_until(router, shutdown.cancelled_owned()),
    );
    supervisor.run().await
}
