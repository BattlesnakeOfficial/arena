use crate::state::AppState;

use cja::jobs::Job;
use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::notifications::{NotificationChannel, NotificationKind};
use crate::play_shutdown_notices::PlayShutdownNoticeJob;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SendNotificationEmailJob {
    pub notification_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for SendNotificationEmailJob {
    const NAME: &'static str = "SendNotificationEmailJob";

    async fn run(&self, state: AppState) -> cja::Result<()> {
        use crate::email::{
            SendOutcome,
            messages::{self, EmailContent},
        };
        use crate::models::{
            claim_email_token, leaderboard_entry_health, notification, notification_email,
        };

        let _permit = state.mailer.acquire_delivery_permit().await?;
        let mut tx = state.db.begin().await?;
        let Some(email) = sqlx::query!(
            "SELECT subject, text, html_body, recipient_override, battlesnake_id, recipient_user, claim_imported_account_id, notification_email_request_id, outcome FROM notification_emails WHERE notification_id = $1 FOR UPDATE",
            self.notification_id
        ).fetch_optional(&mut *tx).await? else { return Ok(()); };
        if email.outcome.is_some() {
            return Ok(());
        }
        let event = sqlx::query!(
            "SELECT user_id, kind FROM notifications WHERE notification_id = $1",
            self.notification_id
        )
        .fetch_one(&mut *tx)
        .await?;
        let kind = NotificationKind::parse(&event.kind);
        let mut terminal = None;
        if kind.is_none() {
            tracing::warn!(notification_id = %self.notification_id, "Unknown notification kind");
            terminal = Some("no_recipient");
        }
        if let Some(kind) = kind
            && !notification::effective_enabled(
                &mut tx,
                event.user_id,
                kind,
                NotificationChannel::Email,
            )
            .await?
        {
            terminal = Some("opted_out");
        }
        if !state.mailer.is_enabled() {
            terminal.get_or_insert("transport_disabled");
        }
        let mut recipient = None;
        let mut confirmation_request = None;
        if terminal.is_none()
            && let Some(kind) = kind
        {
            if kind.toggleable() {
                recipient = if email.recipient_user {
                    notification_email::resolved_recipient(&mut tx, event.user_id).await?
                } else if let Some(snake_id) = email.battlesnake_id {
                    leaderboard_entry_health::owner_notification_email(&mut tx, snake_id).await?
                } else {
                    None // Legacy override-only toggleable rows cannot bypass confirmation.
                };
            } else if kind == NotificationKind::NotificationEmailConfirmation {
                if let Some(request_id) = email.notification_email_request_id {
                    // Hold the user lock through Mailgun so only the current request can send.
                    let pending_id = sqlx::query_scalar!("SELECT pending_notification_email_request_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE", event.user_id).fetch_optional(&mut *tx).await?.flatten();
                    if pending_id == Some(request_id) {
                        let request = sqlx::query!("SELECT email, sent_at FROM notification_email_requests WHERE request_id = $1 AND user_id = $2 FOR UPDATE", request_id, event.user_id).fetch_optional(&mut *tx).await?;
                        if let Some(request) = request.filter(|request| {
                            request.sent_at.is_none()
                                && Some(&request.email) == email.recipient_override.as_ref()
                        }) {
                            let sent_count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1 AND sent_at > NOW() - INTERVAL '24 hours'", event.user_id).fetch_one(&mut *tx).await?;
                            if sent_count >= 5 {
                                terminal = Some("rate_limited");
                            } else {
                                recipient = Some(request.email);
                                confirmation_request = Some(request_id);
                            }
                        }
                    }
                }
            } else {
                recipient = email.recipient_override;
            }
        }
        if terminal.is_none()
            && recipient
                .as_ref()
                .is_none_or(|value| value.trim().is_empty())
        {
            terminal = Some("no_recipient");
        }
        if terminal.is_none() {
            let to = recipient.as_deref().unwrap_or_default();
            let content = if let Some(request_id) = confirmation_request {
                let secret = notification_email::new_secret();
                sqlx::query!("UPDATE notification_email_requests SET token_hash = $2, expires_at = NOW() + INTERVAL '24 hours' WHERE request_id = $1", request_id, notification_email::hash_secret(&secret)).execute(&mut *tx).await?;
                let url = format!(
                    "{}/settings/notification-email/confirm?token={secret}",
                    state.config.base_url.trim_end_matches('/')
                );
                Some(messages::notification_email_confirmation(&url))
            } else if let Some(imported_account_id) = email.claim_imported_account_id {
                let account = sqlx::query!("SELECT username FROM imported_accounts WHERE imported_account_id = $1 AND claimed_by_user_id IS NULL", imported_account_id).fetch_optional(&mut *tx).await?;
                if let Some(account) = account {
                    let token =
                        claim_email_token::create(&mut tx, imported_account_id, event.user_id)
                            .await?;
                    let url = format!("{}/claim/email/verify?token={token}", state.config.base_url);
                    Some(messages::claim_verification(&account.username, &url))
                } else {
                    terminal = Some("no_recipient");
                    None
                }
            } else {
                Some(match email.html_body {
                    Some(html_body) => EmailContent {
                        subject: email.subject.unwrap_or_default(),
                        text: email.text.unwrap_or_default(),
                        html_body,
                    },
                    None => EmailContent::from_text(
                        email.subject.unwrap_or_default(),
                        email.text.unwrap_or_default(),
                    ),
                })
            };
            if terminal.is_none()
                && let (Some(content), Some(kind)) = (content, kind)
            {
                let unsubscribe_url = if kind.toggleable() {
                    let token = crate::notifications::unsubscribe_token(
                        &state.cookie_key,
                        event.user_id,
                        kind,
                    );
                    Some(format!(
                        "{}/notifications/unsubscribe/{token}",
                        state.config.base_url
                    ))
                } else {
                    None
                };
                let message = messages::finish(
                    content,
                    to.to_string(),
                    kind,
                    &state.config.base_url,
                    unsubscribe_url.as_deref(),
                )?;
                let outcome = state
                    .mailer
                    .send_limited(
                        &mut tx,
                        state.config.email_per_recipient_hourly_limit,
                        kind.key(),
                        &message,
                    )
                    .await?;
                if let Some(request_id) = confirmation_request {
                    match outcome {
                        SendOutcome::Sent => {
                            sqlx::query!("UPDATE notification_email_requests SET sent_at = NOW() WHERE request_id = $1", request_id).execute(&mut *tx).await?;
                        }
                        SendOutcome::RateLimited | SendOutcome::Disabled => {
                            sqlx::query!("UPDATE notification_email_requests SET token_hash = NULL, expires_at = NULL WHERE request_id = $1", request_id).execute(&mut *tx).await?;
                        }
                    }
                }
                terminal = Some(match outcome {
                    SendOutcome::Sent => "sent",
                    SendOutcome::RateLimited => "rate_limited",
                    SendOutcome::Disabled => "transport_disabled",
                });
            }
        }
        sqlx::query!("UPDATE notification_emails SET outcome = $2, finished_at = NOW() WHERE notification_id = $1", self.notification_id, terminal).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

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
pub struct PlayGrantReconcileJob;

#[async_trait::async_trait]
impl Job<AppState> for PlayGrantReconcileJob {
    const NAME: &'static str = "PlayGrantReconcileJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        match crate::play_import::run_grant_reconcile(
            &app_state.config.database_url,
            "play",
            &app_state.db,
        )
        .await
        {
            Ok(None) => {
                tracing::info!("Play grant reconcile skipped: play database does not exist");
                Ok(())
            }
            Ok(Some(counts)) => {
                tracing::info!(
                    play_grants_read = counts.play_grants_read,
                    newly_staged = counts.newly_staged,
                    newly_materialized = counts.newly_materialized,
                    refunded_token_grants = counts.refunded_token_grants,
                    skipped_off_catalog = counts.skipped_off_catalog,
                    newly_staged_accounts = counts.newly_staged_accounts,
                    skipped_identity_conflict = counts.skipped_identity_conflict,
                    failed_account_staging = counts.failed_account_staging,
                    "Play grant reconcile completed"
                );
                Ok(())
            }
            Err(error) => {
                tracing::error!("Play grant reconcile failed: {error:#}");
                Err(error)
            }
        }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomizationActiveWeekBackfillJob;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardAchievementsForGameJob {
    pub game_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for AwardAchievementsForGameJob {
    const NAME: &'static str = "AwardAchievementsForGameJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::customizations::achievements::award_for_game(
            &app_state.db,
            self.game_id,
            &app_state.config.base_url,
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackfillAchievementsJob;

#[async_trait::async_trait]
impl Job<AppState> for BackfillAchievementsJob {
    const NAME: &'static str = "BackfillAchievementsJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(30) {
            let page =
                crate::customizations::achievements::backfill_achievement_page(&app_state.db)
                    .await?;
            tracing::info!(
                inserted = page.inserted,
                processed = page.processed,
                complete = page.complete,
                "Achievement backfill page"
            );
            if page.complete || page.processed == 0 {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconcileRecentAchievementsJob;

#[async_trait::async_trait]
impl Job<AppState> for ReconcileRecentAchievementsJob {
    const NAME: &'static str = "ReconcileRecentAchievementsJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::customizations::achievements::reconcile_recent_achievements(&app_state.db).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Job<AppState> for CustomizationActiveWeekBackfillJob {
    const NAME: &'static str = "CustomizationActiveWeekBackfillJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        let inserted = crate::customizations::backfill_active_weeks(&app_state.db).await?;
        let cursor = sqlx::query!(
            "SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE"
        )
        .fetch_one(&app_state.db)
        .await
        .wrap_err("Failed to read active-week backfill cursor")?
        .scanned_through;
        tracing::info!(inserted, %cursor, "Customization active-week backfill completed");
        Ok(())
    }
}

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

/// Cron job: hourly discovery of arena-native games needing GCS archive —
/// fresh terminal flips (recent tier) plus the unarchived backlog, oldest
/// first. See [`crate::native_backup`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NativeGameBackupJob;

#[async_trait::async_trait]
impl Job<AppState> for NativeGameBackupJob {
    const NAME: &'static str = "NativeGameBackupJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::native_backup::run_native_backup_discovery(&app_state).await?;
        Ok(())
    }
}

/// Archive one arena-native game to GCS. Enqueued by NativeGameBackupJob;
/// idempotent (re-checks archived_at under the backup slot before any work).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NativeBackupSingleGameJob {
    pub game_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for NativeBackupSingleGameJob {
    const NAME: &'static str = "NativeBackupSingleGameJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::native_backup::archive_native_game(&app_state, self.game_id).await?;
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
/// Runs once per nominal matchmaker interval, creating one round for each active ladder.
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

/// Cron job that health-checks leaderboard entries whose games show
/// timeouts or errors and pulls the ones that keep failing (BS-3534,
/// DEV-1515). All bookkeeping is idempotent and the owner emails are gated by
/// the pause/resume transitions, so retries and duplicate enqueues are safe;
/// see [`crate::snake_health_sweeper`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SnakeHealthSweeperJob;

#[async_trait::async_trait]
impl Job<AppState> for SnakeHealthSweeperJob {
    const NAME: &'static str = "SnakeHealthSweeperJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::snake_health_sweeper::run_sweep(&app_state).await?;
        Ok(())
    }
}

/// Cron job that fails non-tournament games stuck in `waiting`/`running` past
/// [`crate::config::AppConfig::stuck_game_max_age_hours`]. Idempotent and safe
/// to retry; see [`crate::stuck_game_sweeper`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StuckGameSweeperJob;

#[async_trait::async_trait]
impl Job<AppState> for StuckGameSweeperJob {
    const NAME: &'static str = "StuckGameSweeperJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::stuck_game_sweeper::run_sweep(&app_state).await?;
        Ok(())
    }
}

/// Post-completion job: screen a finished game's distinct shouts with one
/// Jev call and record suppressions (DEV-1297). Fail-open: never retries on
/// Jev errors (the `shout_screenings` marker row makes re-runs no-ops).
/// See [`crate::moderation::shouts`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScreenShoutsJob {
    pub game_id: Uuid,
}

#[async_trait::async_trait]
impl Job<AppState> for ScreenShoutsJob {
    const NAME: &'static str = "ScreenShoutsJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        crate::moderation::shouts::screen_game_shouts(&app_state, self.game_id).await
    }
}

cja::impl_job_registry!(
    AppState,
    NoopJob,
    PlayGrantReconcileJob,
    SendNotificationEmailJob,
    GameRunnerJob,
    GameBackupJob,
    CustomizationActiveWeekBackfillJob,
    AwardAchievementsForGameJob,
    BackfillAchievementsJob,
    ReconcileRecentAchievementsJob,
    BackupSingleGameJob,
    HistoricalBackupDiscoveryJob,
    NativeGameBackupJob,
    NativeBackupSingleGameJob,
    LeaderboardMatchmakerJob,
    LeaderboardRatingUpdateJob,
    RunTournamentRoundJob,
    RunMatchJob,
    UpdateTournamentStatusJob,
    StuckMatchSweeperJob,
    RateLimitPruneJob,
    SnakeHealthSweeperJob,
    StuckGameSweeperJob,
    ScreenShoutsJob,
    PlayShutdownNoticeJob,
    crate::highlights::WeeklyHighlightsJob
);

#[cfg(test)]
mod tests;
