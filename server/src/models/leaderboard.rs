use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
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
    pub wins: i32,
    pub losses: i32,
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
    pub wins: i32,
    pub losses: i32,
    pub mu: f64,
    pub sigma: f64,
    pub snake_name: String,
    pub owner_login: String,
}

// --- Leaderboard queries ---
// Note: Using sqlx::query_as (runtime) instead of sqlx::query_as! (compile-time)
// because these tables are new and not yet in the offline query cache.

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

/// Opt-in a snake to a leaderboard. Returns existing entry if already joined.
pub async fn get_or_create_entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<LeaderboardEntry> {
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
         VALUES ($1, $2)
         ON CONFLICT (leaderboard_id, battlesnake_id)
         DO UPDATE SET disabled_at = NULL
         RETURNING
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, wins, losses,
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
            mu, sigma, display_score, games_played, wins, losses,
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
            le.wins,
            le.losses,
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
         ORDER BY le.display_score DESC",
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
            le.wins,
            le.losses,
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
         ORDER BY le.games_played DESC",
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
            mu, sigma, display_score, games_played, wins, losses,
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

/// Get entry by ID
pub async fn get_entry_by_id(
    pool: &PgPool,
    leaderboard_entry_id: Uuid,
) -> cja::Result<Option<LeaderboardEntry>> {
    let entry = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT
            leaderboard_entry_id, leaderboard_id, battlesnake_id,
            mu, sigma, display_score, games_played, wins, losses,
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

/// Update rating for an entry after a game
pub async fn update_rating(
    pool: &PgPool,
    entry_id: Uuid,
    mu: f64,
    sigma: f64,
    display_score: f64,
    is_win: bool,
) -> cja::Result<()> {
    if is_win {
        sqlx::query(
            "UPDATE leaderboard_entries
             SET mu = $2, sigma = $3, display_score = $4,
                 games_played = games_played + 1,
                 wins = wins + 1
             WHERE leaderboard_entry_id = $1",
        )
        .bind(entry_id)
        .bind(mu)
        .bind(sigma)
        .bind(display_score)
        .execute(pool)
        .await
        .wrap_err("Failed to update rating (win)")?;
    } else {
        sqlx::query(
            "UPDATE leaderboard_entries
             SET mu = $2, sigma = $3, display_score = $4,
                 games_played = games_played + 1,
                 losses = losses + 1
             WHERE leaderboard_entry_id = $1",
        )
        .bind(entry_id)
        .bind(mu)
        .bind(sigma)
        .bind(display_score)
        .execute(pool)
        .await
        .wrap_err("Failed to update rating (loss)")?;
    }

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
            le.mu, le.sigma, le.display_score, le.games_played, le.wins, le.losses,
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

pub async fn create_leaderboard_game(
    pool: &PgPool,
    leaderboard_id: Uuid,
    game_id: Uuid,
) -> cja::Result<LeaderboardGame> {
    let game = sqlx::query_as::<_, LeaderboardGame>(
        "INSERT INTO leaderboard_games (leaderboard_id, game_id)
         VALUES ($1, $2)
         RETURNING leaderboard_game_id, leaderboard_id, game_id, created_at",
    )
    .bind(leaderboard_id)
    .bind(game_id)
    .fetch_one(pool)
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

pub async fn create_game_result(
    pool: &PgPool,
    data: CreateGameResult,
) -> cja::Result<LeaderboardGameResult> {
    let result = sqlx::query_as::<_, LeaderboardGameResult>(
        "INSERT INTO leaderboard_game_results (
            leaderboard_game_id, leaderboard_entry_id, placement,
            mu_before, mu_after, sigma_before, sigma_after, display_score_change
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
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
    .fetch_one(pool)
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
