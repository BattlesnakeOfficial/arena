use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres};
use uuid::Uuid;

/// Application constants for leaderboard configuration
pub const MATCH_SIZE: usize = 4;
pub const MIN_GAMES_FOR_RANKING: i32 = 10;
pub const GAMES_PER_DAY: i32 = 100;

// Leaderboard model
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Leaderboard {
    pub leaderboard_id: Uuid,
    pub name: String,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// Leaderboard entry: one per snake per leaderboard
#[derive(Debug, Serialize, Deserialize, Clone, FromRow)]
pub struct LeaderboardEntry {
    pub leaderboard_entry_id: Uuid,
    pub leaderboard_id: Uuid,
    pub battlesnake_id: Uuid,
    pub mu: f64,
    pub sigma: f64,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// Leaderboard game: links a game to a leaderboard
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardGame {
    pub leaderboard_game_id: Uuid,
    pub leaderboard_id: Uuid,
    pub game_id: Uuid,
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
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// Ranked entry with snake and owner info for display
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct RankedEntry {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub mu: f64,
    pub sigma: f64,
    pub snake_name: String,
    pub owner_login: String,
}

// --- Leaderboard queries ---
// TODO: Switch to sqlx::query_as! (compile-time checked) macros once the migration
// is merged and the .sqlx offline query cache is updated with `cargo sqlx prepare`.
// Currently using sqlx::query_as (runtime) because these tables are new.

pub async fn get_all_leaderboards(pool: &PgPool) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as::<_, Leaderboard>(
        "SELECT leaderboard_id, name, disabled_at, created_at, updated_at
         FROM leaderboards
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboards")?;

    Ok(rows)
}

pub async fn get_active_leaderboards(pool: &PgPool) -> cja::Result<Vec<Leaderboard>> {
    let rows = sqlx::query_as::<_, Leaderboard>(
        "SELECT leaderboard_id, name, disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE disabled_at IS NULL
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch active leaderboards")?;

    Ok(rows)
}

pub async fn get_leaderboard_by_id(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Option<Leaderboard>> {
    let row = sqlx::query_as::<_, Leaderboard>(
        "SELECT leaderboard_id, name, disabled_at, created_at, updated_at
         FROM leaderboards
         WHERE leaderboard_id = $1",
    )
    .bind(leaderboard_id)
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard")?;

    Ok(row)
}

// --- Leaderboard entry queries ---

/// Opt-in a snake to a leaderboard. Always inserts a new entry.
/// The unique constraint has been removed to allow duplicate entries for stress-testing.
pub async fn get_or_create_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<LeaderboardEntry> {
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
         VALUES ($1, $2)
         RETURNING
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at",
    )
    .bind(leaderboard_id)
    .bind(battlesnake_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create or get leaderboard entry")?;

    Ok(entry)
}

/// Get all active entries for a leaderboard (not disabled)
pub async fn get_active_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Vec<LeaderboardEntry>> {
    let entries = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_id = $1 AND disabled_at IS NULL
         ORDER BY display_score DESC",
    )
    .bind(leaderboard_id)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch active leaderboard entries")?;

    Ok(entries)
}

/// Get ranked entries (only snakes with enough games) with snake/owner info
pub async fn get_ranked_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Vec<RankedEntry>> {
    let entries = sqlx::query_as::<_, RankedEntry>(
        "SELECT
            le.leaderboard_entry_id,
            le.battlesnake_id,
            le.display_score,
            le.games_played,
            le.first_place_finishes,
            le.non_first_finishes,
            le.mu,
            le.sigma,
            b.name as snake_name,
            u.github_login as owner_login
         FROM leaderboard_entries le
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         WHERE le.leaderboard_id = $1
           AND le.disabled_at IS NULL
           AND le.games_played >= $2
         ORDER BY le.display_score DESC
         LIMIT 100",
    )
    .bind(leaderboard_id)
    .bind(MIN_GAMES_FOR_RANKING)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch ranked leaderboard entries")?;

    Ok(entries)
}

/// Get placement entries (active snakes below minimum games threshold)
pub async fn get_placement_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
) -> cja::Result<Vec<RankedEntry>> {
    let entries = sqlx::query_as::<_, RankedEntry>(
        "SELECT
            le.leaderboard_entry_id,
            le.battlesnake_id,
            le.display_score,
            le.games_played,
            le.first_place_finishes,
            le.non_first_finishes,
            le.mu,
            le.sigma,
            b.name as snake_name,
            u.github_login as owner_login
         FROM leaderboard_entries le
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         WHERE le.leaderboard_id = $1
           AND le.disabled_at IS NULL
           AND le.games_played < $2
         ORDER BY le.games_played DESC
         LIMIT 100",
    )
    .bind(leaderboard_id)
    .bind(MIN_GAMES_FOR_RANKING)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch placement leaderboard entries")?;

    Ok(entries)
}

/// Get a specific entry by leaderboard and battlesnake
pub async fn get_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_id = $1 AND battlesnake_id = $2",
    )
    .bind(leaderboard_id)
    .bind(battlesnake_id)
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch leaderboard entry")?;

    Ok(entry)
}

/// Get a specific entry by leaderboard and battlesnake, locking the row for update.
/// Use this within a transaction to prevent concurrent rating updates.
pub async fn get_entry_for_update<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_id = $1 AND battlesnake_id = $2
         FOR UPDATE",
    )
    .bind(leaderboard_id)
    .bind(battlesnake_id)
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch leaderboard entry for update")?;

    Ok(entry)
}

/// Get a specific entry by its primary key, locking the row for update.
/// Use this within a transaction to prevent concurrent rating updates.
/// Prefer this over get_entry_for_update when the leaderboard_entry_id is known —
/// it is always deterministic, even when duplicate entries exist for the same battlesnake.
pub async fn get_entry_for_update_by_id<'e, E>(
    executor: E,
    leaderboard_entry_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_entry_id = $1
         FOR UPDATE",
    )
    .bind(leaderboard_entry_id)
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
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes,
            disabled_at, created_at, updated_at
         FROM leaderboard_entries
         WHERE leaderboard_entry_id = $1",
    )
    .bind(leaderboard_entry_id)
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
    sqlx::query(
        "UPDATE leaderboard_entries
         SET mu = $2, sigma = $3, display_score = $4,
             games_played = games_played + 1,
             first_place_finishes = first_place_finishes + CASE WHEN $5 THEN 1 ELSE 0 END,
             non_first_finishes = non_first_finishes + CASE WHEN $5 THEN 0 ELSE 1 END
         WHERE leaderboard_entry_id = $1",
    )
    .bind(entry_id)
    .bind(mu)
    .bind(sigma)
    .bind(display_score)
    .bind(is_first_place)
    .execute(executor)
    .await
    .wrap_err("Failed to update rating")?;

    Ok(())
}

/// Pause or resume a leaderboard entry
pub async fn set_disabled(
    pool: &PgPool,
    entry_id: Uuid,
    disabled_at: Option<chrono::DateTime<chrono::Utc>>,
) -> cja::Result<()> {
    sqlx::query(
        "UPDATE leaderboard_entries
         SET disabled_at = $2
         WHERE leaderboard_entry_id = $1",
    )
    .bind(entry_id)
    .bind(disabled_at)
    .execute(pool)
    .await
    .wrap_err("Failed to update leaderboard entry disabled status")?;

    Ok(())
}

/// Get entries for a specific user across a leaderboard
pub async fn get_user_entries(
    pool: &PgPool,
    leaderboard_id: Uuid,
    user_id: Uuid,
) -> cja::Result<Vec<LeaderboardEntry>> {
    let entries = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            le.leaderboard_entry_id, le.leaderboard_id, le.battlesnake_id,
            le.mu, le.sigma, le.display_score, le.games_played, le.first_place_finishes, le.non_first_finishes,
            le.disabled_at, le.created_at, le.updated_at
         FROM leaderboard_entries le
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         WHERE le.leaderboard_id = $1 AND b.user_id = $2
         ORDER BY le.display_score DESC",
    )
    .bind(leaderboard_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch user leaderboard entries")?;

    Ok(entries)
}

// --- Leaderboard game queries ---

/// Create a leaderboard game link. Accepts any sqlx executor (pool or transaction).
pub async fn create_leaderboard_game<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
    game_id: Uuid,
) -> cja::Result<LeaderboardGame>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let game = sqlx::query_as::<_, LeaderboardGame>(
        "INSERT INTO leaderboard_games (leaderboard_id, game_id)
         VALUES ($1, $2)
         RETURNING leaderboard_game_id, leaderboard_id, game_id, created_at",
    )
    .bind(leaderboard_id)
    .bind(game_id)
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create leaderboard game")?;

    Ok(game)
}

pub async fn find_leaderboard_game_by_game_id(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<LeaderboardGame>> {
    let game = sqlx::query_as::<_, LeaderboardGame>(
        "SELECT leaderboard_game_id, leaderboard_id, game_id, created_at
         FROM leaderboard_games
         WHERE game_id = $1",
    )
    .bind(game_id)
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
    let result = sqlx::query_as::<_, LeaderboardGameResult>(
        "INSERT INTO leaderboard_game_results (
            leaderboard_game_id, leaderboard_entry_id, placement,
            mu_before, mu_after, sigma_before, sigma_after, display_score_change
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (leaderboard_game_id, leaderboard_entry_id) DO NOTHING
         RETURNING
            leaderboard_game_result_id, leaderboard_game_id, leaderboard_entry_id,
            placement, mu_before, mu_after, sigma_before, sigma_after,
            display_score_change, created_at",
    )
    .bind(data.leaderboard_game_id)
    .bind(data.leaderboard_entry_id)
    .bind(data.placement)
    .bind(data.mu_before)
    .bind(data.mu_after)
    .bind(data.sigma_before)
    .bind(data.sigma_after)
    .bind(data.display_score_change)
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to create leaderboard game result")?;

    Ok(result)
}

/// Count active participants in a leaderboard
pub async fn count_active_entries(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)
         FROM leaderboard_entries
         WHERE leaderboard_id = $1 AND disabled_at IS NULL",
    )
    .bind(leaderboard_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count active leaderboard entries")?;

    Ok(row.0)
}

// --- Leaderboard detail page queries ---

/// Get ranked entries with pagination (replaces LIMIT 100 cap)
pub async fn get_ranked_entries_paginated(
    pool: &PgPool,
    leaderboard_id: Uuid,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<RankedEntry>> {
    let entries = sqlx::query_as::<_, RankedEntry>(
        "SELECT
            le.leaderboard_entry_id,
            le.battlesnake_id,
            le.display_score,
            le.games_played,
            le.first_place_finishes,
            le.non_first_finishes,
            le.mu,
            le.sigma,
            b.name as snake_name,
            u.github_login as owner_login
         FROM leaderboard_entries le
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         WHERE le.leaderboard_id = $1
           AND le.disabled_at IS NULL
           AND le.games_played >= $2
         ORDER BY le.display_score DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(leaderboard_id)
    .bind(MIN_GAMES_FOR_RANKING)
    .bind(per_page)
    .bind(page * per_page)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch paginated ranked entries")?;

    Ok(entries)
}

/// Count total ranked entries for pagination
pub async fn count_ranked_entries(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)
         FROM leaderboard_entries
         WHERE leaderboard_id = $1 AND disabled_at IS NULL AND games_played >= $2",
    )
    .bind(leaderboard_id)
    .bind(MIN_GAMES_FOR_RANKING)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count ranked entries")?;

    Ok(row.0)
}

/// Game history entry for a leaderboard entry
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct LeaderboardGameHistoryEntry {
    pub leaderboard_game_id: Uuid,
    pub game_id: Uuid,
    pub placement: i32,
    pub display_score_change: f64,
    pub mu_before: f64,
    pub mu_after: f64,
    pub sigma_before: f64,
    pub sigma_after: f64,
    pub game_created_at: chrono::DateTime<chrono::Utc>,
}

/// Get paginated game history for a leaderboard entry
pub async fn get_game_history_for_entry(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<LeaderboardGameHistoryEntry>> {
    let entries = sqlx::query_as::<_, LeaderboardGameHistoryEntry>(
        "SELECT
            lgr.leaderboard_game_id,
            lg.game_id,
            lgr.placement,
            lgr.display_score_change,
            lgr.mu_before,
            lgr.mu_after,
            lgr.sigma_before,
            lgr.sigma_after,
            lg.created_at as game_created_at
         FROM leaderboard_game_results lgr
         JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
         WHERE lgr.leaderboard_entry_id = $1
         ORDER BY lg.created_at DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(leaderboard_entry_id)
    .bind(per_page)
    .bind(page * per_page)
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
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*)
         FROM leaderboard_game_results
         WHERE leaderboard_entry_id = $1",
    )
    .bind(leaderboard_entry_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count game results for entry")?;

    Ok(row.0)
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
    let points = sqlx::query_as::<_, RatingPoint>(
        "SELECT display_score_after, game_created_at FROM (
            SELECT
                (lgr.mu_after - 3.0 * lgr.sigma_after) as display_score_after,
                lg.created_at as game_created_at
            FROM leaderboard_game_results lgr
            JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
            WHERE lgr.leaderboard_entry_id = $1
            ORDER BY lg.created_at DESC
            LIMIT 500
         ) recent
         ORDER BY game_created_at ASC",
    )
    .bind(leaderboard_entry_id)
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
    let opponents = sqlx::query_as::<_, GameOpponent>(
        "SELECT
            gb.game_id,
            b.name as snake_name,
            gb.placement,
            gb.leaderboard_entry_id
         FROM game_battlesnakes gb
         LEFT JOIN leaderboard_entries le ON gb.leaderboard_entry_id = le.leaderboard_entry_id
         JOIN battlesnakes b ON COALESCE(gb.battlesnake_id, le.battlesnake_id) = b.battlesnake_id
         WHERE gb.game_id = ANY($1)
           AND (gb.leaderboard_entry_id IS NULL OR gb.leaderboard_entry_id != $2)
           AND (gb.battlesnake_id IS NULL OR gb.battlesnake_id != (
               SELECT battlesnake_id FROM leaderboard_entries WHERE leaderboard_entry_id = $2
           ))",
    )
    .bind(game_ids)
    .bind(exclude_entry_id)
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
) -> cja::Result<LeaderboardStatus> {
    let last_game: (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT MAX(created_at) FROM leaderboard_games WHERE leaderboard_id = $1")
            .bind(leaderboard_id)
            .fetch_one(pool)
            .await
            .wrap_err("Failed to fetch last game created_at")?;

    let in_progress: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM leaderboard_games lg
         JOIN games g ON lg.game_id = g.game_id
         WHERE lg.leaderboard_id = $1 AND g.status != 'finished'",
    )
    .bind(leaderboard_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count games in progress")?;

    let total: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM leaderboard_games WHERE leaderboard_id = $1")
            .bind(leaderboard_id)
            .fetch_one(pool)
            .await
            .wrap_err("Failed to count total games")?;

    Ok(LeaderboardStatus {
        last_game_created_at: last_game.0,
        games_in_progress: in_progress.0,
        total_games: total.0,
    })
}

/// Activity feed entry for recent leaderboard events
#[derive(Debug, FromRow)]
pub struct ActivityFeedEntry {
    pub snake_name: String,
    pub owner_login: String,
    pub leaderboard_entry_id: Uuid,
    pub placement: i32,
    pub display_score_change: f64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Get recent activity feed for a leaderboard
pub async fn get_activity_feed(
    pool: &PgPool,
    leaderboard_id: Uuid,
    limit: i64,
) -> cja::Result<Vec<ActivityFeedEntry>> {
    let entries = sqlx::query_as::<_, ActivityFeedEntry>(
        "SELECT
            b.name as snake_name,
            u.github_login as owner_login,
            lgr.leaderboard_entry_id,
            lgr.placement,
            lgr.display_score_change,
            lgr.created_at
         FROM leaderboard_game_results lgr
         JOIN leaderboard_entries le ON lgr.leaderboard_entry_id = le.leaderboard_entry_id
         JOIN battlesnakes b ON le.battlesnake_id = b.battlesnake_id
         JOIN users u ON b.user_id = u.user_id
         JOIN leaderboard_games lg ON lgr.leaderboard_game_id = lg.leaderboard_game_id
         WHERE lg.leaderboard_id = $1
         ORDER BY lgr.created_at DESC
         LIMIT $2",
    )
    .bind(leaderboard_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch activity feed")?;

    Ok(entries)
}

/// Summary of a battlesnake's leaderboard participation
#[derive(Debug, FromRow)]
pub struct BattlesnakeLeaderboardSummary {
    pub leaderboard_entry_id: Uuid,
    pub leaderboard_id: Uuid,
    pub leaderboard_name: String,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Get all leaderboard entries for a battlesnake
pub async fn get_entries_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<BattlesnakeLeaderboardSummary>> {
    let entries = sqlx::query_as::<_, BattlesnakeLeaderboardSummary>(
        "SELECT
            le.leaderboard_entry_id,
            le.leaderboard_id,
            l.name as leaderboard_name,
            le.display_score,
            le.games_played,
            le.first_place_finishes,
            le.non_first_finishes,
            le.disabled_at
         FROM leaderboard_entries le
         JOIN leaderboards l ON le.leaderboard_id = l.leaderboard_id
         WHERE le.battlesnake_id = $1
         ORDER BY le.display_score DESC",
    )
    .bind(battlesnake_id)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboard entries for battlesnake")?;

    Ok(entries)
}

/// Get competition rank for a specific entry (count of entries with higher score + 1)
pub async fn get_rank_for_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    display_score: f64,
    games_played: i32,
) -> cja::Result<Option<i64>> {
    if games_played < MIN_GAMES_FOR_RANKING {
        return Ok(None);
    }

    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) + 1
         FROM leaderboard_entries
         WHERE leaderboard_id = $1
           AND disabled_at IS NULL
           AND games_played >= $2
           AND display_score > $3",
    )
    .bind(leaderboard_id)
    .bind(MIN_GAMES_FOR_RANKING)
    .bind(display_score)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to get rank for entry")?;

    Ok(Some(row.0))
}
