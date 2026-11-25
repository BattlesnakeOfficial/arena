use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres};
use uuid::Uuid;

use crate::models::{
    battlesnake::Visibility, leaderboard_registration, leaderboard_season, user::User,
};

/// Application constants for leaderboard configuration
/// Smallest game the matchmaker will create when fewer than a leaderboard's match size
/// snakes are enabled. Below this the ladder is starved and matchmaking
/// pauses entirely.
pub const MIN_MATCH_SIZE: usize = 2;
pub const MIN_GAMES_FOR_RANKING: i32 = 10;

// Leaderboard model
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Leaderboard {
    pub leaderboard_id: Uuid,
    pub name: String,
    pub game_type: String,
    pub board_size: String,
    pub match_size: i32,
    /// Private leaderboards are unlisted: reachable by direct link only, and
    /// joining takes an invite code (see `models::private_leaderboard`).
    pub visibility: Visibility,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// Leaderboard entry: one per snake per leaderboard
#[derive(Debug, Serialize, Deserialize, Clone, FromRow)]
pub struct LeaderboardEntry {
    pub leaderboard_entry_id: Uuid,
    pub leaderboard_id: Uuid,
    /// The season this entry's ratings belong to.
    pub season_id: Uuid,
    pub battlesnake_id: Uuid,
    pub mu: f64,
    pub sigma: f64,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled_reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// Leaderboard game: links a game to a leaderboard
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardGame {
    pub leaderboard_game_id: Uuid,
    pub leaderboard_id: Uuid,
    pub game_id: Uuid,
    /// The season the game was created in (resolved at INSERT, immutable).
    pub season_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// Leaderboard game result: per-snake rating change audit trail
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardGameResult {
    pub leaderboard_game_result_id: Uuid,
    pub leaderboard_game_id: Uuid,
    pub leaderboard_entry_id: Uuid,
    pub placement: i32,
    pub mu_before: f64,
    pub mu_after: f64,
    pub sigma_before: f64,
    pub sigma_after: f64,
    pub display_score_change: f64,
    pub food_eaten: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// Ranked entry with snake and owner info for display
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct RankedEntry {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    /// The snake owner's user id. Internal (used by the tournament import
    /// flow to enforce per-user caps) — not exposed in API responses.
    #[serde(skip)]
    pub user_id: Uuid,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub mu: f64,
    pub sigma: f64,
    /// `None` when the snake is soft-deleted (tombstoned on an ended
    /// season's frozen standings) — see `deleted`.
    pub snake_name: Option<String>,
    pub snake_color: Option<String>,
    /// The snake's declared head/tail cosmetic slugs ('' when never
    /// customized). Display-only; unknown slugs render as the default shape.
    #[serde(skip)]
    pub snake_head: Option<String>,
    #[serde(skip)]
    pub snake_tail: Option<String>,
    /// Owner's GitHub login — the `/users/{login}` URL key, not display text.
    pub owner_login: Option<String>,
    /// Owner's public name: `display_name` when set, else the GitHub login.
    pub owner_name: Option<String>,
    /// Owner's ISO-3166-1 alpha-2 country ('' when unset). Display-only.
    #[serde(skip)]
    pub owner_country: Option<String>,
    /// The snake is soft-deleted: the identity fields above are NULL but the
    /// row (and its rank) stays as a tombstone.
    pub deleted: bool,
}

/// Sort order for ranked leaderboard entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderboardSort {
    #[default]
    Rating,
    FoodEaten,
}

impl LeaderboardSort {
    pub fn as_str(&self) -> &'static str {
        match self {
            LeaderboardSort::Rating => "rating",
            LeaderboardSort::FoodEaten => "food_eaten",
        }
    }
}

// --- Leaderboard queries ---

/// Every public leaderboard, inactive ones included. Private leaderboards are
/// unlisted, so anything that lists ladders to users goes through here.
pub async fn get_public_leaderboards(pool: &PgPool) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as!(
        Leaderboard,
        r#"SELECT leaderboard_id, name, game_type, board_size, match_size,
                visibility as "visibility: Visibility",
                disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE visibility = 'public'
         ORDER BY created_at ASC, name ASC"#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch public leaderboards")?;

    Ok(rows)
}

/// Every active leaderboard, private ones included. For background work
/// (matchmaking, health sweeps) that must treat private ladders like any
/// other — never for anything a user sees as a list.
pub async fn get_active_leaderboards(pool: &PgPool) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as!(
        Leaderboard,
        r#"SELECT leaderboard_id, name, game_type, board_size, match_size,
                visibility as "visibility: Visibility",
                disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE disabled_at IS NULL
         ORDER BY created_at ASC, name ASC"#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch active leaderboards")?;

    Ok(rows)
}

/// Active public leaderboards: the user-facing subset of
/// [`get_active_leaderboards`].
pub async fn get_active_public_leaderboards(pool: &PgPool) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as!(
        Leaderboard,
        r#"SELECT leaderboard_id, name, game_type, board_size, match_size,
                visibility as "visibility: Visibility",
                disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE disabled_at IS NULL AND visibility = 'public'
         ORDER BY created_at ASC, name ASC"#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch active public leaderboards")?;

    Ok(rows)
}

/// Active leaderboards to Test Snake against: every active public ladder plus
/// the private ladders this snake is on. Test Snake is owner-only and the
/// owner is a member of those, so nothing leaks — and a snake the health
/// sweeper paused on a private ladder's shape can reproduce the failure.
pub async fn get_active_leaderboards_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as!(
        Leaderboard,
        r#"SELECT l.leaderboard_id, l.name, l.game_type, l.board_size, l.match_size,
                l.visibility as "visibility: Visibility",
                l.disabled_at, l.created_at, l.updated_at
         FROM leaderboards l
         WHERE l.disabled_at IS NULL
           AND (l.visibility = 'public' OR EXISTS (
                SELECT 1 FROM leaderboard_registrations r
                WHERE r.leaderboard_id = l.leaderboard_id AND r.battlesnake_id = $1
           ))
         ORDER BY l.created_at ASC, l.name ASC"#,
        battlesnake_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboards for battlesnake")?;

    Ok(rows)
}

pub async fn get_leaderboard_by_id(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Option<Leaderboard>> {
    let row = sqlx::query_as!(
        Leaderboard,
        r#"SELECT leaderboard_id, name, game_type, board_size, match_size,
                visibility as "visibility: Visibility",
                disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE leaderboard_id = $1"#,
        leaderboard_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard")?;

    Ok(row)
}

/// The public leaderboard named `name` (oldest first if somehow repeated).
/// Names, not UUIDs, are what stay the same across prod, dev and tests: the
/// seeded ladders ("Standard 11x11", ...) get random ids per database.
pub async fn public_leaderboard_id_by_name(pool: &PgPool, name: &str) -> cja::Result<Option<Uuid>> {
    sqlx::query_scalar!(
        "SELECT leaderboard_id FROM leaderboards
         WHERE name = $1 AND visibility = 'public'
         ORDER BY created_at
         LIMIT 1",
        name
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to look up leaderboard by name")
}

// --- Leaderboard entry queries ---

/// Opt-in a snake to a leaderboard and return its current-season entry.
///
/// Registers the snake in the league
/// ([`leaderboard_registration::get_or_create_registration`], which carries
/// the join semantics: re-enables a paused registration, never a
/// `'deleted'`/`'retired'` one), then lazily creates the active season's entry
/// ([`ensure_snake_season_entry`]). `Ok(None)` = the snake is missing,
/// deleted or retired, the registration is terminal, or the league has no
/// active season (a gap: the registration still exists).
pub async fn get_or_create_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let registration =
        leaderboard_registration::get_or_create_registration(pool, leaderboard_id, battlesnake_id)
            .await?;
    if registration.is_none() {
        return Ok(None);
    }
    ensure_snake_season_entry(
        pool,
        &crate::scoring::default_registry(),
        leaderboard_id,
        battlesnake_id,
    )
    .await
}

/// Lazily create one snake's entry for the league's active season, at
/// default ratings, and (re)initialize every scoring algorithm's row for it.
///
/// Idempotent: gated on an active registration and a live snake, and
/// `ON CONFLICT (season_id, battlesnake_id) DO NOTHING`. `Ok(None)` = no
/// active season, or the registration is paused / the snake is dead.
pub async fn ensure_snake_season_entry(
    pool: &PgPool,
    scoring: &crate::scoring::ScoringRegistry,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let Some(season) = leaderboard_season::get_active_season(pool, leaderboard_id).await? else {
        return Ok(None);
    };
    // One row per conflict key by construction (a single registration). Keep
    // it that way: a repeated key would turn any future DO UPDATE into a
    // whole-batch abort.
    sqlx::query!(
        r#"INSERT INTO leaderboard_entries (leaderboard_id, season_id, battlesnake_id)
         SELECT $1, $2, b.battlesnake_id
         FROM leaderboard_registrations r
         JOIN battlesnakes b ON b.battlesnake_id = r.battlesnake_id
         WHERE r.leaderboard_id = $1 AND r.battlesnake_id = $3
           AND r.disabled_at IS NULL
           AND b.deleted_at IS NULL AND b.retired_at IS NULL
         ON CONFLICT (season_id, battlesnake_id) DO NOTHING"#,
        leaderboard_id,
        season.season_id,
        battlesnake_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to create season leaderboard entry")?;

    let entry = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            le.leaderboard_entry_id, le.leaderboard_id, le.season_id AS "season_id!", le.battlesnake_id,
            le.mu, le.sigma, le.display_score, le.games_played, le.first_place_finishes, le.non_first_finishes,
            le.disabled_at, le.disabled_reason, le.created_at, le.updated_at
         FROM leaderboard_entries le
         JOIN leaderboard_registrations r
           ON r.leaderboard_id = le.leaderboard_id AND r.battlesnake_id = le.battlesnake_id
         JOIN battlesnakes b ON b.battlesnake_id = le.battlesnake_id
         WHERE le.season_id = $1 AND le.battlesnake_id = $2
           AND r.disabled_at IS NULL
           AND b.deleted_at IS NULL AND b.retired_at IS NULL"#,
        season.season_id,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch season leaderboard entry")?;

    let Some(entry) = entry else {
        return Ok(None);
    };
    for algo in scoring.algorithms() {
        algo.initialize_entry(pool, entry.leaderboard_entry_id)
            .await
            .wrap_err_with(|| format!("Failed to initialize {} for entry", algo.key()))?;
    }
    Ok(Some(entry))
}

/// Lazily create the active season's entry for every active registration of
/// the league (live snakes only), then (re)initialize every scoring
/// algorithm's row for every active-season entry — idempotent, so it also
/// heals a crash between the INSERT and the initialization.
///
/// No-op during a gap.
pub async fn ensure_active_season_entries(
    pool: &PgPool,
    scoring: &crate::scoring::ScoringRegistry,
    leaderboard_id: Uuid,
) -> cja::Result<()> {
    let Some(season) = leaderboard_season::get_active_season(pool, leaderboard_id).await? else {
        return Ok(());
    };
    // One row per conflict key by construction: registrations are unique per
    // (league, snake) and the season is fixed. Keep it that way: a repeated
    // key would turn any future DO UPDATE into a whole-batch abort.
    sqlx::query!(
        r#"INSERT INTO leaderboard_entries (leaderboard_id, season_id, battlesnake_id)
         SELECT $1, $2, b.battlesnake_id
         FROM leaderboard_registrations r
         JOIN battlesnakes b ON b.battlesnake_id = r.battlesnake_id
         WHERE r.leaderboard_id = $1
           AND r.disabled_at IS NULL
           AND b.deleted_at IS NULL AND b.retired_at IS NULL
         ON CONFLICT (season_id, battlesnake_id) DO NOTHING"#,
        leaderboard_id,
        season.season_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to create season leaderboard entries")?;

    for entry in get_active_entries(pool, leaderboard_id).await? {
        for algo in scoring.algorithms() {
            algo.initialize_entry(pool, entry.leaderboard_entry_id)
                .await
                .wrap_err_with(|| format!("Failed to initialize {} for entry", algo.key()))?;
        }
    }
    Ok(())
}

/// The active season's entries whose registration is in rotation and whose
/// snake is live — the matchmaking pool. Empty during a gap.
pub async fn get_active_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Vec<LeaderboardEntry>> {
    let entries = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            le.leaderboard_entry_id, le.leaderboard_id, le.season_id AS "season_id!", le.battlesnake_id,
            le.mu, le.sigma, le.display_score, le.games_played, le.first_place_finishes, le.non_first_finishes,
            le.disabled_at, le.disabled_reason, le.created_at, le.updated_at
         FROM leaderboard_entries le
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         JOIN leaderboard_registrations r
           ON r.leaderboard_id = le.leaderboard_id AND r.battlesnake_id = le.battlesnake_id
         JOIN battlesnakes b ON b.battlesnake_id = le.battlesnake_id
         WHERE le.leaderboard_id = $1
           AND s.starts_at <= clock_timestamp()
           AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)
           AND r.disabled_at IS NULL
           AND b.deleted_at IS NULL AND b.retired_at IS NULL
         ORDER BY le.display_score DESC"#,
        leaderboard_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch active leaderboard entries")?;

    Ok(entries)
}

/// Get ranked entries (only snakes with enough games) with snake/owner info
pub async fn get_ranked_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    sort: LeaderboardSort,
) -> cja::Result<Vec<RankedEntry>> {
    let entries = match sort {
        LeaderboardSort::Rating => {
            sqlx::query_as!(
                RankedEntry,
                r#"SELECT
                    le.leaderboard_entry_id,
                    le.battlesnake_id,
                    b.user_id,
                    le.display_score,
                    le.games_played,
                    le.first_place_finishes,
                    le.non_first_finishes,
                    le.mu,
                    le.sigma,
                    (b.deleted_at IS NOT NULL) AS "deleted!",
                    CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
                    CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
                    CASE WHEN b.deleted_at IS NULL THEN b.head END AS "snake_head?",
                    CASE WHEN b.deleted_at IS NULL THEN b.tail END AS "snake_tail?",
                    CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
                    CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
                    CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
                 FROM leaderboard_entries le
                 JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
                 JOIN users u ON b.user_id = u.user_id
                 JOIN leaderboard_seasons s ON s.season_id = le.season_id
                 JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
                 WHERE le.leaderboard_id = $1 AND s.season_id = $2
                   AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                        OR lr.disabled_at IS NULL)
                   AND le.games_played >= $3
                 ORDER BY le.display_score DESC
                 LIMIT 100"#,
                leaderboard_id,
                season_id,
                MIN_GAMES_FOR_RANKING
            )
            .fetch_all(pool)
            .await
        }
        LeaderboardSort::FoodEaten => {
            sqlx::query_as!(
                RankedEntry,
                r#"SELECT
                    le.leaderboard_entry_id,
                    le.battlesnake_id,
                    b.user_id,
                    le.display_score,
                    le.games_played,
                    le.first_place_finishes,
                    le.non_first_finishes,
                    le.mu,
                    le.sigma,
                    (b.deleted_at IS NOT NULL) AS "deleted!",
                    CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
                    CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
                    CASE WHEN b.deleted_at IS NULL THEN b.head END AS "snake_head?",
                    CASE WHEN b.deleted_at IS NULL THEN b.tail END AS "snake_tail?",
                    CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
                    CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
                    CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
                 FROM leaderboard_entries le
                 JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
                 JOIN users u ON b.user_id = u.user_id
                 LEFT JOIN food_eaten_stats fes ON le.leaderboard_entry_id = fes.leaderboard_entry_id
                 JOIN leaderboard_seasons s ON s.season_id = le.season_id
                 JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
                 WHERE le.leaderboard_id = $1 AND s.season_id = $2
                   AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                        OR lr.disabled_at IS NULL)
                   AND le.games_played >= $3
                 ORDER BY COALESCE(fes.food_score, 0) DESC
                 LIMIT 100"#,
                leaderboard_id,
                season_id,
                MIN_GAMES_FOR_RANKING
            )
            .fetch_all(pool)
            .await
        }
    }
    .wrap_err("Failed to fetch ranked leaderboard entries")?;

    Ok(entries)
}

/// Get placement entries (active snakes below minimum games threshold)
pub async fn get_placement_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
) -> cja::Result<Vec<RankedEntry>> {
    let entries = sqlx::query_as!(
        RankedEntry,
        r#"SELECT
            le.leaderboard_entry_id,
            le.battlesnake_id,
            b.user_id,
            le.display_score,
            le.games_played,
            le.first_place_finishes,
            le.non_first_finishes,
            le.mu,
            le.sigma,
            (b.deleted_at IS NOT NULL) AS "deleted!",
            CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
            CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
            CASE WHEN b.deleted_at IS NULL THEN b.head END AS "snake_head?",
            CASE WHEN b.deleted_at IS NULL THEN b.tail END AS "snake_tail?",
            CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
            CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
            CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
         FROM leaderboard_entries le
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
         WHERE le.leaderboard_id = $1 AND s.season_id = $2
           AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                OR lr.disabled_at IS NULL)
           AND le.games_played < $3
         ORDER BY le.games_played DESC
         LIMIT 100"#,
        leaderboard_id,
        season_id,
        MIN_GAMES_FOR_RANKING
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch placement leaderboard entries")?;

    Ok(entries)
}

/// A snake's entry for the league's active season (`None` during a gap).
pub async fn get_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let entry = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            le.leaderboard_entry_id, le.leaderboard_id, le.season_id AS "season_id!", le.battlesnake_id,
            le.mu, le.sigma, le.display_score, le.games_played, le.first_place_finishes, le.non_first_finishes,
            le.disabled_at, le.disabled_reason, le.created_at, le.updated_at
         FROM leaderboard_entries le
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         WHERE le.leaderboard_id = $1 AND le.battlesnake_id = $2
           AND s.starts_at <= clock_timestamp()
           AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)"#,
        leaderboard_id,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard entry")?;

    Ok(entry)
}

/// A snake's entry in one season, locking the row for update. Use this
/// within a transaction to prevent concurrent rating updates.
pub async fn get_entry_for_update_by_season_snake<'e, E>(
    executor: E,
    season_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let entry = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            leaderboard_entry_id, leaderboard_id, season_id AS "season_id!", battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, disabled_reason, created_at, updated_at
         FROM leaderboard_entries
         WHERE season_id = $1 AND battlesnake_id = $2
         FOR UPDATE"#,
        season_id,
        battlesnake_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch season leaderboard entry for update")?;

    Ok(entry)
}

/// Get a specific entry by its primary key, locking the row for update.
/// Use this within a transaction to prevent concurrent rating updates.
/// Prefer this over get_entry_for_update_by_season_snake when the leaderboard_entry_id is known —
/// it is always deterministic, even when duplicate entries exist for the same battlesnake.
pub async fn get_entry_for_update_by_id<'e, E>(
    executor: E,
    leaderboard_entry_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let entry = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            leaderboard_entry_id, leaderboard_id, season_id AS "season_id!", battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, disabled_reason, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_entry_id = $1
         FOR UPDATE"#,
        leaderboard_entry_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch leaderboard entry for update by ID")?;

    Ok(entry)
}

/// Get entry by ID
pub async fn get_entry_by_id(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let entry = sqlx::query_as!(
        LeaderboardEntry,
        r#"SELECT
            leaderboard_entry_id, leaderboard_id, season_id AS "season_id!", battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, disabled_reason, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_entry_id = $1"#,
        leaderboard_entry_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard entry by ID")?;

    Ok(entry)
}

/// Update rating for an entry after a game.
/// Accepts any sqlx executor (pool or transaction).
pub async fn update_rating<'e, E>(
    executor: E,
    entry_id: Uuid,
    mu: f64,
    sigma: f64,
    display_score: f64,
    is_first_place: bool,
) -> cja::Result<()>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"UPDATE leaderboard_entries
         SET mu = $2, sigma = $3, display_score = $4,
             games_played = games_played + 1,
             first_place_finishes = first_place_finishes + CASE WHEN $5 THEN 1 ELSE 0 END,
             non_first_finishes = non_first_finishes + CASE WHEN $5 THEN 0 ELSE 1 END
         WHERE leaderboard_entry_id = $1"#,
        entry_id,
        mu,
        sigma,
        display_score,
        is_first_place
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update rating")?;

    Ok(())
}

/// One of a user's snakes registered in a league, with its active-season
/// entry when it has one.
#[derive(Debug, Clone)]
pub struct UserLeagueMembership {
    pub battlesnake_id: Uuid,
    pub leaderboard_registration_id: Uuid,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled_reason: Option<String>,
    /// `None` when the snake has no entry for the active season yet (a gap,
    /// or joined while paused).
    pub leaderboard_entry_id: Option<Uuid>,
    /// Active-season score; 0 without an entry.
    pub display_score: f64,
    /// Active-season games; 0 without an entry.
    pub games_played: i32,
}

/// A user's live snakes registered in a league (active or paused), best
/// active-season score first.
pub async fn get_user_memberships(
    pool: &PgPool,
    leaderboard_id: Uuid,
    user_id: Uuid,
) -> cja::Result<Vec<UserLeagueMembership>> {
    sqlx::query_as!(
        UserLeagueMembership,
        r#"SELECT
            r.battlesnake_id,
            r.leaderboard_registration_id,
            r.disabled_at,
            r.disabled_reason,
            le.leaderboard_entry_id AS "leaderboard_entry_id?",
            COALESCE(le.display_score, 0.0) AS "display_score!",
            COALESCE(le.games_played, 0) AS "games_played!"
         FROM leaderboard_registrations r
         JOIN battlesnakes b ON b.battlesnake_id = r.battlesnake_id
         LEFT JOIN leaderboard_seasons s
           ON s.leaderboard_id = r.leaderboard_id
          AND s.starts_at <= clock_timestamp()
          AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)
         LEFT JOIN leaderboard_entries le
           ON le.season_id = s.season_id AND le.battlesnake_id = r.battlesnake_id
         WHERE r.leaderboard_id = $1 AND b.user_id = $2 AND b.deleted_at IS NULL
         ORDER BY le.display_score DESC NULLS LAST"#,
        leaderboard_id,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch user league memberships")
}

// --- Leaderboard game queries ---

/// Create a leaderboard game link in the league's active season. Errors with
/// "no active season" when the league has none. Accepts any sqlx executor
/// (pool or transaction).
pub async fn create_leaderboard_game<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
    game_id: Uuid,
) -> cja::Result<LeaderboardGame>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    // Stamped with the league's active season at INSERT; no active season
    // (a gap, or `ends_at` crossed since the caller checked) inserts nothing.
    let game = sqlx::query_as!(
        LeaderboardGame,
        r#"INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id)
         SELECT $1, $2, s.season_id
         FROM leaderboard_seasons s
         WHERE s.leaderboard_id = $1
           AND s.starts_at <= clock_timestamp()
           AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)
         RETURNING leaderboard_game_id, leaderboard_id, game_id, season_id AS "season_id!", created_at"#,
        leaderboard_id,
        game_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to create leaderboard game")?;

    game.ok_or_else(|| color_eyre::eyre::eyre!("no active season for leaderboard {leaderboard_id}"))
}

pub async fn find_leaderboard_game_by_game_id(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<LeaderboardGame>> {
    let game = sqlx::query_as!(
        LeaderboardGame,
        r#"SELECT leaderboard_game_id, leaderboard_id, game_id, season_id AS "season_id!", created_at
         FROM leaderboard_games
         WHERE game_id = $1"#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to find leaderboard game by game_id")?;

    Ok(game)
}

// --- Leaderboard game result queries ---

pub struct CreateGameResult {
    pub leaderboard_game_id: Uuid,
    pub leaderboard_entry_id: Uuid,
    pub placement: i32,
    pub mu_before: f64,
    pub mu_after: f64,
    pub sigma_before: f64,
    pub sigma_after: f64,
    pub display_score_change: f64,
}

/// Record a game result for a snake. Accepts any sqlx executor (pool or transaction).
/// Uses ON CONFLICT DO NOTHING as a DB-level idempotency guard.
pub async fn create_game_result<'e, E>(
    executor: E,
    data: CreateGameResult,
) -> cja::Result<Option<LeaderboardGameResult>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let result = sqlx::query_as!(
        LeaderboardGameResult,
        r#"INSERT INTO leaderboard_game_results (
            leaderboard_game_id, leaderboard_entry_id, placement,
            mu_before, mu_after, sigma_before, sigma_after, display_score_change
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (leaderboard_game_id, leaderboard_entry_id) DO NOTHING
         RETURNING
            leaderboard_game_result_id, leaderboard_game_id, leaderboard_entry_id,
            placement, mu_before, mu_after, sigma_before, sigma_after,
            display_score_change, food_eaten, created_at"#,
        data.leaderboard_game_id,
        data.leaderboard_entry_id,
        data.placement,
        data.mu_before,
        data.mu_after,
        data.sigma_before,
        data.sigma_after,
        data.display_score_change
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to create leaderboard game result")?;

    Ok(result)
}

/// Count active participants in a leaderboard
pub async fn count_active_entries(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!"
         FROM leaderboard_registrations
         WHERE leaderboard_id = $1 AND disabled_at IS NULL"#,
        leaderboard_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count active leaderboard entries")?;

    Ok(count)
}

// --- Leaderboard detail page queries ---

/// Get ranked entries with pagination (replaces LIMIT 100 cap)
pub async fn get_ranked_entries_paginated(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    page: i64,
    per_page: i64,
    sort: LeaderboardSort,
) -> cja::Result<Vec<RankedEntry>> {
    let offset = page * per_page;
    let entries = match sort {
        LeaderboardSort::Rating => {
            sqlx::query_as!(
                RankedEntry,
                r#"SELECT
                    le.leaderboard_entry_id,
                    le.battlesnake_id,
                    b.user_id,
                    le.display_score,
                    le.games_played,
                    le.first_place_finishes,
                    le.non_first_finishes,
                    le.mu,
                    le.sigma,
                    (b.deleted_at IS NOT NULL) AS "deleted!",
                    CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
                    CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
                    CASE WHEN b.deleted_at IS NULL THEN b.head END AS "snake_head?",
                    CASE WHEN b.deleted_at IS NULL THEN b.tail END AS "snake_tail?",
                    CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
                    CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
                    CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
                 FROM leaderboard_entries le
                 JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
                 JOIN users u ON b.user_id = u.user_id
                 JOIN leaderboard_seasons s ON s.season_id = le.season_id
                 JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
                 WHERE le.leaderboard_id = $1 AND s.season_id = $2
                   AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                        OR lr.disabled_at IS NULL)
                   AND le.games_played >= $3
                 ORDER BY le.display_score DESC
                 LIMIT $4 OFFSET $5"#,
                leaderboard_id,
                season_id,
                MIN_GAMES_FOR_RANKING,
                per_page,
                offset
            )
            .fetch_all(pool)
            .await
        }
        LeaderboardSort::FoodEaten => {
            sqlx::query_as!(
                RankedEntry,
                r#"SELECT
                    le.leaderboard_entry_id,
                    le.battlesnake_id,
                    b.user_id,
                    le.display_score,
                    le.games_played,
                    le.first_place_finishes,
                    le.non_first_finishes,
                    le.mu,
                    le.sigma,
                    (b.deleted_at IS NOT NULL) AS "deleted!",
                    CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
                    CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
                    CASE WHEN b.deleted_at IS NULL THEN b.head END AS "snake_head?",
                    CASE WHEN b.deleted_at IS NULL THEN b.tail END AS "snake_tail?",
                    CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
                    CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
                    CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
                 FROM leaderboard_entries le
                 JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
                 JOIN users u ON b.user_id = u.user_id
                 LEFT JOIN food_eaten_stats fes ON le.leaderboard_entry_id = fes.leaderboard_entry_id
                 JOIN leaderboard_seasons s ON s.season_id = le.season_id
                 JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
                 WHERE le.leaderboard_id = $1 AND s.season_id = $2
                   AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                        OR lr.disabled_at IS NULL)
                   AND le.games_played >= $3
                 ORDER BY COALESCE(fes.food_score, 0) DESC
                 LIMIT $4 OFFSET $5"#,
                leaderboard_id,
                season_id,
                MIN_GAMES_FOR_RANKING,
                per_page,
                offset
            )
            .fetch_all(pool)
            .await
        }
    }
    .wrap_err("Failed to fetch paginated ranked entries")?;

    Ok(entries)
}

/// Count total ranked entries for pagination
pub async fn count_ranked_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!"
         FROM leaderboard_entries le
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
         WHERE le.leaderboard_id = $1 AND s.season_id = $2
           AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                OR lr.disabled_at IS NULL)
           AND le.games_played >= $3"#,
        leaderboard_id,
        season_id,
        MIN_GAMES_FOR_RANKING
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count ranked entries")?;

    Ok(count)
}

/// Game history entry for a leaderboard entry
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardGameHistoryEntry {
    pub leaderboard_game_id: Uuid,
    pub game_id: Uuid,
    pub placement: i32,
    /// First place shared with another snake: nobody won.
    pub is_draw: bool,
    pub display_score_change: f64,
    pub mu_before: f64,
    pub mu_after: f64,
    pub sigma_before: f64,
    pub sigma_after: f64,
    pub food_eaten: i32,
    pub game_created_at: chrono::DateTime<chrono::Utc>,
}

/// Get paginated game history for a leaderboard entry
pub async fn get_game_history_for_entry(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<LeaderboardGameHistoryEntry>> {
    let offset = page * per_page;
    let entries = sqlx::query_as!(
        LeaderboardGameHistoryEntry,
        r#"SELECT
            lgr.leaderboard_game_id,
            lg.game_id,
            lgr.placement,
            lgr.placement = 1 AND EXISTS (
                SELECT 1 FROM leaderboard_game_results tied
                WHERE tied.leaderboard_game_id = lgr.leaderboard_game_id
                  AND tied.placement = 1
                  AND tied.leaderboard_entry_id <> lgr.leaderboard_entry_id
            ) AS "is_draw!",
            lgr.display_score_change,
            lgr.mu_before,
            lgr.mu_after,
            lgr.sigma_before,
            lgr.sigma_after,
            lgr.food_eaten,
            lg.created_at as game_created_at
         FROM leaderboard_game_results lgr
         JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
         WHERE lgr.leaderboard_entry_id = $1
         ORDER BY lg.created_at DESC
         LIMIT $2 OFFSET $3"#,
        leaderboard_entry_id,
        per_page,
        offset
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch game history for entry")?;

    Ok(entries)
}

/// Count total game results for a leaderboard entry
pub async fn count_game_results_for_entry(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!"
         FROM leaderboard_game_results
         WHERE leaderboard_entry_id = $1"#,
        leaderboard_entry_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count game results for entry")?;

    Ok(count)
}

/// Rating point for chart visualization
#[derive(Debug, FromRow)]
pub struct RatingPoint {
    pub display_score_after: f64,
    pub game_created_at: chrono::DateTime<chrono::Utc>,
}

/// Get full rating history for chart
pub async fn get_rating_history_for_entry(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
) -> cja::Result<Vec<RatingPoint>> {
    // Cap at 500 most recent points to avoid unbounded result sets for
    // snakes with thousands of games. We use a subquery so the outer
    // ORDER is ascending (needed for the SVG polyline) while still
    // keeping only the latest 500.
    let points = sqlx::query_as!(
        RatingPoint,
        r#"SELECT display_score_after as "display_score_after!", game_created_at as "game_created_at!" FROM (
            SELECT
                (lgr.mu_after - 3.0 * lgr.sigma_after) as display_score_after,
                lg.created_at as game_created_at
            FROM leaderboard_game_results lgr
            JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
            WHERE lgr.leaderboard_entry_id = $1
            ORDER BY lg.created_at DESC
            LIMIT 500
         ) recent
         ORDER BY game_created_at ASC"#,
        leaderboard_entry_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch rating history")?;

    Ok(points)
}

/// Opponent info for a game
#[derive(Debug, FromRow)]
pub struct GameOpponent {
    pub game_id: Uuid,
    pub snake_name: String,
    pub placement: Option<i32>,
    pub leaderboard_entry_id: Option<Uuid>,
}

/// Get opponents for a set of games, excluding a specific entry
pub async fn get_opponents_for_games(
    pool: &PgPool,
    game_ids: &[Uuid],
    exclude_entry_id: Uuid,
) -> cja::Result<Vec<GameOpponent>> {
    let opponents = sqlx::query_as!(
        GameOpponent,
        r#"SELECT
            gb.game_id,
            b.name as "snake_name!",
            gb.placement,
            gb.leaderboard_entry_id
         FROM game_battlesnakes gb
         LEFT JOIN leaderboard_entries le ON gb.leaderboard_entry_id = le.leaderboard_entry_id
         JOIN battlesnakes b ON COALESCE(gb.battlesnake_id, le.battlesnake_id) = b.battlesnake_id
         WHERE gb.game_id = ANY($1)
           AND (gb.leaderboard_entry_id IS NULL OR gb.leaderboard_entry_id != $2)
           AND (gb.battlesnake_id IS NULL OR gb.battlesnake_id != (
               SELECT battlesnake_id FROM leaderboard_entries WHERE leaderboard_entry_id = $2
           ))"#,
        game_ids,
        exclude_entry_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch opponents for games")?;

    Ok(opponents)
}

/// Leaderboard status for matchmaker visibility
#[derive(Debug)]
pub struct LeaderboardStatus {
    pub last_game_created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub games_in_progress: i64,
    pub total_games: i64,
}

/// Get leaderboard status (last game, in-progress count, total games)
pub async fn get_leaderboard_status(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
) -> cja::Result<LeaderboardStatus> {
    let last_game_created_at = sqlx::query_scalar!(
        r#"SELECT MAX(created_at) FROM leaderboard_games
         WHERE leaderboard_id = $1 AND season_id = $2"#,
        leaderboard_id,
        season_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch last game created_at")?;

    // Only `running` is live. Failed games are terminal, and a `waiting`
    // ladder game may be held for minutes until its snakes are free (DEV-1613),
    // so counting it would overstate what's on the board.
    let games_in_progress = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!" FROM leaderboard_games lg
         JOIN games g ON lg.game_id = g.game_id
         WHERE lg.leaderboard_id = $1 AND lg.season_id = $2 AND g.status = 'running'"#,
        leaderboard_id,
        season_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count games in progress")?;

    let total_games = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!" FROM leaderboard_games
         WHERE leaderboard_id = $1 AND season_id = $2"#,
        leaderboard_id,
        season_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count total games")?;

    Ok(LeaderboardStatus {
        last_game_created_at,
        games_in_progress,
        total_games,
    })
}

/// Activity feed entry for recent leaderboard events
#[derive(Debug, FromRow)]
pub struct ActivityFeedEntry {
    /// `None` when the snake is soft-deleted (tombstoned) — see `deleted`.
    pub snake_name: Option<String>,
    pub snake_color: Option<String>,
    /// Owner's GitHub login — the `/users/{login}` URL key, not display text.
    pub owner_login: Option<String>,
    /// Owner's public name: `display_name` when set, else the GitHub login.
    pub owner_name: Option<String>,
    pub leaderboard_entry_id: Uuid,
    pub placement: i32,
    /// First place shared with another snake: nobody won.
    pub is_draw: bool,
    pub display_score_change: f64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The snake is soft-deleted: identity fields above are NULL but the
    /// event stays as a tombstone (in every season — the feed has no
    /// registration predicate).
    pub deleted: bool,
}

/// Get recent activity feed for a leaderboard
pub async fn get_activity_feed(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    limit: i64,
) -> cja::Result<Vec<ActivityFeedEntry>> {
    let entries = sqlx::query_as!(
        ActivityFeedEntry,
        r#"SELECT
            (b.deleted_at IS NOT NULL) AS "deleted!",
            CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
            CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
            CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
            CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
            lgr.leaderboard_entry_id,
            lgr.placement,
            lgr.placement = 1 AND EXISTS (
                SELECT 1 FROM leaderboard_game_results tied
                WHERE tied.leaderboard_game_id = lgr.leaderboard_game_id
                  AND tied.placement = 1
                  AND tied.leaderboard_entry_id <> lgr.leaderboard_entry_id
            ) AS "is_draw!",
            lgr.display_score_change,
            lgr.created_at
         FROM leaderboard_game_results lgr
         JOIN leaderboard_entries le ON lgr.leaderboard_entry_id = le.leaderboard_entry_id
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
         WHERE lg.leaderboard_id = $1 AND lg.season_id = $2
         ORDER BY lgr.created_at DESC, lgr.leaderboard_game_id DESC, lgr.placement ASC,
                  b.battlesnake_id ASC
         LIMIT $3"#,
        leaderboard_id,
        season_id,
        limit
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch activity feed")?;

    Ok(entries)
}

/// Summary of a battlesnake's leaderboard participation
#[derive(Debug, FromRow)]
pub struct BattlesnakeLeaderboardSummary {
    /// The active-season entry; `None` when the snake has none yet (a gap).
    /// The stats below are `None` exactly when this is.
    pub leaderboard_entry_id: Option<Uuid>,
    /// The active season's id; `None` during a gap. Scopes the entry's rank.
    pub season_id: Option<Uuid>,
    pub leaderboard_id: Uuid,
    pub leaderboard_name: String,
    pub leaderboard_visibility: Visibility,
    pub display_score: Option<f64>,
    pub games_played: Option<i32>,
    pub first_place_finishes: Option<i32>,
    pub non_first_finishes: Option<i32>,
    /// Membership state, from the registration.
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled_reason: Option<String>,
}

/// A battlesnake's league registrations (one row each, with the
/// active-season entry when there is one) as `viewer` may see them. Private
/// leaderboards are only listed to their members (who hold the link already)
/// and admins; listing them to anyone else would leak the link. Pass `None`
/// for public surfaces such as social cards.
pub async fn get_entries_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    viewer: Option<&User>,
) -> cja::Result<Vec<BattlesnakeLeaderboardSummary>> {
    let viewer_id = viewer.map(|u| u.user_id);
    let viewer_is_admin = viewer.is_some_and(|u| u.is_admin);
    let entries = sqlx::query_as!(
        BattlesnakeLeaderboardSummary,
        r#"SELECT
            le.leaderboard_entry_id AS "leaderboard_entry_id?",
            s.season_id AS "season_id?",
            r.leaderboard_id,
            l.name as leaderboard_name,
            l.visibility as "leaderboard_visibility: Visibility",
            le.display_score AS "display_score?",
            le.games_played AS "games_played?",
            le.first_place_finishes AS "first_place_finishes?",
            le.non_first_finishes AS "non_first_finishes?",
            r.disabled_at,
            r.disabled_reason
         FROM leaderboard_registrations r
         JOIN leaderboards l ON r.leaderboard_id = l.leaderboard_id
         LEFT JOIN leaderboard_seasons s
           ON s.leaderboard_id = r.leaderboard_id
          AND s.starts_at <= clock_timestamp()
          AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)
         LEFT JOIN leaderboard_entries le
           ON le.season_id = s.season_id AND le.battlesnake_id = r.battlesnake_id
         WHERE r.battlesnake_id = $1
           AND (l.visibility = 'public' OR $2 OR leaderboard_member(l.leaderboard_id, $3))
         ORDER BY le.display_score DESC NULLS LAST"#,
        battlesnake_id,
        viewer_is_admin,
        viewer_id as Option<Uuid>
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboard entries for battlesnake")?;

    Ok(entries)
}

/// Get competition rank for a specific entry (count of entries with higher score + 1)
pub async fn get_rank_for_entry(
    pool: &PgPool,
    season_id: Uuid,
    display_score: f64,
    games_played: i32,
) -> cja::Result<Option<i64>> {
    if games_played < MIN_GAMES_FOR_RANKING {
        return Ok(None);
    }

    let rank = sqlx::query_scalar!(
        r#"SELECT COUNT(*) + 1 as "rank!"
         FROM leaderboard_entries le
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
         WHERE s.season_id = $1
           AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                OR lr.disabled_at IS NULL)
           AND le.games_played >= $2
           AND le.display_score > $3"#,
        season_id,
        MIN_GAMES_FOR_RANKING,
        display_score
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to get rank for entry")?;

    Ok(Some(rank))
}

/// Top food-eater on a leaderboard (display row)
#[derive(Debug, FromRow)]
pub struct TopEater {
    pub leaderboard_entry_id: Uuid,
    pub food_score: i64,
    /// `None` when the snake is soft-deleted (tombstoned) — see `deleted`.
    pub snake_name: Option<String>,
    pub snake_color: Option<String>,
    /// Owner's GitHub login — the `/users/{login}` URL key, not display text.
    pub owner_login: Option<String>,
    /// Owner's public name: `display_name` when set, else the GitHub login.
    pub owner_name: Option<String>,
    /// Owner's ISO-3166-1 alpha-2 country ('' when unset). Display-only.
    pub owner_country: Option<String>,
    /// The snake is soft-deleted: identity fields above are NULL but the
    /// row stays as a tombstone.
    pub deleted: bool,
}

/// Get the top food-eaters for a leaderboard, ordered by cumulative food eaten.
pub async fn get_top_eaters(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    limit: i64,
) -> cja::Result<Vec<TopEater>> {
    let entries = sqlx::query_as!(
        TopEater,
        r#"SELECT
            le.leaderboard_entry_id,
            fes.food_score,
            (b.deleted_at IS NOT NULL) AS "deleted!",
            CASE WHEN b.deleted_at IS NULL THEN b.name END AS "snake_name?",
            CASE WHEN b.deleted_at IS NULL THEN b.color END AS "snake_color?",
            CASE WHEN b.deleted_at IS NULL THEN u.github_login END AS "owner_login?",
            CASE WHEN b.deleted_at IS NULL THEN COALESCE(NULLIF(u.display_name, ''), u.github_login) END AS "owner_name?",
            CASE WHEN b.deleted_at IS NULL THEN u.country END AS "owner_country?"
         FROM leaderboard_entries le
         JOIN food_eaten_stats fes ON le.leaderboard_entry_id = fes.leaderboard_entry_id
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         JOIN leaderboard_seasons s ON s.season_id = le.season_id
         JOIN leaderboard_registrations lr ON lr.leaderboard_id = le.leaderboard_id AND lr.battlesnake_id = le.battlesnake_id
         WHERE le.leaderboard_id = $1 AND s.season_id = $2
           AND ((s.ends_at IS NOT NULL AND clock_timestamp() >= s.ends_at)
                OR lr.disabled_at IS NULL)
           AND fes.food_score > 0
         ORDER BY fes.food_score DESC
         LIMIT $3"#,
        leaderboard_id,
        season_id,
        limit
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch top eaters")?;

    Ok(entries)
}

/// The user-independent slice of the homepage: featured ladder, its recent
/// games (rail), and the top-five preview. Bundled so it can sit
/// behind a single TTL cache entry.
pub struct HomeFeed {
    pub featured: Option<Leaderboard>,
    pub activity: Vec<ActivityFeedEntry>,
    pub top_entries: Vec<RankedEntry>,
}

pub async fn load_home_feed(pool: &sqlx::PgPool) -> cja::Result<HomeFeed> {
    use color_eyre::eyre::Context as _;

    let leaderboards = get_active_public_leaderboards(pool)
        .await
        .wrap_err("Failed to fetch leaderboards")?;
    let featured = leaderboards.into_iter().next();

    // The featured ladder's default season (active, else last ended); none
    // when only future seasons exist.
    let featured_season = match &featured {
        Some(lb) => match leaderboard_season::select_season(pool, lb.leaderboard_id, None).await? {
            leaderboard_season::SeasonLookup::Found(selection) => selection
                .selected
                .map(|info| (lb.leaderboard_id, info.season.season_id)),
            leaderboard_season::SeasonLookup::Foreign => None,
        },
        None => None,
    };

    let (activity, top_entries) = if let Some((leaderboard_id, season_id)) = featured_season {
        let activity = get_activity_feed(pool, leaderboard_id, season_id, 8)
            .await
            .wrap_err("Failed to fetch activity feed")?;
        let top = get_ranked_entries_paginated(
            pool,
            leaderboard_id,
            season_id,
            0,
            5,
            LeaderboardSort::Rating,
        )
        .await
        .wrap_err("Failed to fetch top entries")?;
        (activity, top)
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(HomeFeed {
        featured,
        activity,
        top_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The league's auto-created Preseason (its only season in these tests).
    async fn preseason_id(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1",
        )
        .bind(leaderboard_id)
        .fetch_one(pool)
        .await?)
    }

    async fn leaderboard_game_with_status(
        pool: &PgPool,
        leaderboard_id: Uuid,
        status: &str,
    ) -> cja::Result<()> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await?;
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id) VALUES ($1, $2, (SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1))")
            .bind(leaderboard_id)
            .bind(game_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// "In progress" means running. Terminal states (finished, failed) must
    /// never show as live, and a waiting ladder game may be held for minutes
    /// before its snakes are free (DEV-1613), so it isn't live yet either.
    #[sqlx::test(migrations = "../migrations")]
    async fn games_in_progress_counts_only_live_states(pool: PgPool) -> cja::Result<()> {
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('status-test') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;

        for status in ["waiting", "running", "finished", "failed"] {
            leaderboard_game_with_status(&pool, leaderboard_id, status).await?;
        }

        let season_id = preseason_id(&pool, leaderboard_id).await?;
        let status = get_leaderboard_status(&pool, leaderboard_id, season_id).await?;
        assert_eq!(status.games_in_progress, 1);
        assert_eq!(status.total_games, 4);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn activity_feed_uses_snake_id_to_break_placement_ties(pool: PgPool) -> cja::Result<()> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('feed-order') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9910710, 'feed-owner') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let low_snake = Uuid::from_u128(1);
        let high_snake = Uuid::from_u128(2);
        sqlx::query!(
            "INSERT INTO battlesnakes (battlesnake_id, user_id, name, url)
             VALUES ($1, $3, 'low', 'http://low'), ($2, $3, 'high', 'http://high')",
            low_snake,
            high_snake,
            user_id
        )
        .execute(&pool)
        .await?;
        let low_entry = get_or_create_entry(&pool, leaderboard_id, low_snake)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;
        let high_entry = get_or_create_entry(&pool, leaderboard_id, high_snake)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;
        let game_id = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        )
        .fetch_one(&pool)
        .await?;
        let leaderboard_game_id = sqlx::query_scalar!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id)
             VALUES ($1, $2, (SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1)) RETURNING leaderboard_game_id",
            leaderboard_id,
            game_id
        )
        .fetch_one(&pool)
        .await?;
        for entry_id in [high_entry, low_entry] {
            create_game_result(
                &pool,
                CreateGameResult {
                    leaderboard_game_id,
                    leaderboard_entry_id: entry_id,
                    placement: 1,
                    mu_before: 25.0,
                    mu_after: 25.0,
                    sigma_before: 8.0,
                    sigma_after: 8.0,
                    display_score_change: 0.0,
                },
            )
            .await?;
        }

        let tied_at = chrono::Utc::now();
        sqlx::query!(
            "UPDATE leaderboard_game_results
             SET created_at = $1
             WHERE leaderboard_game_id = $2",
            tied_at,
            leaderboard_game_id,
        )
        .execute(&pool)
        .await?;

        let season_id = preseason_id(&pool, leaderboard_id).await?;
        for _ in 0..5 {
            let feed = get_activity_feed(&pool, leaderboard_id, season_id, 2).await?;
            assert_eq!(
                feed.iter()
                    .map(|entry| entry.leaderboard_entry_id)
                    .collect::<Vec<_>>(),
                vec![low_entry, high_entry]
            );
        }
        Ok(())
    }

    /// Every leaderboard query that carries the owner's login also carries
    /// their public name: `display_name` when set, the GitHub login when it
    /// is NULL or empty.
    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_queries_return_owner_public_name(pool: PgPool) -> cja::Result<()> {
        use std::collections::BTreeSet;

        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('owner-names') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let game_id = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        )
        .fetch_one(&pool)
        .await?;
        let leaderboard_game_id = sqlx::query_scalar!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id)
             VALUES ($1, $2, (SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1)) RETURNING leaderboard_game_id",
            leaderboard_id,
            game_id
        )
        .fetch_one(&pool)
        .await?;

        let owners: [(i64, &str, Option<&str>, &str); 3] = [
            (9920001, "gh-display", Some("Display Person"), "CA"),
            (9920002, "gh-null", None, ""),
            (9920003, "gh-empty", Some(""), ""),
        ];
        // Each owner gets one ranked and one placement snake so every query
        // sees both the display-name and the login-fallback case. The first
        // owner also carries a country so owner_country flows through the
        // ranked/placement/top-eater queries (the activity feed doesn't
        // select it).
        for (github_id, login, display_name, country) in owners {
            let user_id = sqlx::query_scalar!(
                "INSERT INTO users (external_github_id, github_login, display_name, country)
                 VALUES ($1, $2, $3, $4) RETURNING user_id",
                github_id,
                login,
                display_name,
                country
            )
            .fetch_one(&pool)
            .await?;
            for (kind, games_played) in [("ranked", MIN_GAMES_FOR_RANKING), ("placement", 1)] {
                let battlesnake_id = sqlx::query_scalar!(
                    "INSERT INTO battlesnakes (user_id, name, url)
                     VALUES ($1, $2, 'http://snake') RETURNING battlesnake_id",
                    user_id,
                    format!("{login}-{kind}")
                )
                .fetch_one(&pool)
                .await?;
                let entry_id = get_or_create_entry(&pool, leaderboard_id, battlesnake_id)
                    .await?
                    .expect("live snake joins")
                    .leaderboard_entry_id;
                sqlx::query!(
                    "UPDATE leaderboard_entries SET games_played = $2
                     WHERE leaderboard_entry_id = $1",
                    entry_id,
                    games_played
                )
                .execute(&pool)
                .await?;
                // get_or_create_entry initializes every algorithm's row, so overwrite it.
                sqlx::query(
                    "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score)
                     VALUES ($1, 3)
                     ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
                )
                .bind(entry_id)
                .execute(&pool)
                .await?;
                create_game_result(
                    &pool,
                    CreateGameResult {
                        leaderboard_game_id,
                        leaderboard_entry_id: entry_id,
                        placement: 1,
                        mu_before: 25.0,
                        mu_after: 25.0,
                        sigma_before: 8.0,
                        sigma_after: 8.0,
                        display_score_change: 0.0,
                    },
                )
                .await?;
            }
        }

        let expected: BTreeSet<(String, String, String)> = [
            ("gh-display", "Display Person", "CA"),
            ("gh-null", "gh-null", ""),
            ("gh-empty", "gh-empty", ""),
        ]
        .into_iter()
        .map(|(login, name, country)| (login.to_string(), name.to_string(), country.to_string()))
        .collect();
        let names = |rows: Vec<(String, String, String)>| rows.into_iter().collect::<BTreeSet<_>>();
        let ranked_names = |rows: Vec<RankedEntry>| {
            names(
                rows.into_iter()
                    .map(|e| {
                        (
                            e.owner_login.unwrap(),
                            e.owner_name.unwrap(),
                            e.owner_country.unwrap(),
                        )
                    })
                    .collect(),
            )
        };

        let season_id = preseason_id(&pool, leaderboard_id).await?;
        for sort in [LeaderboardSort::Rating, LeaderboardSort::FoodEaten] {
            assert_eq!(
                ranked_names(get_ranked_entries(&pool, leaderboard_id, season_id, sort).await?),
                expected,
                "get_ranked_entries {sort:?}"
            );
            assert_eq!(
                ranked_names(
                    get_ranked_entries_paginated(&pool, leaderboard_id, season_id, 0, 50, sort)
                        .await?
                ),
                expected,
                "get_ranked_entries_paginated {sort:?}"
            );
        }
        assert_eq!(
            ranked_names(get_placement_entries(&pool, leaderboard_id, season_id).await?),
            expected,
            "get_placement_entries"
        );
        assert_eq!(
            names(
                get_top_eaters(&pool, leaderboard_id, season_id, 10)
                    .await?
                    .into_iter()
                    .map(|e| (
                        e.owner_login.unwrap(),
                        e.owner_name.unwrap(),
                        e.owner_country.unwrap()
                    ))
                    .collect()
            ),
            expected,
            "get_top_eaters"
        );
        let feed_expected: BTreeSet<(String, String)> = expected
            .into_iter()
            .map(|(login, name, _)| (login, name))
            .collect();
        assert_eq!(
            get_activity_feed(&pool, leaderboard_id, season_id, 10)
                .await?
                .into_iter()
                .map(|e| (e.owner_login.unwrap(), e.owner_name.unwrap()))
                .collect::<BTreeSet<_>>(),
            feed_expected,
            "get_activity_feed"
        );
        Ok(())
    }

    /// Wait until `n` sessions in this test's database are blocked on a lock.
    /// One finished game in `season_id` with a result for each entry.
    async fn season_game(
        pool: &PgPool,
        leaderboard_id: Uuid,
        season_id: Uuid,
        entry_ids: &[Uuid],
    ) -> cja::Result<()> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'finished') RETURNING game_id",
        )
        .fetch_one(pool)
        .await?;
        let leaderboard_game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id)
             VALUES ($1, $2, $3) RETURNING leaderboard_game_id",
        )
        .bind(leaderboard_id)
        .bind(game_id)
        .bind(season_id)
        .fetch_one(pool)
        .await?;
        for (i, &leaderboard_entry_id) in entry_ids.iter().enumerate() {
            create_game_result(
                pool,
                CreateGameResult {
                    leaderboard_game_id,
                    leaderboard_entry_id,
                    placement: i as i32 + 1,
                    mu_before: 25.0,
                    mu_after: 25.0,
                    sigma_before: 8.0,
                    sigma_after: 8.0,
                    display_score_change: 0.0,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Every display query sees only the season it's given. An ended
    /// season's standings are frozen (entrants who later left still show);
    /// a live season's hide them.
    #[sqlx::test(migrations = "../migrations")]
    async fn display_queries_are_scoped_to_one_season(pool: PgPool) -> cja::Result<()> {
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('scoped') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let preseason = preseason_id(&pool, leaderboard_id).await?;
        sqlx::query(
            "UPDATE leaderboard_seasons
             SET starts_at = clock_timestamp() - interval '30 days',
                 ends_at = clock_timestamp() - interval '1 day'
             WHERE season_id = $1",
        )
        .bind(preseason)
        .execute(&pool)
        .await?;
        let season1: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
             VALUES ($1, 'Season 1', clock_timestamp() - interval '1 day') RETURNING season_id",
        )
        .bind(leaderboard_id)
        .fetch_one(&pool)
        .await?;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9930001, 'scoped-owner') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;

        // (name, season, games, score, food)
        let rows = [
            ("ada", preseason, 12, 50.0, 4),
            ("bea", preseason, 11, 40.0, 2),
            ("pip", preseason, 3, 1.0, 1),
            ("cy", season1, 10, 45.0, 5),
            ("bea", season1, 10, 90.0, 9),
        ];
        let mut snakes = std::collections::HashMap::new();
        let mut entries = std::collections::HashMap::new();
        for (name, season_id, games, score, food) in rows {
            let battlesnake_id = match snakes.get(name) {
                Some(id) => *id,
                None => {
                    let id: Uuid = sqlx::query_scalar(
                        "INSERT INTO battlesnakes (user_id, name, url)
                         VALUES ($1, $2, 'http://snake') RETURNING battlesnake_id",
                    )
                    .bind(user_id)
                    .bind(name)
                    .fetch_one(&pool)
                    .await?;
                    leaderboard_registration::get_or_create_registration(&pool, leaderboard_id, id)
                        .await?
                        .expect("live snake registers");
                    snakes.insert(name, id);
                    id
                }
            };
            let entry_id: Uuid = sqlx::query_scalar(
                "INSERT INTO leaderboard_entries
                     (leaderboard_id, season_id, battlesnake_id, games_played, display_score)
                 VALUES ($1, $2, $3, $4, $5) RETURNING leaderboard_entry_id",
            )
            .bind(leaderboard_id)
            .bind(season_id)
            .bind(battlesnake_id)
            .bind(games)
            .bind(score)
            .fetch_one(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score) VALUES ($1, $2)
                 ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
            )
            .bind(entry_id)
            .bind(food)
            .execute(&pool)
            .await?;
            entries.insert((name, season_id), entry_id);
        }
        // Bea leaves the league after the Preseason.
        leaderboard_registration::set_registration_disabled(
            &pool,
            leaderboard_id,
            snakes["bea"],
            Some(chrono::Utc::now()),
        )
        .await?;
        season_game(
            &pool,
            leaderboard_id,
            preseason,
            &[entries[&("ada", preseason)], entries[&("bea", preseason)]],
        )
        .await?;
        season_game(
            &pool,
            leaderboard_id,
            preseason,
            &[entries[&("pip", preseason)]],
        )
        .await?;
        season_game(&pool, leaderboard_id, season1, &[entries[&("cy", season1)]]).await?;

        let names = |rows: Vec<RankedEntry>| {
            rows.into_iter()
                .map(|e| e.snake_name.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        for sort in [LeaderboardSort::Rating, LeaderboardSort::FoodEaten] {
            let pre = names(get_ranked_entries(&pool, leaderboard_id, preseason, sort).await?);
            let pre_page = names(
                get_ranked_entries_paginated(&pool, leaderboard_id, preseason, 0, 50, sort).await?,
            );
            let live = names(get_ranked_entries(&pool, leaderboard_id, season1, sort).await?);
            assert_eq!(pre, vec!["ada", "bea"], "{sort:?}");
            assert_eq!(pre_page, pre, "{sort:?}");
            assert_eq!(live, vec!["cy"], "{sort:?}");
        }
        assert_eq!(
            count_ranked_entries(&pool, leaderboard_id, preseason).await?,
            2
        );
        assert_eq!(
            count_ranked_entries(&pool, leaderboard_id, season1).await?,
            1
        );
        assert_eq!(
            names(get_placement_entries(&pool, leaderboard_id, preseason).await?),
            vec!["pip"]
        );
        assert!(
            get_placement_entries(&pool, leaderboard_id, season1)
                .await?
                .is_empty()
        );

        let eaters = |rows: Vec<TopEater>| {
            rows.into_iter()
                .map(|e| e.snake_name.unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            eaters(get_top_eaters(&pool, leaderboard_id, preseason, 10).await?),
            vec!["ada", "bea", "pip"]
        );
        assert_eq!(
            eaters(get_top_eaters(&pool, leaderboard_id, season1, 10).await?),
            vec!["cy"]
        );

        let pre_status = get_leaderboard_status(&pool, leaderboard_id, preseason).await?;
        let live_status = get_leaderboard_status(&pool, leaderboard_id, season1).await?;
        assert_eq!(pre_status.total_games, 2);
        assert_eq!(live_status.total_games, 1);
        assert!(pre_status.last_game_created_at.is_some());

        let feed_entries = |rows: Vec<ActivityFeedEntry>| {
            let mut ids: Vec<Uuid> = rows.into_iter().map(|e| e.leaderboard_entry_id).collect();
            ids.sort();
            ids
        };
        let mut expected_pre = vec![
            entries[&("ada", preseason)],
            entries[&("bea", preseason)],
            entries[&("pip", preseason)],
        ];
        expected_pre.sort();
        assert_eq!(
            feed_entries(get_activity_feed(&pool, leaderboard_id, preseason, 10).await?),
            expected_pre
        );
        assert_eq!(
            feed_entries(get_activity_feed(&pool, leaderboard_id, season1, 10).await?),
            vec![entries[&("cy", season1)]]
        );

        // Ranks stay within the season; the leaver counts only once frozen.
        assert_eq!(
            get_rank_for_entry(&pool, preseason, 45.0, 10).await?,
            Some(2)
        );
        assert_eq!(
            get_rank_for_entry(&pool, preseason, 30.0, 10).await?,
            Some(3)
        );
        assert_eq!(get_rank_for_entry(&pool, season1, 30.0, 10).await?, Some(2));
        assert_eq!(get_rank_for_entry(&pool, season1, 30.0, 9).await?, None);
        Ok(())
    }

    /// Ended-season standings tombstone soft-deleted snakes: the row, its
    /// rank and its stats stay, but every identity field is NULL
    /// (`deleted == true`). Live seasons are unchanged — except the
    /// activity feed, which has no registration predicate and tombstones in
    /// every season by design.
    #[sqlx::test(migrations = "../migrations")]
    async fn ended_season_standings_tombstone_deleted_snakes(pool: PgPool) -> cja::Result<()> {
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('tombstone') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let preseason = preseason_id(&pool, leaderboard_id).await?;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9932001, 'tombstone-owner') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;

        // (name, games, score, food) in the active Preseason.
        let preseason_rows = [
            ("ada", 12, 50.0, 4),
            ("bea", 11, 40.0, 2),
            ("pip", 3, 1.0, 1),
        ];
        let mut snakes = std::collections::HashMap::new();
        let mut entries = std::collections::HashMap::new();
        for (name, games, score, food) in preseason_rows {
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, 'http://snake') RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(name)
            .fetch_one(&pool)
            .await?;
            let entry_id = get_or_create_entry(&pool, leaderboard_id, battlesnake_id)
                .await?
                .expect("live snake joins")
                .leaderboard_entry_id;
            sqlx::query(
                "UPDATE leaderboard_entries SET games_played = $2, display_score = $3
                 WHERE leaderboard_entry_id = $1",
            )
            .bind(entry_id)
            .bind(games)
            .bind(score)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score)
                 VALUES ($1, $2)
                 ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
            )
            .bind(entry_id)
            .bind(food)
            .execute(&pool)
            .await?;
            snakes.insert(name, battlesnake_id);
            entries.insert(name, entry_id);
        }
        season_game(
            &pool,
            leaderboard_id,
            preseason,
            &[entries["ada"], entries["bea"]],
        )
        .await?;

        // The Preseason ends; Season 1 goes live.
        sqlx::query(
            "UPDATE leaderboard_seasons
             SET starts_at = clock_timestamp() - interval '30 days',
                 ends_at = clock_timestamp() - interval '1 day'
             WHERE season_id = $1",
        )
        .bind(preseason)
        .execute(&pool)
        .await?;
        let season1: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
             VALUES ($1, 'Season 1', clock_timestamp() - interval '1 day') RETURNING season_id",
        )
        .bind(leaderboard_id)
        .fetch_one(&pool)
        .await?;

        // cy joins the live season, plays, then is deleted mid-season.
        let cy_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'cy', 'http://snake') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let cy_entry = get_or_create_entry(&pool, leaderboard_id, cy_id)
            .await?
            .expect("live snake joins the live season")
            .leaderboard_entry_id;
        sqlx::query(
            "UPDATE leaderboard_entries SET games_played = 10, display_score = 45
             WHERE leaderboard_entry_id = $1",
        )
        .bind(cy_entry)
        .execute(&pool)
        .await?;
        season_game(&pool, leaderboard_id, season1, &[cy_entry]).await?;

        // Only once the facts are in the history books, the ghosts are
        // deleted — through the real flow, so registrations are disabled
        // exactly as in prod.
        for snake_id in [snakes["bea"], cy_id] {
            assert_eq!(
                crate::models::battlesnake::delete_battlesnake(&pool, snake_id, user_id).await?,
                crate::models::battlesnake::DeleteBattlesnakeOutcome::Deleted
            );
        }

        // Ended-season ranked standings: bea stays as a tombstone in rank
        // order; ada is untouched.
        for sort in [LeaderboardSort::Rating, LeaderboardSort::FoodEaten] {
            for rows in [
                get_ranked_entries(&pool, leaderboard_id, preseason, sort).await?,
                get_ranked_entries_paginated(&pool, leaderboard_id, preseason, 0, 50, sort).await?,
            ] {
                let order: Vec<Uuid> = rows.iter().map(|e| e.battlesnake_id).collect();
                assert_eq!(order, vec![snakes["ada"], snakes["bea"]], "{sort:?}");
                let (ada, bea) = (&rows[0], &rows[1]);
                assert!(!ada.deleted, "{sort:?}");
                assert_eq!(ada.snake_name.as_deref(), Some("ada"), "{sort:?}");
                assert!(bea.deleted, "{sort:?}");
                assert!(bea.snake_name.is_none(), "{sort:?}");
                assert!(bea.snake_color.is_none(), "{sort:?}");
                assert!(bea.snake_head.is_none(), "{sort:?}");
                assert!(bea.snake_tail.is_none(), "{sort:?}");
                assert!(bea.owner_login.is_none(), "{sort:?}");
                assert!(bea.owner_name.is_none(), "{sort:?}");
                assert!(bea.owner_country.is_none(), "{sort:?}");
                // The row itself is intact: rank inputs and stats survive.
                assert_eq!((bea.display_score, bea.games_played), (40.0, 11));
            }
        }
        // The tombstone still counts and still holds its rank slot: a
        // 45.0 snake ranks #2 behind ada, exactly as it would with bea live.
        assert_eq!(
            count_ranked_entries(&pool, leaderboard_id, preseason).await?,
            2
        );
        assert_eq!(
            get_rank_for_entry(&pool, preseason, 45.0, 10).await?,
            Some(2)
        );

        // Top eaters: bea is present as a tombstone between ada and pip.
        let eaters = get_top_eaters(&pool, leaderboard_id, preseason, 10).await?;
        assert_eq!(
            eaters
                .iter()
                .map(|e| e.leaderboard_entry_id)
                .collect::<Vec<_>>(),
            vec![entries["ada"], entries["bea"], entries["pip"]]
        );
        assert!(!eaters[0].deleted);
        let bea_eater = &eaters[1];
        assert!(bea_eater.deleted);
        assert!(bea_eater.snake_name.is_none());
        assert!(bea_eater.snake_color.is_none());
        assert!(bea_eater.owner_login.is_none());
        assert!(bea_eater.owner_name.is_none());
        assert!(bea_eater.owner_country.is_none());

        // Feed in the ended season: bea's event stays, tombstoned; ada's is
        // untouched.
        let feed = get_activity_feed(&pool, leaderboard_id, preseason, 10).await?;
        let ada_event = feed
            .iter()
            .find(|e| e.leaderboard_entry_id == entries["ada"])
            .expect("ada's event stays");
        assert!(!ada_event.deleted);
        assert_eq!(ada_event.snake_name.as_deref(), Some("ada"));
        let bea_event = feed
            .iter()
            .find(|e| e.leaderboard_entry_id == entries["bea"])
            .expect("bea's event stays");
        assert!(bea_event.deleted);
        assert!(bea_event.snake_name.is_none());
        assert!(bea_event.snake_color.is_none());
        assert!(bea_event.owner_login.is_none());
        assert!(bea_event.owner_name.is_none());

        // Live season: ranked and placement drop cy entirely (disabled
        // registration), but the feed keeps cy's event as a tombstone —
        // the feed has never filtered on registration, so it tombstones in
        // every season.
        for sort in [LeaderboardSort::Rating, LeaderboardSort::FoodEaten] {
            assert!(
                get_ranked_entries(&pool, leaderboard_id, season1, sort)
                    .await?
                    .iter()
                    .all(|e| e.battlesnake_id != cy_id),
                "live-season ranked drops deleted snakes ({sort:?})"
            );
        }
        assert!(
            get_placement_entries(&pool, leaderboard_id, season1)
                .await?
                .is_empty()
        );
        let live_feed = get_activity_feed(&pool, leaderboard_id, season1, 10).await?;
        let cy_event = live_feed
            .iter()
            .find(|e| e.leaderboard_entry_id == cy_entry)
            .expect("the feed keeps deleted snakes' events");
        assert!(cy_event.deleted);
        assert!(cy_event.snake_name.is_none());
        assert!(cy_event.owner_login.is_none());
        assert!(cy_event.owner_name.is_none());
        Ok(())
    }

    async fn wait_for_lock_waiters(pool: &PgPool, n: i64) -> cja::Result<()> {
        for _ in 0..500 {
            let waiting = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "count!" FROM pg_stat_activity
                   WHERE datname = current_database() AND wait_event_type = 'Lock'"#
            )
            .fetch_one(pool)
            .await?;
            if waiting >= n {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Err(cja::color_eyre::eyre::eyre!(
            "timed out waiting for {n} lock waiters"
        ))
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn joining_a_deleted_snake_is_refused(pool: PgPool) -> cja::Result<()> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('join-deleted') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9911811, 'join-deleted-owner') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'join-deleted-snake', 'http://snake') RETURNING battlesnake_id",
            user_id
        )
        .fetch_one(&pool)
        .await?;

        let joined = get_or_create_entry(&pool, leaderboard_id, snake).await?;
        assert!(joined.is_some(), "live snake must join: {joined:?}");

        assert_eq!(
            crate::models::battlesnake::delete_battlesnake(&pool, snake, user_id).await?,
            crate::models::battlesnake::DeleteBattlesnakeOutcome::Deleted
        );

        // The join is refused and the entry stays terminal.
        let rejoined = get_or_create_entry(&pool, leaderboard_id, snake).await?;
        assert!(rejoined.is_none(), "deleted snake rejoined: {rejoined:?}");
        let row = sqlx::query!(
            "SELECT disabled_at, disabled_reason
             FROM leaderboard_registrations
             WHERE leaderboard_id = $1 AND battlesnake_id = $2",
            leaderboard_id,
            snake
        )
        .fetch_one(&pool)
        .await?;
        assert!(row.disabled_at.is_some(), "entry re-enabled: {row:?}");
        assert_eq!(
            row.disabled_reason.as_deref(),
            Some("deleted"),
            "marker cleared: {row:?}"
        );

        // A snake that never existed is refused the same way.
        let missing = get_or_create_entry(&pool, leaderboard_id, Uuid::from_u128(99118110)).await?;
        assert!(missing.is_none(), "missing snake joined: {missing:?}");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn join_still_resumes_manually_paused_and_health_entries(
        pool: PgPool,
    ) -> cja::Result<()> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('join-resume') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9911812, 'join-resume-owner') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'join-resume-snake', 'http://snake') RETURNING battlesnake_id",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            get_or_create_entry(&pool, leaderboard_id, snake)
                .await?
                .is_some()
        );

        for reason in [None, Some("health")] {
            sqlx::query!(
                "UPDATE leaderboard_registrations
                 SET disabled_at = NOW(), disabled_reason = $2,
                     health_consecutive_failures = 3, health_consecutive_successes = 2
                 WHERE leaderboard_id = $1 AND battlesnake_id = $3",
                leaderboard_id,
                reason,
                snake
            )
            .execute(&pool)
            .await?;

            get_or_create_entry(&pool, leaderboard_id, snake)
                .await?
                .expect("paused entry resumes on join");
            let joined = crate::models::leaderboard_registration::get_registration(
                &pool,
                leaderboard_id,
                snake,
            )
            .await?
            .expect("registration");
            assert!(joined.disabled_at.is_none(), "{joined:?}");
            assert_eq!(joined.disabled_reason, None, "{joined:?}");
            let row = sqlx::query!(
                "SELECT health_consecutive_failures, health_consecutive_successes
                 FROM leaderboard_registrations
                 WHERE leaderboard_id = $1 AND battlesnake_id = $2",
                leaderboard_id,
                snake
            )
            .fetch_one(&pool)
            .await?;
            assert_eq!(
                (
                    row.health_consecutive_failures,
                    row.health_consecutive_successes
                ),
                (0, 0),
                "re-enabling must reset health streaks (paused via {reason:?}): {row:?}"
            );
        }
        Ok(())
    }

    /// The race, replayed deterministically: `delete_battlesnake` holds the
    /// snake row locked with both of its writes applied (but uncommitted)
    /// while a join runs. The join must not resurrect the entry after the
    /// delete commits. Both writes happen before the join is spawned: the old
    /// upsert never touches `battlesnakes`, so a join spawned before delete's
    /// entry write has nothing to block on, finishes first, and the test
    /// would pass on the old SQL.
    #[sqlx::test(migrations = "../migrations")]
    async fn join_cannot_resurrect_an_entry_while_delete_holds_the_snake_row(
        pool: PgPool,
    ) -> cja::Result<()> {
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('join-race') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES (9911813, 'join-race-owner') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'join-race-snake', 'http://snake') RETURNING battlesnake_id",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            get_or_create_entry(&pool, leaderboard_id, snake)
                .await?
                .is_some()
        );

        // Holder mimics delete_battlesnake's transaction end-to-end:
        // lock, then BOTH writes, all uncommitted.
        let mut holder = pool.begin().await?;
        sqlx::query_scalar!(
            r#"SELECT battlesnake_id FROM battlesnakes
               WHERE battlesnake_id = $1 AND deleted_at IS NULL FOR UPDATE"#,
            snake
        )
        .fetch_one(&mut *holder)
        .await?;
        sqlx::query!(
            "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
            snake
        )
        .execute(&mut *holder)
        .await?;
        sqlx::query!(
            r#"UPDATE leaderboard_registrations
               SET disabled_at = COALESCE(disabled_at, NOW()),
                   disabled_reason = $2, updated_at = NOW()
               WHERE battlesnake_id = $1"#,
            snake,
            crate::models::battlesnake::DISABLED_REASON_DELETED
        )
        .execute(&mut *holder)
        .await?;

        // get_or_create_entry takes &PgPool, so clone into a local and move it.
        let p = pool.clone();
        let join =
            tokio::spawn(async move { get_or_create_entry(&p, leaderboard_id, snake).await });

        // Exactly one lock waiter on both sides of the fix: the old upsert
        // queues on the entry row lock; the fixed one queues on FOR SHARE
        // against the holder's FOR UPDATE.
        wait_for_lock_waiters(&pool, 1).await?;

        holder.commit().await?;

        let joined = tokio::time::timeout(std::time::Duration::from_secs(30), join)
            .await
            .expect("join deadlocked")
            .expect("join task panicked")?;
        assert!(
            joined.is_none(),
            "join resurrected a deleted snake's entry: {joined:?}"
        );

        let row = sqlx::query!(
            r#"SELECT disabled_at, disabled_reason
               FROM leaderboard_registrations
               WHERE leaderboard_id = $1 AND battlesnake_id = $2"#,
            leaderboard_id,
            snake
        )
        .fetch_one(&pool)
        .await?;
        assert!(row.disabled_at.is_some(), "entry re-enabled: {row:?}");
        assert_eq!(
            row.disabled_reason.as_deref(),
            Some("deleted"),
            "marker cleared: {row:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn join_waiting_behind_retirement_cannot_restore_entry(pool: PgPool) -> cja::Result<()> {
        use crate::models::battlesnake::{self, RetireBattlesnakeOutcome};
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('retirement-race') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login) VALUES (262050, 'retirement-race-owner') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let snake: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Retirement Race', 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await?;
        let entry = get_or_create_entry(&pool, leaderboard_id, snake)
            .await?
            .unwrap();
        leaderboard_registration::set_registration_disabled(
            &pool,
            leaderboard_id,
            snake,
            Some(chrono::Utc::now()),
        )
        .await?;

        let mut holder = pool.begin().await?;
        sqlx::query("SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 FOR UPDATE")
            .bind(snake)
            .fetch_one(&mut *holder)
            .await?;
        let retire_pool = pool.clone();
        let retire = tokio::spawn(async move {
            battlesnake::retire_battlesnake(&retire_pool, snake, owner).await
        });
        wait_for_lock_waiters(&pool, 1).await?;
        let join_pool = pool.clone();
        let join =
            tokio::spawn(
                async move { get_or_create_entry(&join_pool, leaderboard_id, snake).await },
            );
        wait_for_lock_waiters(&pool, 2).await?;
        holder.commit().await?;
        assert_eq!(
            retire.await.expect("retirement panicked")?,
            RetireBattlesnakeOutcome::Retired {
                withdrawn_registrations: 0
            }
        );
        assert!(join.await.expect("join panicked")?.is_none());
        let after_entry = get_entry(&pool, leaderboard_id, snake).await?.unwrap();
        assert_eq!(after_entry.leaderboard_entry_id, entry.leaderboard_entry_id);
        let after =
            crate::models::leaderboard_registration::get_registration(&pool, leaderboard_id, snake)
                .await?
                .unwrap();
        assert!(after.disabled_at.is_some());
        assert!(
            after.disabled_reason.is_none(),
            "manual pause stays manual: {after:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn manual_resume_winning_snake_lock_is_recorded_by_retirement(
        pool: PgPool,
    ) -> cja::Result<()> {
        use crate::models::battlesnake::{
            self, RetireBattlesnakeOutcome, UnretireBattlesnakeOutcome,
        };
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('manual-resume-race') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let owner: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login) VALUES (262051, 'manual-resume-owner') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let snake: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Manual Resume', 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await?;
        get_or_create_entry(&pool, leaderboard_id, snake)
            .await?
            .unwrap();
        leaderboard_registration::set_registration_disabled(
            &pool,
            leaderboard_id,
            snake,
            Some(chrono::Utc::now()),
        )
        .await?;
        let mut holder = pool.begin().await?;
        sqlx::query("SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 FOR UPDATE")
            .bind(snake)
            .fetch_one(&mut *holder)
            .await?;
        let resume_pool = pool.clone();
        let resume = tokio::spawn(async move {
            leaderboard_registration::set_registration_disabled(
                &resume_pool,
                leaderboard_id,
                snake,
                None,
            )
            .await
        });
        wait_for_lock_waiters(&pool, 1).await?;
        let retire_pool = pool.clone();
        let retire = tokio::spawn(async move {
            battlesnake::retire_battlesnake(&retire_pool, snake, owner).await
        });
        wait_for_lock_waiters(&pool, 2).await?;
        holder.commit().await?;
        resume.await.expect("resume panicked")?;
        assert_eq!(
            retire.await.expect("retirement panicked")?,
            RetireBattlesnakeOutcome::Retired {
                withdrawn_registrations: 0
            }
        );
        let after =
            crate::models::leaderboard_registration::get_registration(&pool, leaderboard_id, snake)
                .await?
                .unwrap();
        assert_eq!(after.disabled_reason.as_deref(), Some("retired"));
        assert!(after.disabled_at.is_some());
        assert_eq!(
            battlesnake::unretire_battlesnake(&pool, snake, owner).await?,
            UnretireBattlesnakeOutcome::Unretired
        );
        assert!(
            crate::models::leaderboard_registration::get_registration(&pool, leaderboard_id, snake)
                .await?
                .unwrap()
                .disabled_at
                .is_none()
        );
        Ok(())
    }
}
