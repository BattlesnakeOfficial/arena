//! Per-leaderboard-entry health state for the matchmaking sweeper (BS-3534,
//! DEV-1515).
//!
//! The sweeper probes an entry with a test game shaped like its leaderboard,
//! so a snake that only breaks on Royale fails only its Royale entry.
//! `health_consecutive_failures` climbs on failed probes and resets on any
//! pass; when it crosses the configured threshold the sweeper pulls just that
//! entry with `disabled_reason = 'health'`. Health-paused entries keep getting
//! probed: `health_consecutive_successes` climbs on passing probes and the
//! sweeper puts the entry back once it crosses the recovery threshold.
//!
//! The entry row itself is the deactivation marker, and every transition is
//! one guarded `UPDATE` on it: [`deactivate`] only flips an enabled entry,
//! [`reactivate`] only a health-paused one, and each returns `true` only for
//! the call that actually flipped it. That is what gates the owner emails to
//! once per transition no matter how often the job retries, and why there's no
//! second "deactivated" stamp to drift out of sync (the DEV-710 bugs).

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// `disabled_reason` value the sweeper writes on entries it disables. Manual
/// pauses leave the reason NULL.
pub const DISABLED_REASON_HEALTH: &str = "health";

/// Minimum spacing between counted probes (failures and recovery successes
/// alike). The cron enqueues a sweep every 30 minutes with no dedup or
/// completion check, so sweeps can pile up and drain back-to-back (or
/// overlap with `workers > 1`); without this gate an entry down for one
/// short window could burn its whole failure budget in minutes — or bounce
/// back into matchmaking off a burst of piled-up probes. Half the sweep
/// interval: real consecutive sweeps always count, piled-up ones don't.
const PROBE_COUNT_SPACING_MINUTES: i32 = 15;

/// One of a snake's entries the sweeper pulled from matchmaking, for the
/// owner-facing profile banner.
#[derive(Debug, Clone)]
pub struct HealthPausedEntry {
    pub leaderboard_entry_id: Uuid,
    pub leaderboard_name: String,
    pub paused_at: DateTime<Utc>,
    pub consecutive_failures: i32,
    pub last_failure: Option<String>,
}

/// Record a passing probe of an entry in matchmaking: the failure streak
/// starts over. `checked_at` is when the sweep started (the evidence cursor:
/// bad turns after it will be looked at next sweep).
pub async fn record_success(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    checked_at: DateTime<Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"UPDATE leaderboard_entries
         SET health_consecutive_failures = 0,
             health_last_failure = NULL,
             health_last_checked_at = $2
         WHERE leaderboard_entry_id = $1 AND disabled_at IS NULL"#,
        leaderboard_entry_id,
        checked_at,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record entry health success")?;

    Ok(())
}

/// Record a failed probe of an entry in matchmaking and return the
/// consecutive-failure count, or `None` if the entry left matchmaking
/// meanwhile (the owner paused it).
///
/// The count only increments when the previous check is at least
/// [`PROBE_COUNT_SPACING_MINUTES`] older — back-to-back probes from piled-up
/// or concurrent sweeps update the failure text but don't inflate the
/// streak, keeping "N consecutive failures" meaning N separate sweep windows
/// (~N × 30 min), as the threshold design assumes.
pub async fn record_failure(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    failure_summary: &str,
    checked_at: DateTime<Utc>,
) -> cja::Result<Option<i32>> {
    sqlx::query_scalar!(
        r#"UPDATE leaderboard_entries
         SET health_consecutive_failures = CASE
                 WHEN health_last_checked_at IS NULL
                   OR health_last_checked_at <= $3::timestamptz - make_interval(mins => $4)
                 THEN health_consecutive_failures + 1
                 ELSE GREATEST(health_consecutive_failures, 1)
             END,
             health_last_failure = $2,
             health_last_checked_at = $3
         WHERE leaderboard_entry_id = $1 AND disabled_at IS NULL
         RETURNING health_consecutive_failures"#,
        leaderboard_entry_id,
        failure_summary,
        checked_at,
        PROBE_COUNT_SPACING_MINUTES,
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to record entry health failure")
}

/// Record a passing probe of a health-paused entry and return the
/// consecutive-success count, or `None` if the entry isn't health-paused any
/// more (the owner resumed it meanwhile). Spacing-gated like
/// [`record_failure`], so piled-up sweeps can't fake a recovery in minutes.
pub async fn record_recovery_success(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    checked_at: DateTime<Utc>,
) -> cja::Result<Option<i32>> {
    sqlx::query_scalar!(
        r#"UPDATE leaderboard_entries
         SET health_consecutive_successes = CASE
                 WHEN health_last_checked_at IS NULL
                   OR health_last_checked_at <= $2::timestamptz - make_interval(mins => $3)
                 THEN health_consecutive_successes + 1
                 ELSE GREATEST(health_consecutive_successes, 1)
             END,
             health_last_checked_at = $2
         WHERE leaderboard_entry_id = $1 AND disabled_reason = $4
         RETURNING health_consecutive_successes"#,
        leaderboard_entry_id,
        checked_at,
        PROBE_COUNT_SPACING_MINUTES,
        DISABLED_REASON_HEALTH,
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to record entry recovery success")
}

/// Record a failed probe of a health-paused entry: the recovery streak
/// starts over. No spacing gate — any failure genuinely breaks the streak.
pub async fn record_recovery_failure(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    failure_summary: &str,
    checked_at: DateTime<Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"UPDATE leaderboard_entries
         SET health_consecutive_successes = 0,
             health_last_failure = $2,
             health_last_checked_at = $3
         WHERE leaderboard_entry_id = $1 AND disabled_reason = $4"#,
        leaderboard_entry_id,
        failure_summary,
        checked_at,
        DISABLED_REASON_HEALTH,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record entry recovery failure")?;

    Ok(())
}

/// Pull an entry from matchmaking (`disabled_reason = 'health'`) and start
/// its recovery streak from zero.
///
/// Returns `true` only when this call performed the enabled -> paused
/// transition; the caller emails the owner exactly on that `true`, which
/// keeps a re-entrant job from emailing twice. An entry that's already
/// disabled (by the owner, or a deleted snake) is left alone.
pub async fn deactivate(pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<bool> {
    let flipped = sqlx::query_scalar!(
        r#"UPDATE leaderboard_entries
         SET disabled_at = NOW(),
             disabled_reason = $2,
             health_consecutive_successes = 0,
             updated_at = NOW()
         WHERE leaderboard_entry_id = $1 AND disabled_at IS NULL
         RETURNING leaderboard_entry_id"#,
        leaderboard_entry_id,
        DISABLED_REASON_HEALTH
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to pull leaderboard entry from matchmaking")?;

    Ok(flipped.is_some())
}

/// Put a health-paused entry back into matchmaking with both streaks reset,
/// so the next sweep starts fresh. A manual pause is never touched.
///
/// Returns `true` only when this call performed the paused -> enabled
/// transition; the sweeper sends the "back in matchmaking" email exactly on
/// that `true`.
pub async fn reactivate(pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<bool> {
    let flipped = sqlx::query_scalar!(
        r#"UPDATE leaderboard_entries
         SET disabled_at = NULL,
             disabled_reason = NULL,
             health_consecutive_failures = 0,
             health_consecutive_successes = 0,
             health_last_failure = NULL,
             updated_at = NOW()
         WHERE leaderboard_entry_id = $1 AND disabled_reason = $2
         RETURNING leaderboard_entry_id"#,
        leaderboard_entry_id,
        DISABLED_REASON_HEALTH
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to put leaderboard entry back into matchmaking")?;

    Ok(flipped.is_some())
}

/// The owner's Resume Matchmaking: [`reactivate`] every one of the snake's
/// health-paused entries. Returns how many were resumed.
pub async fn reactivate_snake(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<u64> {
    let result = sqlx::query!(
        r#"UPDATE leaderboard_entries
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
    .execute(pool)
    .await
    .wrap_err("Failed to put snake back into matchmaking")?;

    Ok(result.rows_affected())
}

/// A snake's entries the sweeper pulled from matchmaking, on leaderboards
/// that are still running, oldest pause first.
pub async fn health_paused_entries(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<HealthPausedEntry>> {
    sqlx::query_as!(
        HealthPausedEntry,
        r#"SELECT le.leaderboard_entry_id,
                  l.name AS leaderboard_name,
                  le.disabled_at AS "paused_at!",
                  le.health_consecutive_failures AS consecutive_failures,
                  le.health_last_failure AS last_failure
         FROM leaderboard_entries le
         JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
         WHERE le.battlesnake_id = $1
           AND le.disabled_reason = $2
           AND le.disabled_at IS NOT NULL
           AND l.disabled_at IS NULL
         ORDER BY le.disabled_at, l.name"#,
        battlesnake_id,
        DISABLED_REASON_HEALTH
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch health-paused entries")
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
           AND b.deleted_at IS NULL
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
    use chrono::Duration;

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

    async fn create_entry(pool: &PgPool, battlesnake_id: Uuid, board: &str) -> cja::Result<Uuid> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            board,
        )
        .fetch_one(pool)
        .await?;
        let entry = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id).await?;
        Ok(entry.leaderboard_entry_id)
    }

    struct EntryState {
        disabled: bool,
        reason: Option<String>,
        failures: i32,
        successes: i32,
        last_failure: Option<String>,
    }

    async fn state(pool: &PgPool, entry_id: Uuid) -> cja::Result<EntryState> {
        let row = sqlx::query!(
            r#"SELECT disabled_at IS NOT NULL AS "disabled!", disabled_reason,
                      health_consecutive_failures, health_consecutive_successes,
                      health_last_failure
             FROM leaderboard_entries WHERE leaderboard_entry_id = $1"#,
            entry_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(EntryState {
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
        let entry = create_entry(&pool, snake_id, "standard").await?;
        let t0 = Utc::now();

        assert_eq!(record_failure(&pool, entry, "timeout", t0).await?, Some(1));
        // A piled-up sweep minutes later must not inflate the streak.
        let piled = t0 + Duration::minutes(2);
        assert_eq!(
            record_failure(&pool, entry, "refused", piled).await?,
            Some(1)
        );
        assert_eq!(
            state(&pool, entry).await?.last_failure.as_deref(),
            Some("refused")
        );
        // A real next sweep does count.
        let t1 = next_sweep(piled);
        assert_eq!(record_failure(&pool, entry, "refused", t1).await?, Some(2));

        record_success(&pool, entry, next_sweep(t1)).await?;
        let after = state(&pool, entry).await?;
        assert_eq!(after.failures, 0);
        assert!(after.last_failure.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn probes_of_a_paused_entry_never_count(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9002).await?;
        let snake_id = create_snake(&pool, user_id, "paused").await?;
        let entry = create_entry(&pool, snake_id, "standard").await?;
        leaderboard::set_disabled(&pool, entry, Some(Utc::now())).await?;

        let now = Utc::now();
        assert_eq!(record_failure(&pool, entry, "down", now).await?, None);
        assert_eq!(record_recovery_success(&pool, entry, now).await?, None);
        assert!(!deactivate(&pool, entry).await?);
        let after = state(&pool, entry).await?;
        assert_eq!((after.failures, after.successes), (0, 0));
        assert_eq!(after.reason, None, "a manual pause stays a manual pause");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deactivate_pulls_only_that_entry_and_notifies_once(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9003).await?;
        let snake_id = create_snake(&pool, user_id, "royale-crasher").await?;
        let royale = create_entry(&pool, snake_id, "royale").await?;
        let duels = create_entry(&pool, snake_id, "duels").await?;

        assert!(deactivate(&pool, royale).await?);
        assert!(
            !deactivate(&pool, royale).await?,
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
        let entry = create_entry(&pool, snake_id, "standard").await?;
        let t0 = Utc::now();
        record_failure(&pool, entry, "down", t0).await?;
        assert!(deactivate(&pool, entry).await?);

        let t1 = next_sweep(t0);
        assert_eq!(record_recovery_success(&pool, entry, t1).await?, Some(1));
        let piled = t1 + Duration::minutes(1);
        assert_eq!(record_recovery_success(&pool, entry, piled).await?, Some(1));
        let t2 = next_sweep(piled);
        assert_eq!(record_recovery_success(&pool, entry, t2).await?, Some(2));
        record_recovery_failure(&pool, entry, "down again", next_sweep(t2)).await?;
        assert_eq!(state(&pool, entry).await?.successes, 0);

        assert!(reactivate(&pool, entry).await?);
        assert!(!reactivate(&pool, entry).await?, "only one call flips it");
        let after = state(&pool, entry).await?;
        assert!(!after.disabled);
        assert_eq!((after.failures, after.successes), (0, 0));
        assert!(after.last_failure.is_none());

        // Owner resumed meanwhile: a late recovery probe doesn't count.
        assert_eq!(
            record_recovery_success(&pool, entry, Utc::now()).await?,
            None
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reactivate_never_touches_manual_pauses(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9005).await?;
        let snake_id = create_snake(&pool, user_id, "mixed").await?;
        let pulled = create_entry(&pool, snake_id, "standard").await?;
        let paused = create_entry(&pool, snake_id, "royale").await?;
        leaderboard::set_disabled(&pool, paused, Some(Utc::now())).await?;
        record_failure(&pool, pulled, "down", Utc::now()).await?;
        assert!(deactivate(&pool, pulled).await?);

        assert_eq!(health_paused_entries(&pool, snake_id).await?.len(), 1);
        assert!(!reactivate(&pool, paused).await?);
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
        let entry = create_entry(&pool, snake_id, "standard").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
            entry
        )
        .fetch_one(&pool)
        .await?;

        record_failure(&pool, entry, "down", Utc::now()).await?;
        leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_id).await?;
        assert_eq!(
            state(&pool, entry).await?.failures,
            1,
            "already in rotation"
        );

        assert!(deactivate(&pool, entry).await?);
        leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_id).await?;
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
        let live = create_entry(&pool, snake_id, "live").await?;
        let retired = create_entry(&pool, snake_id, "retired").await?;
        for entry in [live, retired] {
            record_failure(&pool, entry, "POST /move (turn 0): boom", Utc::now()).await?;
            assert!(deactivate(&pool, entry).await?);
        }
        sqlx::query!("UPDATE leaderboards SET disabled_at = NOW() WHERE name = 'retired'")
            .execute(&pool)
            .await?;

        let paused = health_paused_entries(&pool, snake_id).await?;
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].leaderboard_entry_id, live);
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
