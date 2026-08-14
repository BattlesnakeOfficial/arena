//! Per-snake health state for the matchmaking sweeper (BS-3534).
//!
//! One row per snake the sweeper has probed. `consecutive_failures` climbs on
//! failed probes and resets to zero on any success; when it crosses the
//! configured threshold the sweeper disables the snake's leaderboard entries
//! (with `disabled_reason = 'health'`) and stamps `deactivated_at`. That
//! stamp is a compare-and-set: only the transition from NULL "wins", which is
//! what gates the owner notification email to once per deactivation no matter
//! how often the job retries.
//!
//! Deactivated snakes keep getting probed: `consecutive_successes` climbs on
//! healthy probes (spacing-gated the same way as failures) and the sweeper
//! auto-reactivates once it crosses the recovery threshold — the mirror image
//! of the deactivation flow, gated by the NOW() -> NULL transition on
//! `deactivated_at`.

use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// `disabled_reason` value the sweeper writes on leaderboard entries it
/// disables. Manual pauses leave the reason NULL.
pub const DISABLED_REASON_HEALTH: &str = "health";

#[derive(Debug, Clone)]
pub struct SnakeHealthStatus {
    pub battlesnake_id: Uuid,
    pub consecutive_failures: i32,
    pub consecutive_successes: i32,
    pub last_checked_at: chrono::DateTime<chrono::Utc>,
    pub last_failure: Option<String>,
    pub deactivated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Fetch a snake's health row, if the sweeper has ever probed it.
pub async fn get(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<Option<SnakeHealthStatus>> {
    sqlx::query_as!(
        SnakeHealthStatus,
        r#"SELECT battlesnake_id, consecutive_failures, consecutive_successes, last_checked_at, last_failure, deactivated_at
         FROM snake_health_status
         WHERE battlesnake_id = $1"#,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch snake health status")
}

/// Record a successful probe: reset the failure streak and clear any stale
/// deactivation stamp.
///
/// Clearing `deactivated_at` here matters: the owner can put a deactivated
/// snake back into matchmaking through paths that don't know about health
/// state (re-joining a leaderboard, the leaderboard page's Resume button).
/// A success probe only runs for snakes back in matchmaking, which is
/// exactly when the once-per-deactivation email gate should re-arm — and
/// when the profile banner should stop claiming the snake is paused.
pub async fn record_success(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        r#"INSERT INTO snake_health_status (battlesnake_id, consecutive_failures, last_checked_at, last_failure)
         VALUES ($1, 0, NOW(), NULL)
         ON CONFLICT (battlesnake_id) DO UPDATE
            SET consecutive_failures = 0,
                last_checked_at = NOW(),
                last_failure = NULL,
                deactivated_at = NULL,
                updated_at = NOW()"#,
        battlesnake_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record snake health success")?;

    Ok(())
}

/// Minimum spacing between counted probes (failures and recovery
/// successes alike). The cron enqueues a sweep every 30 minutes with no
/// dedup or completion check, so sweeps can pile up and drain back-to-back
/// (or overlap with `workers > 1`); without this gate a snake down for one
/// short window could burn its whole failure budget in minutes — or bounce
/// back into matchmaking off a burst of piled-up probes. Half the sweep
/// interval: real consecutive sweeps always count, piled-up ones don't.
const PROBE_COUNT_SPACING_MINUTES: i32 = 15;

/// Record a failed probe and return the consecutive-failure count.
///
/// The count only increments when the previous check is at least
/// [`PROBE_COUNT_SPACING_MINUTES`] old — back-to-back probes from
/// piled-up or concurrent sweeps update `last_failure` but don't inflate
/// the streak, keeping "N consecutive failures" meaning N separate sweep
/// windows (~N × 30 min), as the threshold design assumes.
pub async fn record_failure(
    pool: &PgPool,
    battlesnake_id: Uuid,
    failure_summary: &str,
) -> cja::Result<i32> {
    let row = sqlx::query!(
        r#"INSERT INTO snake_health_status (battlesnake_id, consecutive_failures, last_checked_at, last_failure)
         VALUES ($1, 1, NOW(), $2)
         ON CONFLICT (battlesnake_id) DO UPDATE
            SET consecutive_failures = CASE
                    WHEN snake_health_status.last_checked_at
                         <= NOW() - make_interval(mins => $3)
                    THEN snake_health_status.consecutive_failures + 1
                    ELSE GREATEST(snake_health_status.consecutive_failures, 1)
                END,
                last_checked_at = NOW(),
                last_failure = $2,
                updated_at = NOW()
         RETURNING consecutive_failures"#,
        battlesnake_id,
        failure_summary,
        PROBE_COUNT_SPACING_MINUTES,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to record snake health failure")?;

    Ok(row.consecutive_failures)
}

/// Record a healthy probe of a *deactivated* snake and return the
/// consecutive-success count. Unlike [`record_success`] this must NOT clear
/// `deactivated_at` — recovery is gated on a streak, and the reactivation
/// itself (entries + stamp + notification) happens in [`reactivate`] once
/// the caller sees the streak cross the threshold.
///
/// Spacing-gated like [`record_failure`], so piled-up sweeps can't fake a
/// recovery streak in minutes.
///
/// Returns 0 when the snake has no health row — a health-disabled entry
/// without one shouldn't be possible, but if it happens the streak simply
/// starts counting from the next probe.
pub async fn record_recovery_success(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<i32> {
    let count = sqlx::query_scalar!(
        r#"UPDATE snake_health_status
         SET consecutive_successes = CASE
                 WHEN last_checked_at <= NOW() - make_interval(mins => $2)
                 THEN consecutive_successes + 1
                 ELSE GREATEST(consecutive_successes, 1)
             END,
             last_checked_at = NOW(),
             updated_at = NOW()
         WHERE battlesnake_id = $1 AND deactivated_at IS NOT NULL
         RETURNING consecutive_successes"#,
        battlesnake_id,
        PROBE_COUNT_SPACING_MINUTES,
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to record recovery success")?;

    Ok(count.unwrap_or(0))
}

/// Record a failed probe of a *deactivated* snake: the recovery streak
/// starts over. No spacing gate — any failure genuinely breaks the streak.
pub async fn record_recovery_failure(
    pool: &PgPool,
    battlesnake_id: Uuid,
    failure_summary: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"UPDATE snake_health_status
         SET consecutive_successes = 0,
             last_checked_at = NOW(),
             last_failure = $2,
             updated_at = NOW()
         WHERE battlesnake_id = $1 AND deactivated_at IS NOT NULL"#,
        battlesnake_id,
        failure_summary,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record recovery failure")?;

    Ok(())
}

/// Pull a snake from matchmaking: disable its active leaderboard entries
/// (tagged `'health'` so reactivation can tell them apart from manual
/// pauses) and stamp `deactivated_at`.
///
/// Returns `true` only when this call performed the NULL -> NOW() transition
/// on `deactivated_at` — the caller sends the owner notification exactly on
/// that `true`, which keeps a re-entrant job from emailing twice.
pub async fn deactivate(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<bool> {
    let mut tx = pool.begin().await.wrap_err("Failed to begin transaction")?;

    sqlx::query!(
        r#"UPDATE leaderboard_entries
         SET disabled_at = NOW(), disabled_reason = $2, updated_at = NOW()
         WHERE battlesnake_id = $1 AND disabled_at IS NULL"#,
        battlesnake_id,
        DISABLED_REASON_HEALTH
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to disable leaderboard entries")?;

    let newly_deactivated = sqlx::query_scalar!(
        r#"UPDATE snake_health_status
         SET deactivated_at = NOW(), consecutive_successes = 0, updated_at = NOW()
         WHERE battlesnake_id = $1 AND deactivated_at IS NULL
         RETURNING battlesnake_id"#,
        battlesnake_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to stamp deactivation")?
    .is_some();

    tx.commit()
        .await
        .wrap_err("Failed to commit deactivation")?;

    Ok(newly_deactivated)
}

/// Recovery (owner-initiated or sweeper auto-recovery): re-enable exactly
/// the entries the sweeper disabled (manual pauses stay paused), clear the
/// deactivation stamp, and reset both streaks so the next sweep starts
/// fresh.
///
/// Returns `true` only when this call performed the NOW() -> NULL
/// transition on `deactivated_at` — the sweeper sends the "back in
/// matchmaking" notification exactly on that `true`, mirroring how
/// [`deactivate`] gates its email.
pub async fn reactivate(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<bool> {
    let mut tx = pool.begin().await.wrap_err("Failed to begin transaction")?;

    // Lock the row so concurrent sweeps can't both observe the NOW() ->
    // NULL transition and double-notify.
    let was_deactivated = sqlx::query_scalar!(
        r#"SELECT deactivated_at FROM snake_health_status
         WHERE battlesnake_id = $1
         FOR UPDATE"#,
        battlesnake_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock snake health status")?
    .flatten()
    .is_some();

    sqlx::query!(
        r#"UPDATE leaderboard_entries
         SET disabled_at = NULL, disabled_reason = NULL, updated_at = NOW()
         WHERE battlesnake_id = $1 AND disabled_reason = $2"#,
        battlesnake_id,
        DISABLED_REASON_HEALTH
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to re-enable leaderboard entries")?;

    sqlx::query!(
        r#"UPDATE snake_health_status
         SET deactivated_at = NULL, consecutive_failures = 0, consecutive_successes = 0, last_failure = NULL, updated_at = NOW()
         WHERE battlesnake_id = $1"#,
        battlesnake_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to reset snake health status")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit reactivation")?;

    Ok(was_deactivated)
}

/// The owner's best notification address: their GitHub email when present,
/// otherwise the email of a play account they claimed (migrated users often
/// have no public GitHub email but always had a play address).
pub async fn owner_notification_email(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Option<String>> {
    let email = sqlx::query_scalar!(
        r#"SELECT COALESCE(u.github_email, ia.email) as "email?"
         FROM battlesnakes b
         JOIN users u ON b.user_id = u.user_id
         LEFT JOIN imported_accounts ia ON ia.claimed_by_user_id = u.user_id
         WHERE b.battlesnake_id = $1
         ORDER BY ia.is_email_verified DESC NULLS LAST
         LIMIT 1"#,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to look up owner email")?
    .flatten();

    Ok(email)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::leaderboard;

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
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

    async fn create_leaderboard_with_entry(
        pool: &PgPool,
        battlesnake_id: Uuid,
        name: &str,
    ) -> cja::Result<Uuid> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            name,
        )
        .fetch_one(pool)
        .await?;
        let entry = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id).await?;
        Ok(entry.leaderboard_entry_id)
    }

    async fn entry_state(pool: &PgPool, entry_id: Uuid) -> cja::Result<(bool, Option<String>)> {
        let row = sqlx::query!(
            "SELECT disabled_at IS NOT NULL as \"disabled!\", disabled_reason
             FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
            entry_id,
        )
        .fetch_one(pool)
        .await?;
        Ok((row.disabled, row.disabled_reason))
    }

    /// Push a snake's `last_checked_at` back past the failure-count spacing
    /// gate, simulating the passage of a real sweep interval.
    async fn age_last_check(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            "UPDATE snake_health_status
             SET last_checked_at = last_checked_at - INTERVAL '16 minutes'
             WHERE battlesnake_id = $1",
            battlesnake_id,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failure_streak_increments_and_success_resets(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9001).await?;
        let snake_id = create_snake(&pool, user_id, "streaky").await?;

        assert_eq!(record_failure(&pool, snake_id, "GET /: timeout").await?, 1);

        // Piled-up or overlapping sweeps probe again within minutes: the
        // streak must not inflate (it means "N separate sweep windows").
        assert_eq!(record_failure(&pool, snake_id, "GET /: refused").await?, 1);

        // A real next sweep (past the spacing gate) does count.
        age_last_check(&pool, snake_id).await?;
        assert_eq!(record_failure(&pool, snake_id, "GET /: refused").await?, 2);

        let status = get(&pool, snake_id).await?.expect("row exists");
        assert_eq!(status.consecutive_failures, 2);
        assert_eq!(status.last_failure.as_deref(), Some("GET /: refused"));
        assert!(status.deactivated_at.is_none());

        record_success(&pool, snake_id).await?;
        let status = get(&pool, snake_id).await?.expect("row exists");
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_failure.is_none());

        // The streak really is consecutive: a success starts it over — and a
        // failure right after a success still registers as 1, gate or not.
        assert_eq!(record_failure(&pool, snake_id, "boom").await?, 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deactivate_disables_entries_and_gates_notification_to_once(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 9002).await?;
        let snake_id = create_snake(&pool, user_id, "dead").await?;
        let entry_a = create_leaderboard_with_entry(&pool, snake_id, "standard").await?;
        let entry_b = create_leaderboard_with_entry(&pool, snake_id, "royale").await?;

        record_failure(&pool, snake_id, "POST /move: timeout").await?;

        // First deactivation wins the CAS: this is the one that notifies.
        assert!(deactivate(&pool, snake_id).await?);
        assert_eq!(
            entry_state(&pool, entry_a).await?,
            (true, Some(DISABLED_REASON_HEALTH.to_string()))
        );
        assert_eq!(
            entry_state(&pool, entry_b).await?,
            (true, Some(DISABLED_REASON_HEALTH.to_string()))
        );

        // A retried job (or duplicate enqueue) must not notify again.
        assert!(!deactivate(&pool, snake_id).await?);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reactivate_restores_health_entries_but_not_manual_pauses(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 9003).await?;
        let snake_id = create_snake(&pool, user_id, "recovering").await?;
        let entry_active = create_leaderboard_with_entry(&pool, snake_id, "standard").await?;
        let entry_paused = create_leaderboard_with_entry(&pool, snake_id, "royale").await?;

        // Owner manually paused one entry before the snake broke.
        leaderboard::set_disabled(&pool, entry_paused, Some(chrono::Utc::now())).await?;

        record_failure(&pool, snake_id, "POST /move: timeout").await?;
        assert!(deactivate(&pool, snake_id).await?);

        // The sweeper only touched the enabled entry; the manual pause kept
        // its NULL reason.
        assert_eq!(entry_state(&pool, entry_paused).await?, (true, None));

        reactivate(&pool, snake_id).await?;

        assert_eq!(entry_state(&pool, entry_active).await?, (false, None));
        // Manual pause survives recovery.
        assert_eq!(entry_state(&pool, entry_paused).await?, (true, None));

        let status = get(&pool, snake_id).await?.expect("row exists");
        assert!(status.deactivated_at.is_none());
        assert_eq!(status.consecutive_failures, 0);

        // The full cycle can repeat: a fresh breakage notifies again.
        record_failure(&pool, snake_id, "still broken").await?;
        assert!(deactivate(&pool, snake_id).await?);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn manual_resume_clears_health_reason(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9004).await?;
        let snake_id = create_snake(&pool, user_id, "rejoiner").await?;
        let entry = create_leaderboard_with_entry(&pool, snake_id, "standard").await?;

        record_failure(&pool, snake_id, "down").await?;
        assert!(deactivate(&pool, snake_id).await?);
        assert_eq!(
            entry_state(&pool, entry).await?,
            (true, Some(DISABLED_REASON_HEALTH.to_string()))
        );

        // Resuming through the normal pause/resume path also clears the
        // sweeper's marker — the owner has taken over.
        leaderboard::set_disabled(&pool, entry, None).await?;
        assert_eq!(entry_state(&pool, entry).await?, (false, None));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn healthy_probe_after_out_of_band_resume_rearms_the_email_gate(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 9006).await?;
        let snake_id = create_snake(&pool, user_id, "wanderer").await?;
        let entry = create_leaderboard_with_entry(&pool, snake_id, "standard").await?;

        record_failure(&pool, snake_id, "down").await?;
        assert!(deactivate(&pool, snake_id).await?);

        // Owner resumes through the leaderboard page (not the banner
        // button), which knows nothing about health state…
        leaderboard::set_disabled(&pool, entry, None).await?;

        // …so the next healthy sweep is what clears the stale stamp.
        record_success(&pool, snake_id).await?;
        let status = get(&pool, snake_id).await?.expect("row exists");
        assert!(status.deactivated_at.is_none());

        // Which means a future genuine breakage notifies again instead of
        // being swallowed by a once-per-lifetime gate.
        record_failure(&pool, snake_id, "down again").await?;
        assert!(deactivate(&pool, snake_id).await?);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owner_email_prefers_github_and_falls_back_to_claimed_play_account(
        pool: PgPool,
    ) -> cja::Result<()> {
        // No email anywhere -> None.
        let user_a = create_user(&pool, 9005).await?;
        let snake_a = create_snake(&pool, user_a, "no-email").await?;
        assert_eq!(owner_notification_email(&pool, snake_a).await?, None);

        // A claimed play account supplies the fallback address.
        sqlx::query!(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, is_email_verified, claimed_by_user_id)
             VALUES ('usr_h1', 'act_h1', 'play@example.com', 'player', true, $1)",
            user_a,
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            owner_notification_email(&pool, snake_a).await?.as_deref(),
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
            owner_notification_email(&pool, snake_a).await?.as_deref(),
            Some("gh@example.com")
        );

        Ok(())
    }
}
