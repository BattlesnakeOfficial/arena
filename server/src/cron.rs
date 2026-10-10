use std::time::Duration;

use cja::cron::{CronRegistry, Worker};
use tokio_util::sync::CancellationToken;

use crate::jobs::{
    CustomizationActiveWeekBackfillJob, GameBackupJob, LeaderboardMatchmakerJob, RateLimitPruneJob,
    SnakeHealthSweeperJob, StuckGameSweeperJob, StuckMatchSweeperJob,
};
use crate::state::AppState;

/// Nominal rounds per snake per ladder each day.
pub const GAMES_PER_SNAKE_PER_DAY: u64 = 100;
/// Nominal interval; worker scheduling and tick work can delay actual rounds.
pub const MATCHMAKER_INTERVAL_SECS: u64 = 86_400 / GAMES_PER_SNAKE_PER_DAY;
/// CJA requires elapsed > interval, so a short poll observes the 864s cadence
/// without rounding each round up to the default 60s poll boundary.
const CRON_POLL_SECS: u64 = 2;
const LADDER_DISPATCH_INTERVAL_SECS: u64 = 5;

/// Snake health sweep interval. With the default failure threshold of 3,
/// a broken entry is pulled from matchmaking ~90 minutes after its first
/// failed probe.
pub const SNAKE_HEALTH_SWEEP_INTERVAL_SECS: u64 = 30 * 60;

/// Stuck-game sweep interval. Fails non-tournament games left in
/// waiting/running past the configured max age.
pub const STUCK_GAME_SWEEP_INTERVAL_SECS: u64 = 30 * 60;
pub const CUSTOMIZATION_ACTIVE_WEEK_BACKFILL_INTERVAL_SECS: u64 = 60 * 60;

pub(crate) fn cron_registry() -> CronRegistry<AppState> {
    let mut registry = CronRegistry::new();

    // Game backup discovery: runs every hour, enqueues backup jobs for games from the last 4 hours
    registry.register_job(
        GameBackupJob,
        Some("Enqueue backup jobs for games from the last 4 hours"),
        Duration::from_secs(60 * 60),
    );
    registry.register_job(
        CustomizationActiveWeekBackfillJob,
        Some("Credit historical finished-game weeks"),
        Duration::from_secs(CUSTOMIZATION_ACTIVE_WEEK_BACKFILL_INTERVAL_SECS),
    );

    // Leaderboard matchmaker: one round per derived interval, subject to worker delay.
    registry.register_job(
        LeaderboardMatchmakerJob,
        Some("Create leaderboard match games"),
        Duration::from_secs(MATCHMAKER_INTERVAL_SECS),
    );
    registry.register(
        "LeaderboardGameDispatch",
        Some("Dispatch eligible ladder games"),
        Duration::from_secs(LADDER_DISPATCH_INTERVAL_SECS),
        |app_state, _| Box::pin(dispatch_ladder_callback(app_state)),
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

async fn dispatch_ladder_callback(
    app_state: crate::state::AppState,
) -> Result<(), std::convert::Infallible> {
    if let Err(error) =
        crate::leaderboard_matchmaker::dispatch_pending_ladder_games(&app_state).await
    {
        tracing::error!(error = %format!("{error:#}"), "Ladder dispatch failed");
    }
    Ok(())
}

pub(crate) async fn run_cron(
    app_state: AppState,
    registry: CronRegistry<AppState>,
    shutdown: CancellationToken,
) -> cja::Result<()> {
    Ok(Worker::new_with_timezone(
        app_state,
        registry,
        cja::chrono_tz::UTC,
        Duration::from_secs(CRON_POLL_SECS),
    )
    .run(shutdown)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::{field::Visit, instrument::WithSubscriber};
    use tracing_subscriber::{Layer, prelude::*};

    #[derive(Clone, Default)]
    struct Errors(Arc<Mutex<Vec<String>>>);

    struct ErrorField(Option<String>);

    impl Visit for ErrorField {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "error" {
                self.0 = Some(format!("{value:?}"));
            }
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "error" {
                self.0 = Some(value.to_owned());
            }
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Errors {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut field = ErrorField(None);
            event.record(&mut field);
            if let Some(error) = field.0 {
                self.0.lock().unwrap().push(error);
            }
        }
    }

    #[test]
    fn matchmaker_cadence() {
        assert_eq!(MATCHMAKER_INTERVAL_SECS, 864);
        assert_eq!(LADDER_DISPATCH_INTERVAL_SECS, 5);
        const { assert!(CRON_POLL_SECS < MATCHMAKER_INTERVAL_SECS) };
    }

    #[test]
    fn active_week_backfill_runs_on_boot_then_hourly() {
        let registry = cron_registry();
        let job = registry
            .get("CustomizationActiveWeekBackfillJob")
            .expect("active-week backfill registered");
        assert_eq!(
            job.description,
            Some("Credit historical finished-game weeks")
        );
        let cja::cron::Schedule::Interval(interval) = &job.schedule else {
            panic!("active-week backfill must use an interval");
        };
        assert_eq!(
            interval.0,
            Duration::from_secs(CUSTOMIZATION_ACTIVE_WEEK_BACKFILL_INTERVAL_SECS)
        );
        let now = chrono::Utc::now();
        assert!(job.schedule.should_run(None, now, now, cja::chrono_tz::UTC));
        assert!(
            !job.schedule
                .should_run(Some(&now), now, now, cja::chrono_tz::UTC)
        );
        assert!(job.schedule.should_run(
            Some(&(now - chrono::Duration::hours(2))),
            now,
            now,
            cja::chrono_tz::UTC
        ));
    }

    #[tokio::test]
    async fn dispatch_callback_logs_cause_and_returns_success() -> cja::Result<()> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://127.0.0.1:1/arena")?;
        let app = crate::state::AppState::test_from_pool(pool);
        let errors = Errors::default();
        dispatch_ladder_callback(app)
            .with_subscriber(tracing_subscriber::registry().with(errors.clone()))
            .await
            .expect("cron callback should absorb a dispatch failure");
        let logged = errors.0.lock().unwrap();
        assert!(
            logged.iter().any(
                |error| error.contains("Failed to acquire schedule connection")
                    && (error.contains("refused") || error.contains("timed out"))
            ),
            "missing error cause chain: {logged:?}"
        );
        Ok(())
    }
}
