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

cja::impl_job_registry!(
    AppState,
    NoopJob,
    GameRunnerJob,
    GameBackupJob,
    BackupSingleGameJob,
    HistoricalBackupDiscoveryJob,
    LeaderboardMatchmakerJob,
    LeaderboardRatingUpdateJob
);
