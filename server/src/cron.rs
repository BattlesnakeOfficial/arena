use std::time::Duration;

use cja::cron::{CronRegistry, Worker};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::jobs::{
    GameBackupJob, LeaderboardMatchmakerJob, PlayGrantReconcileJob, RateLimitPruneJob,
    SnakeHealthSweeperJob, StuckGameSweeperJob, StuckMatchSweeperJob,
};
use crate::state::AppState;

/// Matchmaker cron interval in seconds. Shared with the matchmaker to compute games_per_run.
pub const MATCHMAKER_INTERVAL_SECS: u64 = 15 * 60;

/// Snake health sweep interval. With the default failure threshold of 3,
/// a broken entry is pulled from matchmaking ~90 minutes after its first
/// failed probe.
pub const SNAKE_HEALTH_SWEEP_INTERVAL_SECS: u64 = 30 * 60;

/// Stuck-game sweep interval. Fails non-tournament games left in
/// waiting/running past the configured max age.
pub const STUCK_GAME_SWEEP_INTERVAL_SECS: u64 = 30 * 60;

pub(crate) fn cron_registry(config: &AppConfig) -> CronRegistry<AppState> {
    let mut registry = CronRegistry::new();

    registry.register_job(
        PlayGrantReconcileJob,
        Some("Reconcile play customization grants"),
        Duration::from_secs(config.play_grant_reconcile_interval_secs),
    );

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

    // Rate-limit bookkeeping prune: keeps the attempt tables from growing
    // without bound (every request inserts, including rejected ones)
    registry.register_job(
        RateLimitPruneJob,
        Some("Prune rate-limit attempt rows past retention"),
        Duration::from_secs(6 * 60 * 60),
    );

    // Snake health sweeper: probes leaderboard entries whose games show
    // timeouts or errors and pulls ones that keep failing, emailing the
    // owner (BS-3534, DEV-1515)
    registry.register_job(
        SnakeHealthSweeperJob,
        Some("Health-check failing leaderboard entries and pause broken ones"),
        Duration::from_secs(SNAKE_HEALTH_SWEEP_INTERVAL_SECS),
    );

    // Stuck-game sweeper: every 30 min, fails non-tournament games left in
    // waiting/running past STUCK_GAME_MAX_AGE_HOURS
    registry.register_job(
        StuckGameSweeperJob,
        Some("Fail non-tournament games stuck in waiting/running"),
        Duration::from_secs(STUCK_GAME_SWEEP_INTERVAL_SECS),
    );

    registry
}

pub(crate) async fn run_cron(
    app_state: AppState,
    registry: CronRegistry<AppState>,
    shutdown: CancellationToken,
) -> cja::Result<()> {
    Ok(Worker::new(app_state, registry).run(shutdown).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cja::cron::{IntervalSchedule, Schedule};

    #[test]
    fn play_reconcile_job_is_registered_with_configured_interval() {
        assert!(
            <crate::jobs::Jobs as cja::jobs::registry::JobRegistry<AppState>>::job_names()
                .contains(&"PlayGrantReconcileJob")
        );
        let mut config = AppConfig::test_default();
        for interval in [3600, 15] {
            config.play_grant_reconcile_interval_secs = interval;
            let registry = cron_registry(&config);
            let job = registry
                .get("PlayGrantReconcileJob")
                .expect("cron registered");
            assert!(
                matches!(&job.schedule, Schedule::Interval(IntervalSchedule(duration)) if *duration == Duration::from_secs(interval))
            );
        }
    }
}
