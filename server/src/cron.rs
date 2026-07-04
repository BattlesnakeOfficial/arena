use std::time::Duration;

use cja::cron::{CronRegistry, Worker};
use tokio_util::sync::CancellationToken;

use crate::jobs::{
    GameBackupJob, LeaderboardMatchmakerJob, SnakeHealthSweeperJob, StuckMatchSweeperJob,
};
use crate::state::AppState;

/// Matchmaker cron interval in seconds. Shared with the matchmaker to compute games_per_run.
pub const MATCHMAKER_INTERVAL_SECS: u64 = 15 * 60;

/// Snake health sweep interval. With the default failure threshold of 3,
/// a broken snake is pulled from matchmaking after ~90 minutes.
pub const SNAKE_HEALTH_SWEEP_INTERVAL_SECS: u64 = 30 * 60;

fn cron_registry() -> CronRegistry<AppState> {
    let mut registry = CronRegistry::new();

    // Game backup discovery: runs every hour, enqueues backup jobs for games from the last 4 hours
    registry.register_job(
        GameBackupJob,
        Some("Enqueue backup jobs for games from the last 4 hours"),
        Duration::from_secs(60 * 60),
    );

    // Leaderboard matchmaker: runs every 15 minutes, creates match games
    registry.register_job(
        LeaderboardMatchmakerJob,
        Some("Create leaderboard match games"),
        Duration::from_secs(MATCHMAKER_INTERVAL_SECS),
    );

    // Stuck-match sweeper: runs every 2 minutes, re-enqueues evaluation for
    // in-progress tournament matches whose driving jobs died
    registry.register_job(
        StuckMatchSweeperJob,
        Some("Re-enqueue evaluation for stuck tournament matches"),
        Duration::from_secs(2 * 60),
    );

    // Snake health sweeper: probes leaderboard snakes and pulls ones that
    // keep failing, emailing the owner (BS-3534)
    registry.register_job(
        SnakeHealthSweeperJob,
        Some("Health-check leaderboard snakes and deactivate broken ones"),
        Duration::from_secs(SNAKE_HEALTH_SWEEP_INTERVAL_SECS),
    );

    registry
}

pub(crate) async fn run_cron(app_state: AppState) -> cja::Result<()> {
    Ok(Worker::new(app_state, cron_registry())
        .run(CancellationToken::new())
        .await?)
}
