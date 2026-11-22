//! League membership (`leaderboard_registrations`) and the per-league health
//! state the matchmaking sweeper keeps on it (BS-3534, DEV-1515, DEV-2642).
//!
//! A snake joins a **league** once: one registration per (league, snake),
//! independent of seasons. Per-season ratings live on `leaderboard_entries`,
//! created lazily for the active season (see
//! `leaderboard::ensure_snake_season_entry`). The registration is the source
//! of truth for membership: `disabled_at`/`disabled_reason` (NULL = in
//! rotation, NULL reason = manual pause, `'health'` / `'retired'` /
//! `'deleted'`) and the four health-streak columns.
//!
//! The sweeper probes a registration with a test game shaped like its
//! leaderboard, so a snake that only breaks on Royale fails only its Royale
//! registration. `health_consecutive_failures` climbs on failed probes and
//! resets on any pass; when it crosses the configured threshold the sweeper
//! pulls just that registration with `disabled_reason = 'health'`.
//! Health-paused registrations keep getting probed:
//! `health_consecutive_successes` climbs on passing probes and the sweeper
//! puts the registration back once it crosses the recovery threshold.
//!
//! The registration row itself is the deactivation marker, and every
//! transition is one guarded `UPDATE` on it: [`deactivate`] only flips an
//! enabled registration, [`reactivate`] only a health-paused one, and each
//! returns `true` only for the call that actually flipped it. That is what
//! gates the owner emails to once per transition no matter how often the job
//! retries, and why there's no second "deactivated" stamp to drift out of sync
//! (the DEV-710 bugs).
//!
//! Registrations are the only membership writer. The entry-side
//! `disabled_at`/`disabled_reason`/`health_*` columns are inert leftovers
//! from before seasons and nothing reads them for membership.

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::models::battlesnake::{DISABLED_REASON_DELETED, DISABLED_REASON_RETIRED};

/// `disabled_reason` value the sweeper writes on registrations it disables.
/// Manual pauses leave the reason NULL.
pub const DISABLED_REASON_HEALTH: &str = "health";

/// Minimum spacing between counted probes (failures and recovery successes
/// alike). The cron enqueues a sweep every 30 minutes with no dedup or
/// completion check, so sweeps can pile up and drain back-to-back (or
/// overlap with `workers > 1`); without this gate a registration down for one
/// short window could burn its whole failure budget in minutes — or bounce
/// back into matchmaking off a burst of piled-up probes. Half the sweep
/// interval: real consecutive sweeps always count, piled-up ones don't.
const PROBE_COUNT_SPACING_MINUTES: i32 = 15;

/// A snake's membership in one league.
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardRegistration {
    pub leaderboard_registration_id: Uuid,
    pub leaderboard_id: Uuid,
    pub battlesnake_id: Uuid,
    pub disabled_at: Option<DateTime<Utc>>,
    pub disabled_reason: Option<String>,
    pub health_consecutive_failures: i32,
    pub health_consecutive_successes: i32,
    pub health_last_checked_at: Option<DateTime<Utc>>,
    pub health_last_failure: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One of a snake's leagues the sweeper pulled it from, for the owner-facing
/// profile banner.
#[derive(Debug, Clone)]
pub struct HealthPausedLeague {
    pub leaderboard_registration_id: Uuid,
    pub leaderboard_name: String,
    pub paused_at: DateTime<Utc>,
    pub consecutive_failures: i32,
    pub last_failure: Option<String>,
}

/// Register a snake in a league ("put this snake in rotation"). `Ok(None)` =
/// the snake is missing, deleted or retired, or the registration is
/// terminally marked `'deleted'`/`'retired'` (join refused); callers surface
/// that as not-found, not an error.
///
/// A paused registration (manually or by the health sweeper) is re-enabled
/// with its health streaks reset, so a streak from before the owner's fix
/// can't re-pause it on the next failed probe. Re-joining a registration
/// that's already in rotation changes nothing — an idempotent retry mustn't
/// wipe a real failing streak.
///
pub async fn get_or_create_registration(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardRegistration>> {
    // Selecting the snake FOR SHARE (instead of VALUES) refuses deleted and
    // retired snakes and serializes against `battlesnake::delete_battlesnake`
    // and `retire_battlesnake`'s row locks (the FK insert alone takes only
    // FOR KEY SHARE, which doesn't). Keep the liveness predicates on the
    // locked battlesnakes scan itself: when FOR SHARE waits on a row lock,
    // READ COMMITTED re-checks only the locked relation's own predicates
    // against the committed row version (EvalPlanQual). A liveness check
    // anywhere else reads the pre-wait snapshot and lets the join through.
    sqlx::query_as!(
        LeaderboardRegistration,
        r#"INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id)
         SELECT $1::uuid, b.battlesnake_id
         FROM battlesnakes b
         WHERE b.battlesnake_id = $2
           AND b.deleted_at IS NULL
           AND b.retired_at IS NULL
         FOR SHARE
         ON CONFLICT (leaderboard_id, battlesnake_id) DO UPDATE
            SET disabled_at = NULL, disabled_reason = NULL, updated_at = NOW(),
                health_consecutive_failures = CASE
                    WHEN leaderboard_registrations.disabled_at IS NULL
                    THEN leaderboard_registrations.health_consecutive_failures
                    ELSE 0
                END,
                health_consecutive_successes = 0,
                health_last_failure = CASE
                    WHEN leaderboard_registrations.disabled_at IS NULL
                    THEN leaderboard_registrations.health_last_failure
                END
            WHERE leaderboard_registrations.disabled_reason IS DISTINCT FROM $3
              AND leaderboard_registrations.disabled_reason IS DISTINCT FROM $4
         RETURNING
            leaderboard_registration_id, leaderboard_id, battlesnake_id,
            disabled_at, disabled_reason,
            health_consecutive_failures, health_consecutive_successes,
            health_last_checked_at, health_last_failure,
            created_at, updated_at"#,
        leaderboard_id,
        battlesnake_id,
        DISABLED_REASON_DELETED,
        DISABLED_REASON_RETIRED
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to create or get leaderboard registration")
}

/// Snakes currently eligible to be drawn from this league: an active
/// registration on a live (not deleted, not retired) snake.
///
/// Frozen standings of an ended season deliberately include entrants who
/// later left; anything that *acts* on standings (tournament import) must
/// intersect them with this set so leavers and dead snakes are never picked.
pub async fn live_member_snake_ids(
    conn: &mut PgConnection,
    leaderboard_id: Uuid,
) -> cja::Result<std::collections::HashSet<Uuid>> {
    let ids = sqlx::query_scalar!(
        r#"SELECT lr.battlesnake_id
         FROM leaderboard_registrations lr
         JOIN battlesnakes b ON b.battlesnake_id = lr.battlesnake_id
         WHERE lr.leaderboard_id = $1
           AND lr.disabled_at IS NULL
           AND b.deleted_at IS NULL
           AND b.retired_at IS NULL"#,
        leaderboard_id
    )
    .fetch_all(conn)
    .await
    .wrap_err("Failed to fetch live league members")?;
    Ok(ids.into_iter().collect())
}

/// A snake's registration in a league, whatever its state.
pub async fn get_registration(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardRegistration>> {
    sqlx::query_as!(
        LeaderboardRegistration,
        r#"SELECT leaderboard_registration_id, leaderboard_id, battlesnake_id,
                disabled_at, disabled_reason,
                health_consecutive_failures, health_consecutive_successes,
                health_last_checked_at, health_last_failure,
                created_at, updated_at
         FROM leaderboard_registrations
         WHERE leaderboard_id = $1 AND battlesnake_id = $2"#,
        leaderboard_id,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard registration")
}

/// Pause (`Some`) or resume (`None`) a snake's registration (owner/admin
/// action).
///
/// Always clears `disabled_reason`: a manual pause is reason-less, and a
/// manual resume of a health-paused registration means the owner has taken
/// over from the sweeper. Resume doesn't reset streaks. Terminal
/// (`'deleted'`/`'retired'`) registrations are never touched, and a resume
/// only happens for a live snake.
///
/// A resume holds the snake's liveness lock for its transaction.
pub async fn set_registration_disabled(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
    disabled_at: Option<DateTime<Utc>>,
) -> cja::Result<()> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin registration pause/resume")?;
    if disabled_at.is_none() {
        let live = sqlx::query_scalar!(
            "SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 AND deleted_at IS NULL AND retired_at IS NULL FOR SHARE",
            battlesnake_id
        )
        .fetch_optional(&mut *tx)
        .await
        .wrap_err("Failed to lock snake for leaderboard resume")?;
        if live.is_none() {
            return Ok(());
        }
    }
    sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET disabled_at = $3, disabled_reason = NULL
         WHERE leaderboard_id = $1 AND battlesnake_id = $2
           AND disabled_reason IS DISTINCT FROM $4
           AND disabled_reason IS DISTINCT FROM $5"#,
        leaderboard_id,
        battlesnake_id,
        disabled_at,
        DISABLED_REASON_DELETED,
        DISABLED_REASON_RETIRED
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to update leaderboard registration disabled status")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit registration pause/resume")?;
    Ok(())
}

/// Take every in-rotation registration of a snake out of matchmaking with
/// `reason` (retirement). Health streaks are untouched. Returns how many
/// registrations it paused.
///
/// Runs on the caller's lifecycle transaction. Unrelated to the tournament `withdrawn_registrations` count in `retire_battlesnake`.
pub async fn pause_registrations_for_snake(
    connection: &mut PgConnection,
    battlesnake_id: Uuid,
    reason: &str,
) -> cja::Result<u64> {
    let result = sqlx::query!(
        "UPDATE leaderboard_registrations SET disabled_at = NOW(), disabled_reason = $2, updated_at = NOW() WHERE battlesnake_id = $1 AND disabled_at IS NULL",
        battlesnake_id,
        reason
    )
    .execute(connection)
    .await
    .wrap_err("Failed to pause snake's leaderboard registrations")?;
    Ok(result.rows_affected())
}

/// Mark every registration of a deleted snake `'deleted'` (terminal), keeping
/// an existing pause timestamp. Returns how many registrations it marked.
///
/// Runs on the caller's lifecycle transaction (`delete_battlesnake`).
/// Unrelated to tournament registrations.
pub async fn mark_registrations_deleted_for_snake(
    connection: &mut PgConnection,
    battlesnake_id: Uuid,
) -> cja::Result<u64> {
    let result = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET disabled_at = COALESCE(disabled_at, NOW()),
             disabled_reason = $2,
             updated_at = NOW()
         WHERE battlesnake_id = $1"#,
        battlesnake_id,
        DISABLED_REASON_DELETED
    )
    .execute(connection)
    .await
    .wrap_err("Failed to mark deleted snake's leaderboard registrations")?;
    Ok(result.rows_affected())
}

/// Put back every registration of a snake paused with `reason` (un-retire),
/// with health streaks reset. Manual and health pauses are untouched. Returns
/// how many registrations it resumed.
///
/// Runs on the caller's lifecycle transaction. Unrelated to tournament registrations.
pub async fn resume_registrations_for_snake(
    connection: &mut PgConnection,
    battlesnake_id: Uuid,
    reason: &str,
) -> cja::Result<u64> {
    let result = sqlx::query!(
        "UPDATE leaderboard_registrations SET disabled_at = NULL, disabled_reason = NULL, health_consecutive_failures = 0, health_consecutive_successes = 0, health_last_failure = NULL, updated_at = NOW() WHERE battlesnake_id = $1 AND disabled_reason = $2",
        battlesnake_id,
        reason
    )
    .execute(connection)
    .await
    .wrap_err("Failed to resume snake's leaderboard registrations")?;
    Ok(result.rows_affected())
}

/// Record a passing probe of a registration in matchmaking: the failure
/// streak starts over. `checked_at` is when the sweep started (the evidence
/// cursor: bad turns after it will be looked at next sweep).
pub async fn record_success(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
    checked_at: DateTime<Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET health_consecutive_failures = 0,
             health_last_failure = NULL,
             health_last_checked_at = $2
         WHERE leaderboard_registration_id = $1 AND disabled_at IS NULL"#,
        leaderboard_registration_id,
        checked_at,
    )
    .execute(connection)
    .await
    .wrap_err("Failed to record registration health success")?;
    Ok(())
}

/// Record a failed probe of a registration in matchmaking and return the
/// consecutive-failure count, or `None` if the registration left matchmaking
/// meanwhile (the owner paused it).
///
/// The count only increments when the previous check is at least
/// [`PROBE_COUNT_SPACING_MINUTES`] older — back-to-back probes from piled-up
/// or concurrent sweeps update the failure text but don't inflate the
/// streak, keeping "N consecutive failures" meaning N separate sweep windows
/// (~N × 30 min), as the threshold design assumes.
pub async fn record_failure(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
    failure_summary: &str,
    checked_at: DateTime<Utc>,
) -> cja::Result<Option<i32>> {
    let row = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET health_consecutive_failures = CASE
                 WHEN health_last_checked_at IS NULL
                   OR health_last_checked_at <= $3::timestamptz - make_interval(mins => $4)
                 THEN health_consecutive_failures + 1
                 ELSE GREATEST(health_consecutive_failures, 1)
             END,
             health_last_failure = $2,
             health_last_checked_at = $3
         WHERE leaderboard_registration_id = $1 AND disabled_at IS NULL
         RETURNING health_consecutive_failures"#,
        leaderboard_registration_id,
        failure_summary,
        checked_at,
        PROBE_COUNT_SPACING_MINUTES,
    )
    .fetch_optional(connection)
    .await
    .wrap_err("Failed to record registration health failure")?;
    Ok(row.map(|row| row.health_consecutive_failures))
}

/// Record a passing probe of a health-paused registration and return the
/// consecutive-success count, or `None` if the registration isn't
/// health-paused any more (the owner resumed it meanwhile). Spacing-gated
/// like [`record_failure`], so piled-up sweeps can't fake a recovery in
/// minutes.
pub async fn record_recovery_success(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
    checked_at: DateTime<Utc>,
) -> cja::Result<Option<i32>> {
    let row = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET health_consecutive_successes = CASE
                 WHEN health_last_checked_at IS NULL
                   OR health_last_checked_at <= $2::timestamptz - make_interval(mins => $3)
                 THEN health_consecutive_successes + 1
                 ELSE GREATEST(health_consecutive_successes, 1)
             END,
             health_last_checked_at = $2
         WHERE leaderboard_registration_id = $1 AND disabled_reason = $4
         RETURNING health_consecutive_successes"#,
        leaderboard_registration_id,
        checked_at,
        PROBE_COUNT_SPACING_MINUTES,
        DISABLED_REASON_HEALTH,
    )
    .fetch_optional(connection)
    .await
    .wrap_err("Failed to record registration recovery success")?;
    Ok(row.map(|row| row.health_consecutive_successes))
}

/// Record a failed probe of a health-paused registration: the recovery
/// streak starts over. No spacing gate — any failure genuinely breaks the
/// streak.
pub async fn record_recovery_failure(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
    failure_summary: &str,
    checked_at: DateTime<Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET health_consecutive_successes = 0,
             health_last_failure = $2,
             health_last_checked_at = $3
         WHERE leaderboard_registration_id = $1 AND disabled_reason = $4"#,
        leaderboard_registration_id,
        failure_summary,
        checked_at,
        DISABLED_REASON_HEALTH,
    )
    .execute(connection)
    .await
    .wrap_err("Failed to record registration recovery failure")?;
    Ok(())
}

/// Pull a registration from matchmaking (`disabled_reason = 'health'`) and
/// start its recovery streak from zero.
///
/// Returns `true` only when this call performed the enabled -> paused
/// transition; the caller emails the owner exactly on that `true`, which
/// keeps a re-entrant job from emailing twice. A registration that's already
/// disabled (by the owner, or a deleted snake) is left alone.
pub async fn deactivate(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET disabled_at = NOW(),
             disabled_reason = $2,
             health_consecutive_successes = 0,
             updated_at = NOW()
         WHERE leaderboard_registration_id = $1 AND disabled_at IS NULL"#,
        leaderboard_registration_id,
        DISABLED_REASON_HEALTH
    )
    .execute(connection)
    .await
    .wrap_err("Failed to pull leaderboard registration from matchmaking")?;
    Ok(result.rows_affected() > 0)
}

/// Put a health-paused registration back into matchmaking with both streaks
/// reset, so the next sweep starts fresh. A manual pause is never touched.
///
/// Returns `true` only when this call performed the paused -> enabled
/// transition; the sweeper sends the "back in matchmaking" email exactly on
/// that `true`.
pub async fn reactivate(
    connection: &mut PgConnection,
    leaderboard_registration_id: Uuid,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET disabled_at = NULL,
             disabled_reason = NULL,
             health_consecutive_failures = 0,
             health_consecutive_successes = 0,
             health_last_failure = NULL,
             updated_at = NOW()
         WHERE leaderboard_registration_id = $1 AND disabled_reason = $2"#,
        leaderboard_registration_id,
        DISABLED_REASON_HEALTH
    )
    .execute(connection)
    .await
    .wrap_err("Failed to put leaderboard registration back into matchmaking")?;
    Ok(result.rows_affected() > 0)
}

/// The owner's Resume Matchmaking: [`reactivate`] every one of the snake's
/// health-paused registrations. Returns how many were resumed.
///
/// One transaction: snake liveness lock, then the update.
pub async fn reactivate_snake(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin snake reactivation")?;
    let live = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 AND deleted_at IS NULL AND retired_at IS NULL FOR SHARE",
        battlesnake_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock snake for reactivation")?;
    if live.is_none() {
        return Ok(0);
    }
    let result = sqlx::query!(
        r#"UPDATE leaderboard_registrations
         SET disabled_at = NULL,
             disabled_reason = NULL,
             health_consecutive_failures = 0,
             health_consecutive_successes = 0,
             health_last_failure = NULL,
             updated_at = NOW()
         WHERE battlesnake_id = $1 AND disabled_reason = $2"#,
        battlesnake_id,
        DISABLED_REASON_HEALTH
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to put snake back into matchmaking")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit snake reactivation")?;
    Ok(result.rows_affected())
}

/// A snake's leagues the sweeper pulled it from, on leaderboards that are
/// still running, oldest pause first.
pub async fn health_paused_leagues(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<HealthPausedLeague>> {
    sqlx::query_as!(
        HealthPausedLeague,
        r#"SELECT r.leaderboard_registration_id,
                  l.name AS leaderboard_name,
                  r.disabled_at AS "paused_at!",
                  r.health_consecutive_failures AS consecutive_failures,
                  r.health_last_failure AS last_failure
         FROM leaderboard_registrations r
         JOIN leaderboards l ON l.leaderboard_id = r.leaderboard_id
         WHERE r.battlesnake_id = $1
           AND r.disabled_reason = $2
           AND r.disabled_at IS NOT NULL
           AND l.disabled_at IS NULL
         ORDER BY r.disabled_at, l.name"#,
        battlesnake_id,
        DISABLED_REASON_HEALTH
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch health-paused leagues")
}

/// Resolve only for a nondeleted snake owned by this user.
pub async fn owner_notification_email(
    connection: &mut PgConnection,
    battlesnake_id: Uuid,
) -> cja::Result<Option<String>> {
    let user_id = sqlx::query_scalar!(
        "SELECT user_id FROM battlesnakes WHERE battlesnake_id = $1 AND deleted_at IS NULL",
        battlesnake_id
    )
    .fetch_optional(&mut *connection)
    .await?;
    match user_id {
        Some(user_id) => {
            crate::models::notification_email::resolved_recipient(connection, user_id).await
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES ($1, $2)
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    async fn create_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        let id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://example.com/snake')
             RETURNING battlesnake_id",
            user_id,
            name,
        )
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    async fn create_registration(
        pool: &PgPool,
        battlesnake_id: Uuid,
        board: &str,
    ) -> cja::Result<Uuid> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            board,
        )
        .fetch_one(pool)
        .await?;
        let registration = get_or_create_registration(pool, leaderboard_id, battlesnake_id)
            .await?
            .expect("live snake joins");
        Ok(registration.leaderboard_registration_id)
    }

    async fn pause_manually(pool: &PgPool, registration_id: Uuid) -> cja::Result<()> {
        let row = sqlx::query!(
            "SELECT leaderboard_id, battlesnake_id FROM leaderboard_registrations WHERE leaderboard_registration_id = $1",
            registration_id
        )
        .fetch_one(pool)
        .await?;
        set_registration_disabled(
            pool,
            row.leaderboard_id,
            row.battlesnake_id,
            Some(Utc::now()),
        )
        .await
    }

    struct RegistrationState {
        disabled: bool,
        reason: Option<String>,
        failures: i32,
        successes: i32,
        last_failure: Option<String>,
    }

    async fn state(pool: &PgPool, registration_id: Uuid) -> cja::Result<RegistrationState> {
        let row = sqlx::query!(
            r#"SELECT disabled_at IS NOT NULL AS "disabled!", disabled_reason,
                      health_consecutive_failures, health_consecutive_successes,
                      health_last_failure
             FROM leaderboard_registrations WHERE leaderboard_registration_id = $1"#,
            registration_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(RegistrationState {
            disabled: row.disabled,
            reason: row.disabled_reason,
            failures: row.health_consecutive_failures,
            successes: row.health_consecutive_successes,
            last_failure: row.health_last_failure,
        })
    }

    /// A sweep 30 minutes after `t`, i.e. a real next sweep.
    fn next_sweep(t: DateTime<Utc>) -> DateTime<Utc> {
        t + Duration::minutes(30)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failure_streak_is_spacing_gated_and_reset_by_success(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9001).await?;
        let snake_id = create_snake(&pool, user_id, "streaky").await?;
        let entry = create_registration(&pool, snake_id, "standard").await?;
        let t0 = Utc::now();

        assert_eq!(
            record_failure(&mut *pool.acquire().await?, entry, "timeout", t0).await?,
            Some(1)
        );
        // A piled-up sweep minutes later must not inflate the streak.
        let piled = t0 + Duration::minutes(2);
        assert_eq!(
            record_failure(&mut *pool.acquire().await?, entry, "refused", piled).await?,
            Some(1)
        );
        assert_eq!(
            state(&pool, entry).await?.last_failure.as_deref(),
            Some("refused")
        );
        // A real next sweep does count.
        let t1 = next_sweep(piled);
        assert_eq!(
            record_failure(&mut *pool.acquire().await?, entry, "refused", t1).await?,
            Some(2)
        );

        record_success(&mut *pool.acquire().await?, entry, next_sweep(t1)).await?;
        let after = state(&pool, entry).await?;
        assert_eq!(after.failures, 0);
        assert!(after.last_failure.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn probes_of_a_paused_entry_never_count(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9002).await?;
        let snake_id = create_snake(&pool, user_id, "paused").await?;
        let entry = create_registration(&pool, snake_id, "standard").await?;
        pause_manually(&pool, entry).await?;

        let now = Utc::now();
        assert_eq!(
            record_failure(&mut *pool.acquire().await?, entry, "down", now).await?,
            None
        );
        assert_eq!(
            record_recovery_success(&mut *pool.acquire().await?, entry, now).await?,
            None
        );
        assert!(!deactivate(&mut *pool.acquire().await?, entry).await?);
        let after = state(&pool, entry).await?;
        assert_eq!((after.failures, after.successes), (0, 0));
        assert_eq!(after.reason, None, "a manual pause stays a manual pause");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deactivate_pulls_only_that_entry_and_notifies_once(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9003).await?;
        let snake_id = create_snake(&pool, user_id, "royale-crasher").await?;
        let royale = create_registration(&pool, snake_id, "royale").await?;
        let duels = create_registration(&pool, snake_id, "duels").await?;

        assert!(deactivate(&mut *pool.acquire().await?, royale).await?);
        assert!(
            !deactivate(&mut *pool.acquire().await?, royale).await?,
            "a retry must not notify again"
        );

        let royale = state(&pool, royale).await?;
        assert!(royale.disabled);
        assert_eq!(royale.reason.as_deref(), Some(DISABLED_REASON_HEALTH));
        assert!(!state(&pool, duels).await?.disabled);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn recovery_streak_counts_only_while_paused(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9004).await?;
        let snake_id = create_snake(&pool, user_id, "recovering").await?;
        let entry = create_registration(&pool, snake_id, "standard").await?;
        let t0 = Utc::now();
        record_failure(&mut *pool.acquire().await?, entry, "down", t0).await?;
        assert!(deactivate(&mut *pool.acquire().await?, entry).await?);

        let t1 = next_sweep(t0);
        assert_eq!(
            record_recovery_success(&mut *pool.acquire().await?, entry, t1).await?,
            Some(1)
        );
        let piled = t1 + Duration::minutes(1);
        assert_eq!(
            record_recovery_success(&mut *pool.acquire().await?, entry, piled).await?,
            Some(1)
        );
        let t2 = next_sweep(piled);
        assert_eq!(
            record_recovery_success(&mut *pool.acquire().await?, entry, t2).await?,
            Some(2)
        );
        record_recovery_failure(
            &mut *pool.acquire().await?,
            entry,
            "down again",
            next_sweep(t2),
        )
        .await?;
        assert_eq!(state(&pool, entry).await?.successes, 0);

        assert!(reactivate(&mut *pool.acquire().await?, entry).await?);
        assert!(
            !reactivate(&mut *pool.acquire().await?, entry).await?,
            "only one call flips it"
        );
        let after = state(&pool, entry).await?;
        assert!(!after.disabled);
        assert_eq!((after.failures, after.successes), (0, 0));
        assert!(after.last_failure.is_none());

        // Owner resumed meanwhile: a late recovery probe doesn't count.
        assert_eq!(
            record_recovery_success(&mut *pool.acquire().await?, entry, Utc::now()).await?,
            None
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reactivate_never_touches_manual_pauses(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9005).await?;
        let snake_id = create_snake(&pool, user_id, "mixed").await?;
        let pulled = create_registration(&pool, snake_id, "standard").await?;
        let paused = create_registration(&pool, snake_id, "royale").await?;
        pause_manually(&pool, paused).await?;
        record_failure(&mut *pool.acquire().await?, pulled, "down", Utc::now()).await?;
        assert!(deactivate(&mut *pool.acquire().await?, pulled).await?);

        assert_eq!(health_paused_leagues(&pool, snake_id).await?.len(), 1);
        assert!(!reactivate(&mut *pool.acquire().await?, paused).await?);
        assert_eq!(reactivate_snake(&pool, snake_id).await?, 1);

        assert!(!state(&pool, pulled).await?.disabled);
        let paused = state(&pool, paused).await?;
        assert!(paused.disabled);
        assert_eq!(paused.reason, None);
        Ok(())
    }

    /// Joining is "put me in rotation": re-enabling a paused entry starts its
    /// health over, but re-joining an entry that's already in rotation (an
    /// idempotent API retry) must not wipe a real failing streak.
    #[sqlx::test(migrations = "../migrations")]
    async fn joining_resets_health_only_when_it_re_enables(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9006).await?;
        let snake_id = create_snake(&pool, user_id, "joiner").await?;
        let entry = create_registration(&pool, snake_id, "standard").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboard_registrations WHERE leaderboard_registration_id = $1",
            entry
        )
        .fetch_one(&pool)
        .await?;

        record_failure(&mut *pool.acquire().await?, entry, "down", Utc::now()).await?;
        get_or_create_registration(&pool, leaderboard_id, snake_id)
            .await?
            .expect("live snake joins");
        assert_eq!(
            state(&pool, entry).await?.failures,
            1,
            "already in rotation"
        );

        assert!(deactivate(&mut *pool.acquire().await?, entry).await?);
        get_or_create_registration(&pool, leaderboard_id, snake_id)
            .await?
            .expect("live snake joins");
        let after = state(&pool, entry).await?;
        assert!(!after.disabled);
        assert_eq!((after.failures, after.successes), (0, 0));
        assert!(after.last_failure.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paused_entries_skip_retired_leaderboards(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9007).await?;
        let snake_id = create_snake(&pool, user_id, "retired").await?;
        let live = create_registration(&pool, snake_id, "live").await?;
        let retired = create_registration(&pool, snake_id, "retired").await?;
        for entry in [live, retired] {
            record_failure(
                &mut *pool.acquire().await?,
                entry,
                "POST /move (turn 0): boom",
                Utc::now(),
            )
            .await?;
            assert!(deactivate(&mut *pool.acquire().await?, entry).await?);
        }
        sqlx::query!("UPDATE leaderboards SET disabled_at = NOW() WHERE name = 'retired'")
            .execute(&pool)
            .await?;

        let paused = health_paused_leagues(&pool, snake_id).await?;
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].leaderboard_registration_id, live);
        assert_eq!(paused[0].leaderboard_name, "live");
        assert_eq!(paused[0].consecutive_failures, 1);
        assert_eq!(
            paused[0].last_failure.as_deref(),
            Some("POST /move (turn 0): boom")
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owner_email_prefers_github_and_falls_back_to_claimed_play_account(
        pool: PgPool,
    ) -> cja::Result<()> {
        // No email anywhere -> None.
        let user_a = create_user(&pool, 9008).await?;
        let snake_a = create_snake(&pool, user_a, "no-email").await?;
        assert_eq!(
            owner_notification_email(&mut *pool.acquire().await?, snake_a).await?,
            None
        );

        // A claimed play account supplies the fallback address.
        sqlx::query!(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, is_email_verified, claimed_by_user_id)
             VALUES ('usr_h1', 'act_h1', 'play@example.com', 'player', true, $1)",
            user_a,
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            owner_notification_email(&mut *pool.acquire().await?, snake_a)
                .await?
                .as_deref(),
            Some("play@example.com")
        );

        // GitHub email wins when present.
        sqlx::query!(
            "UPDATE users SET github_email = 'gh@example.com' WHERE user_id = $1",
            user_a,
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            owner_notification_email(&mut *pool.acquire().await?, snake_a)
                .await?
                .as_deref(),
            Some("gh@example.com")
        );

        sqlx::query!(
            "UPDATE users SET notification_email = 'confirmed@example.com' WHERE user_id = $1",
            user_a
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            owner_notification_email(&mut *pool.acquire().await?, snake_a)
                .await?
                .as_deref(),
            Some("confirmed@example.com")
        );
        sqlx::query!(
            "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
            snake_a
        )
        .execute(&pool)
        .await?;
        assert!(
            owner_notification_email(&mut *pool.acquire().await?, snake_a)
                .await?
                .is_none()
        );

        Ok(())
    }

    type SixColumns = (
        Option<DateTime<Utc>>,
        Option<String>,
        i32,
        i32,
        Option<DateTime<Utc>>,
        Option<String>,
    );

    async fn registration_columns(pool: &PgPool, registration_id: Uuid) -> SixColumns {
        sqlx::query_as(
            "SELECT disabled_at, disabled_reason, health_consecutive_failures,
                    health_consecutive_successes, health_last_checked_at, health_last_failure
             FROM leaderboard_registrations WHERE leaderboard_registration_id = $1",
        )
        .bind(registration_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn entry_columns(pool: &PgPool, entry_id: Uuid) -> SixColumns {
        sqlx::query_as(
            "SELECT disabled_at, disabled_reason, health_consecutive_failures,
                    health_consecutive_successes, health_last_checked_at, health_last_failure
             FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Registrations are the only membership writer: a health pause lands on
    /// the registration and leaves the entry's inert columns untouched, and
    /// an entry-side write no longer reaches the registration (DEV-2642 PR 2
    /// dropped the mirror and the sync trigger).
    #[sqlx::test(migrations = "../migrations")]
    async fn registration_and_entry_membership_columns_are_decoupled(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 9010).await?;
        let snake_id = create_snake(&pool, user_id, "decoupled").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('decoupled') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let entry =
            crate::models::leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_id)
                .await?
                .expect("entry");
        let registration = get_registration(&pool, leaderboard_id, snake_id)
            .await?
            .expect("registration")
            .leaderboard_registration_id;
        let entry_before = entry_columns(&pool, entry.leaderboard_entry_id).await;

        let t0 = Utc::now();
        record_failure(&mut *pool.acquire().await?, registration, "timeout", t0).await?;
        assert!(deactivate(&mut *pool.acquire().await?, registration).await?);
        let after = registration_columns(&pool, registration).await;
        assert!(after.0.is_some());
        assert_eq!(after.1.as_deref(), Some(DISABLED_REASON_HEALTH));
        assert_eq!(after.2, 1);
        assert_eq!(after.5.as_deref(), Some("timeout"));
        assert_eq!(
            entry_columns(&pool, entry.leaderboard_entry_id).await,
            entry_before,
            "entry columns untouched by registration writes"
        );
        assert_eq!(entry_before, (None, None, 0, 0, None, None));

        sqlx::query(
            "UPDATE leaderboard_entries
             SET disabled_at = NULL, disabled_reason = NULL,
                 health_consecutive_failures = 0, health_last_failure = NULL
             WHERE leaderboard_entry_id = $1",
        )
        .bind(entry.leaderboard_entry_id)
        .execute(&pool)
        .await?;
        assert_eq!(
            registration_columns(&pool, registration).await,
            after,
            "entry writes no longer reach the registration"
        );
        Ok(())
    }
}
