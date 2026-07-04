use crate::state::AppState;

use cja::jobs::Job;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NoopJob;

#[async_trait::async_trait]
impl Job<AppState> for NoopJob {
    const NAME: &'static str = "NoopJob";

    async fn run(&self, _app_state: AppState) -> cja::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GameRunnerJob {
    pub game_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for GameRunnerJob {
    const NAME: &'static str = "GameRunnerJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        // Run the game with HTTP calls to snake APIs, turn-by-turn persistence, and WebSocket notifications
        crate::game_runner::run_game(&app_state, self.game_id).await?;
        Ok(())
    }
}

/// Job to discover games that need backup and enqueue individual backup jobs.
/// Runs as a cron job every hour, checking games from the last 4 hours.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GameBackupJob;

#[async_trait::async_trait]
impl Job<AppState> for GameBackupJob {
    const NAME: &'static str = "GameBackupJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::backup::run_backup_discovery(&app_state).await?;
        Ok(())
    }
}

/// Job to backup a single game from the Engine database to GCS.
/// Enqueued by GameBackupJob for each game that needs archiving.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupSingleGameJob {
    pub engine_game_id: String,
    /// Optional batch ID for historical backfill tracking.
    /// When set, completing this job will increment the batch's completed count.
    #[serde(default)]
    pub batch_id: Option<i32>,
}

#[async_trait::async_trait]
impl Job<AppState> for BackupSingleGameJob {
    const NAME: &'static str = "BackupSingleGameJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::backup::backup_single_game(&app_state, &self.engine_game_id, self.batch_id).await?;
        Ok(())
    }
}

/// Job to discover historical games and enqueue backup jobs in batches.
/// Uses fork-join pattern: enqueues a batch, waits for completion, then enqueues next batch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HistoricalBackupDiscoveryJob {
    /// Cursor: only process games created after this timestamp
    pub after_created: Option<chrono::NaiveDateTime>,
    /// Cursor: for tie-breaking when created timestamps match
    pub after_id: Option<String>,
}

#[async_trait::async_trait]
impl Job<AppState> for HistoricalBackupDiscoveryJob {
    const NAME: &'static str = "HistoricalBackupDiscoveryJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::backup::run_historical_backup_discovery(
            &app_state,
            self.after_created,
            self.after_id.as_deref(),
        )
        .await?;
        Ok(())
    }
}

/// Cron job to create leaderboard match games.
/// Runs every 15 minutes, creating games for active leaderboards.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LeaderboardMatchmakerJob;

#[async_trait::async_trait]
impl Job<AppState> for LeaderboardMatchmakerJob {
    const NAME: &'static str = "LeaderboardMatchmakerJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::leaderboard_matchmaker::run_matchmaker(&app_state).await?;
        Ok(())
    }
}

/// Job to update ratings after a leaderboard game completes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LeaderboardRatingUpdateJob {
    pub leaderboard_game_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for LeaderboardRatingUpdateJob {
    const NAME: &'static str = "LeaderboardRatingUpdateJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::leaderboard_ratings::update_ratings(&app_state, self.leaderboard_game_id).await?;
        Ok(())
    }
}

/// Job to kick off every ready match in a tournament's current round.
/// Enqueued when the owner clicks "Run Round".
///
/// `round` pins the job to the round the owner saw when they clicked: if the
/// tournament has moved on (or was reset and restarted) by the time the job
/// runs, `run_round` no-ops instead of firing matches the owner never asked
/// for. `serde(default)` makes payloads enqueued before this field existed
/// deserialize to round 0, which never matches a live round — a safe no-op.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunTournamentRoundJob {
    pub tournament_id: Uuid,
    #[serde(default)]
    pub round: i32,
}

#[async_trait::async_trait]
impl Job<AppState> for RunTournamentRoundJob {
    const NAME: &'static str = "RunTournamentRoundJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::tournament_match::run_round(&app_state, self.tournament_id, self.round).await?;
        Ok(())
    }
}

/// Job to evaluate a tournament match and take its next step: create the
/// next game, wait on one in flight, or complete the match and advance the
/// winner. Re-enqueued by the game completion hook, so it is re-entrant.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunMatchJob {
    pub match_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for RunMatchJob {
    const NAME: &'static str = "RunMatchJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::tournament_match::run_match(&app_state, self.match_id).await?;
        Ok(())
    }
}

/// Job to advance a tournament's round counter when a round finishes, and
/// mark the tournament completed after the final.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateTournamentStatusJob {
    pub tournament_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for UpdateTournamentStatusJob {
    const NAME: &'static str = "UpdateTournamentStatusJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::tournament_match::update_tournament_progress(&app_state, self.tournament_id).await?;
        Ok(())
    }
}

/// Cron job that re-enqueues evaluation for tournament matches whose driving
/// jobs died (the job system deletes jobs that exhaust their retries, so a
/// match can otherwise get stuck in progress forever). Runs every couple of
/// minutes; see [`crate::tournament_match::sweep_stuck_matches`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StuckMatchSweeperJob;

#[async_trait::async_trait]
impl Job<AppState> for StuckMatchSweeperJob {
    const NAME: &'static str = "StuckMatchSweeperJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::tournament_match::sweep_stuck_matches(&app_state).await?;
        Ok(())
    }
}

/// Cron job that prunes rate-limit bookkeeping (game_creation_attempts,
/// claim_attempts) past its retention window. The limits record every
/// attempt — including rejected ones — so without this the tables grow
/// without bound.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RateLimitPruneJob;

#[async_trait::async_trait]
impl Job<AppState> for RateLimitPruneJob {
    const NAME: &'static str = "RateLimitPruneJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::models::rate_limit::prune_old_attempts(&app_state.db).await?;
        Ok(())
    }
}

cja::impl_job_registry!(
    AppState,
    NoopJob,
    GameRunnerJob,
    GameBackupJob,
    BackupSingleGameJob,
    HistoricalBackupDiscoveryJob,
    LeaderboardMatchmakerJob,
    LeaderboardRatingUpdateJob,
    RunTournamentRoundJob,
    RunMatchJob,
    UpdateTournamentStatusJob,
    StuckMatchSweeperJob,
    RateLimitPruneJob
);
