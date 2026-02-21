use std::time::Duration;

use cja::cron::{CronRegistry, Worker};
use tokio_util::sync::CancellationToken;

use crate::jobs::{GameBackupJob, LeaderboardMatchmakerJob};
use crate::state::AppState;

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
        Duration::from_secs(15 * 60),
    );

    registry
}

pub(crate) async fn run_cron(app_state: AppState) -> cja::Result<()> {
    Ok(Worker::new(app_state, cron_registry())
        .run(CancellationToken::new())
        .await?)
}
