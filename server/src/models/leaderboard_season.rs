//! Leaderboard seasons: dated windows a league's ratings are scoped to.
//!
//! The active season is derived, never stored: `starts_at <= now < ends_at`
//! (half-open; an open-ended season has `ends_at IS NULL`). The
//! `leaderboard_seasons_no_overlap` exclusion constraint guarantees at most one
//! active season per league. Predicates use `clock_timestamp()` rather than
//! `now()` so a long transaction (or one that waited on a lock) still sees
//! the wall clock, not its start time.

use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LeaderboardSeason {
    pub season_id: Uuid,
    pub leaderboard_id: Uuid,
    pub name: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The league's active season, or `None` during a gap between seasons.
pub async fn get_active_season<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
) -> cja::Result<Option<LeaderboardSeason>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as!(
        LeaderboardSeason,
        r#"SELECT season_id, leaderboard_id, name, starts_at, ends_at
         FROM leaderboard_seasons
         WHERE leaderboard_id = $1
           AND starts_at <= clock_timestamp()
           AND (ends_at IS NULL OR clock_timestamp() < ends_at)"#,
        leaderboard_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch active leaderboard season")
}

/// [`get_active_season`], locking the season row `FOR SHARE` so an
/// admin can't end (or move) it until the caller's transaction commits.
///
/// When the lock had to wait, Postgres re-checks only this row's predicates
/// against the committed version, and `clock_timestamp()` is re-evaluated,
/// so a season that ended while we waited comes back as `None`.
pub async fn lock_active_season<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
) -> cja::Result<Option<LeaderboardSeason>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as!(
        LeaderboardSeason,
        r#"SELECT season_id, leaderboard_id, name, starts_at, ends_at
         FROM leaderboard_seasons
         WHERE leaderboard_id = $1
           AND starts_at <= clock_timestamp()
           AND (ends_at IS NULL OR clock_timestamp() < ends_at)
         FOR SHARE"#,
        leaderboard_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to lock active leaderboard season")
}
