use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

use super::game::{Game, GameBoardSize, GameStatus, GameType};

// GameBattlesnake model for our application
#[derive(Debug, Serialize, Deserialize)]
pub struct GameBattlesnake {
    pub game_battlesnake_id: Uuid,
    pub game_id: Uuid,
    pub battlesnake_id: Option<Uuid>,
    pub placement: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// For adding a battlesnake to a game
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AddBattlesnakeToGame {
    pub battlesnake_id: Uuid,
}

// For setting the result of a game for a battlesnake
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SetGameResult {
    pub placement: i32,
}

// Extended GameBattlesnake with battlesnake details
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct GameBattlesnakeWithDetails {
    pub game_battlesnake_id: Uuid,
    pub game_id: Uuid,
    pub battlesnake_id: Uuid,
    pub placement: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    // Battlesnake details
    pub name: String,
    pub url: String,
    pub engine_region: crate::models::battlesnake::EngineRegion,
    pub user_id: Uuid,
    pub leaderboard_entry_id: Option<Uuid>,
    pub color: String,
    pub head: String,
    pub tail: String,
    /// Owner's GitHub login — the `/users/{login}` URL key, not display text.
    pub owner_login: String,
    /// Owner's public name: `display_name` when set, else the GitHub login.
    pub owner_name: String,
}

// Database functions for game battlesnake management

// Get all battlesnakes in a game
// TODO: Switch to sqlx::query_as! once schema stabilizes and offline cache is updated.
// Using runtime query_as here because COALESCE(gb.battlesnake_id, le.battlesnake_id)
// requires a LEFT JOIN that makes SQLx macro type inference ambiguous.
pub async fn get_battlesnakes_by_game_id(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Vec<GameBattlesnakeWithDetails>> {
    let game_battlesnakes = sqlx::query_as::<_, GameBattlesnakeWithDetails>(
        r#"
        SELECT
            gb.game_battlesnake_id,
            gb.game_id,
            COALESCE(gb.battlesnake_id, le.battlesnake_id) AS battlesnake_id,
            gb.placement,
            gb.created_at,
            gb.updated_at,
            b.name,
            b.url,
            b.engine_region,
            b.user_id,
            gb.leaderboard_entry_id,
            b.color,
            b.head,
            b.tail,
            u.github_login AS owner_login,
            COALESCE(NULLIF(u.display_name, ''), u.github_login) AS owner_name
        FROM game_battlesnakes gb
        LEFT JOIN leaderboard_entries le ON gb.leaderboard_entry_id = le.leaderboard_entry_id
        JOIN battlesnakes b
            ON COALESCE(gb.battlesnake_id, le.battlesnake_id) = b.battlesnake_id
        JOIN users u ON b.user_id = u.user_id
        WHERE gb.game_id = $1
        ORDER BY gb.placement NULLS LAST, gb.created_at ASC
        "#,
    )
    .bind(game_id)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch battlesnakes for game from database")?;

    Ok(game_battlesnakes)
}

// Get all games for a battlesnake
pub async fn get_games_by_battlesnake_id(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<Game>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            g.game_id,
            g.board_size,
            g.game_type,
            g.status,
            g.enqueued_at,
            g.created_at,
            g.updated_at
        FROM games g
        JOIN game_battlesnakes gb ON g.game_id = gb.game_id
        WHERE gb.battlesnake_id = $1
        ORDER BY g.created_at DESC
        "#,
        battlesnake_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch games for battlesnake from database")?;

    let games = rows
        .into_iter()
        .map(|row| {
            let board_size = GameBoardSize::from_str(&row.board_size)
                .wrap_err_with(|| format!("Invalid board size: {}", row.board_size))?;
            let game_type = GameType::from_str(&row.game_type)
                .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
            let status = GameStatus::from_str(&row.status)
                .wrap_err_with(|| format!("Invalid game status: {}", row.status))?;

            Ok(Game {
                game_id: row.game_id,
                board_size,
                game_type,
                status,
                enqueued_at: row.enqueued_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .collect::<cja::Result<Vec<_>>>()?;

    Ok(games)
}

// Add a battlesnake to a game
pub async fn add_battlesnake_to_game(
    pool: &PgPool,
    game_id: Uuid,
    data: AddBattlesnakeToGame,
) -> cja::Result<GameBattlesnake> {
    // Check if the game already has 4 battlesnakes
    let count = sqlx::query!(
        r#"
        SELECT COUNT(*) as count
        FROM game_battlesnakes
        WHERE game_id = $1
        "#,
        game_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count battlesnakes in game")?;

    if count.count.unwrap_or(0) >= 4 {
        return Err(cja::color_eyre::eyre::eyre!(
            "Game already has the maximum of 4 battlesnakes"
        ));
    }

    // Add the battlesnake to the game using the executor pattern
    super::game::add_battlesnake_to_game(pool, game_id, data.clone()).await?;

    // Fetch and return the newly created game_battlesnake
    let game_battlesnake = sqlx::query_as!(
        GameBattlesnake,
        r#"
        SELECT
            game_battlesnake_id,
            game_id,
            battlesnake_id,
            placement,
            created_at,
            updated_at
        FROM game_battlesnakes
        WHERE game_id = $1 AND battlesnake_id = $2
        "#,
        game_id,
        data.battlesnake_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch newly created game battlesnake")?;

    Ok(game_battlesnake)
}

// Remove a battlesnake from a game
pub async fn remove_battlesnake_from_game(
    pool: &PgPool,
    game_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        DELETE FROM game_battlesnakes
        WHERE game_id = $1 AND battlesnake_id = $2
        "#,
        game_id,
        battlesnake_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to remove battlesnake from game")?;

    Ok(())
}

// Set the result of a game for a battlesnake (by battlesnake_id)
// Note: This may not work correctly with duplicate snakes in a game
pub async fn set_game_result(
    pool: &PgPool,
    game_id: Uuid,
    battlesnake_id: Uuid,
    data: SetGameResult,
) -> cja::Result<GameBattlesnake> {
    // Validate placement is between 1 and 4
    if data.placement < 1 || data.placement > 4 {
        return Err(cja::color_eyre::eyre::eyre!(
            "Placement must be between 1 and 4"
        ));
    }

    let game_battlesnake = sqlx::query_as!(
        GameBattlesnake,
        r#"
        UPDATE game_battlesnakes
        SET placement = $3
        WHERE game_id = $1 AND battlesnake_id = $2
        RETURNING
            game_battlesnake_id,
            game_id,
            battlesnake_id,
            placement,
            created_at,
            updated_at
        "#,
        game_id,
        battlesnake_id,
        data.placement
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to set game result")?;

    Ok(game_battlesnake)
}

// Set the result for a specific game_battlesnake (supports duplicate snakes).
// Takes a connection so the game runner can compose it into its atomic
// finish transaction.
pub async fn set_game_result_by_id(
    conn: &mut sqlx::PgConnection,
    game_battlesnake_id: Uuid,
    placement: i32,
) -> cja::Result<GameBattlesnake> {
    // Validate placement is between 1 and 4
    if !(1..=4).contains(&placement) {
        return Err(cja::color_eyre::eyre::eyre!(
            "Placement must be between 1 and 4"
        ));
    }

    let game_battlesnake = sqlx::query_as!(
        GameBattlesnake,
        r#"
        UPDATE game_battlesnakes
        SET placement = $2
        WHERE game_battlesnake_id = $1
        RETURNING
            game_battlesnake_id,
            game_id,
            battlesnake_id,
            placement,
            created_at,
            updated_at
        "#,
        game_battlesnake_id,
        placement
    )
    .fetch_one(&mut *conn)
    .await
    .wrap_err("Failed to set game result")?;

    Ok(game_battlesnake)
}

// Game history entry for snake profile page
#[derive(Debug)]
pub struct GameHistoryEntry {
    pub game_id: Uuid,
    pub board_size: GameBoardSize,
    pub game_type: GameType,
    pub status: GameStatus,
    pub placement: Option<i32>,
    pub snake_count: i64,
    pub winner_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Return one page of profile history. Direct and per-leaderboard-entry paths
/// walk their own `(link_id, created_at DESC)` indexes, each bounded by
/// `offset + limit`. Participant creation time is the recency key; the row ID
/// breaks timestamp ties. Imported legacy games have no participant rows, so
/// an `engine_game_id` filter changes no results and ruins the ordered walk.
pub async fn get_game_history_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    limit: i64,
    offset: i64,
) -> cja::Result<Vec<GameHistoryEntry>> {
    let rows = sqlx::query!(
        r#"
        WITH linked AS (
            (SELECT gb.game_battlesnake_id, gb.game_id, gb.placement,
                    gb.created_at AS linked_at
             FROM game_battlesnakes gb
             WHERE gb.battlesnake_id = $1
             ORDER BY gb.created_at DESC, gb.game_battlesnake_id DESC
             LIMIT $2)
            UNION ALL
            SELECT lb.game_battlesnake_id, lb.game_id, lb.placement, lb.linked_at
            FROM leaderboard_entries le
            CROSS JOIN LATERAL (
                SELECT gb.game_battlesnake_id, gb.game_id, gb.placement,
                       gb.created_at AS linked_at
                FROM game_battlesnakes gb
                WHERE gb.leaderboard_entry_id = le.leaderboard_entry_id
                  AND gb.battlesnake_id IS NULL
                ORDER BY gb.created_at DESC, gb.game_battlesnake_id DESC
                LIMIT $2
            ) lb
            WHERE le.battlesnake_id = $1
        ), page AS (
            SELECT * FROM linked
            ORDER BY linked_at DESC, game_battlesnake_id DESC
            LIMIT $3 OFFSET $4
        )
        SELECT g.game_id, g.board_size, g.game_type, g.status, p.placement,
               (SELECT COUNT(*) FROM game_battlesnakes gb2
                WHERE gb2.game_id = p.game_id) AS "snake_count!",
               winner.name AS "winner_name?", g.created_at
        FROM page p
        JOIN games g ON g.game_id = p.game_id
        LEFT JOIN LATERAL (
            SELECT b.name
            FROM game_battlesnakes gb_winner
            LEFT JOIN leaderboard_entries le_winner
              ON le_winner.leaderboard_entry_id = gb_winner.leaderboard_entry_id
            JOIN battlesnakes b
              ON b.battlesnake_id = COALESCE(gb_winner.battlesnake_id,
                                            le_winner.battlesnake_id)
            WHERE gb_winner.game_id = p.game_id AND gb_winner.placement = 1
            ORDER BY gb_winner.game_battlesnake_id LIMIT 1
        ) winner ON TRUE
        ORDER BY p.linked_at DESC, p.game_battlesnake_id DESC
        "#,
        battlesnake_id,
        offset + limit,
        limit,
        offset
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch game history for battlesnake")?;

    let entries = rows
        .into_iter()
        .map(|row| {
            let board_size = GameBoardSize::from_str(&row.board_size)
                .wrap_err_with(|| format!("Invalid board size: {}", row.board_size))?;
            let game_type = GameType::from_str(&row.game_type)
                .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
            let status = GameStatus::from_str(&row.status)
                .wrap_err_with(|| format!("Invalid game status: {}", row.status))?;

            Ok(GameHistoryEntry {
                game_id: row.game_id,
                board_size,
                game_type,
                status,
                placement: row.placement,
                snake_count: row.snake_count,
                winner_name: row.winner_name,
                created_at: row.created_at,
            })
        })
        .collect::<cja::Result<Vec<_>>>()?;

    Ok(entries)
}

#[derive(Debug)]
pub struct GameHistoryStats {
    pub total_games: i64,
    pub finished_games: i64,
    pub placement_count: i64,
    pub wins: i64,
    pub second_places: i64,
    pub third_places: i64,
    pub fourth_places: i64,
    pub win_rate: f64,
    pub average_placement: f64,
}

/// Aggregate all profile history. Leaderboard rows never probe `games`:
/// 27.8k such probes timed out at 15s on prod, while a participant-only
/// aggregate took 10.1ms warm. Leaderboard type comes from `leaderboards`;
/// NULL placement means unfinished for these rows, verified on prod 2026-09-30.
/// `set_game_result` keys on `battlesnake_id`, so it cannot place ladder rows
/// (whose `battlesnake_id` is NULL); the ladder finisher writes placements via
/// `set_game_result_by_id` and status='finished' in one transaction. Revisit
/// this aggregate if another writer can place ladder rows. Direct rows still
/// join `games` because placed failed games exist there.
pub async fn get_game_stats_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<GameHistoryStats> {
    let row = sqlx::query!(
        r#"
        WITH classified AS (
            SELECT gb.placement,
                   (g.status = 'finished' AND lower(g.game_type) <> 'solo') AS competitive
            FROM game_battlesnakes gb
            JOIN games g ON g.game_id = gb.game_id
            WHERE gb.battlesnake_id = $1
            UNION ALL
            SELECT lb.placement,
                   (lb.placement IS NOT NULL AND lower(l.game_type) <> 'solo') AS competitive
            FROM leaderboard_entries le
            JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
            CROSS JOIN LATERAL (
                SELECT gb.placement
                FROM game_battlesnakes gb
                WHERE gb.leaderboard_entry_id = le.leaderboard_entry_id
                  AND gb.battlesnake_id IS NULL
            ) lb
            WHERE le.battlesnake_id = $1
        )
        SELECT COUNT(*) AS "total_games!",
               COUNT(*) FILTER (WHERE competitive) AS "finished_games!",
               COUNT(*) FILTER (WHERE competitive AND placement = 1) AS "wins!",
               COUNT(*) FILTER (WHERE competitive AND placement = 2) AS "second_places!",
               COUNT(*) FILTER (WHERE competitive AND placement = 3) AS "third_places!",
               COUNT(*) FILTER (WHERE competitive AND placement = 4) AS "fourth_places!",
               COUNT(placement) FILTER (WHERE competitive) AS "placement_count!",
               COALESCE(SUM(placement) FILTER (WHERE competitive), 0)::bigint AS "placement_sum!"
        FROM classified
        "#,
        battlesnake_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch game stats for battlesnake")?;

    Ok(GameHistoryStats {
        total_games: row.total_games,
        finished_games: row.finished_games,
        placement_count: row.placement_count,
        wins: row.wins,
        second_places: row.second_places,
        third_places: row.third_places,
        fourth_places: row.fourth_places,
        win_rate: if row.finished_games > 0 {
            row.wins as f64 / row.finished_games as f64 * 100.0
        } else {
            0.0
        },
        average_placement: if row.placement_count > 0 {
            row.placement_sum as f64 / row.placement_count as f64
        } else {
            0.0
        },
    })
}

// Get a game with all its battlesnakes
pub async fn get_game_with_battlesnakes(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<(Game, Vec<GameBattlesnakeWithDetails>)> {
    // Get the game
    let game = super::game::get_game_by_id(pool, game_id)
        .await?
        .ok_or_else(|| cja::color_eyre::eyre::eyre!("Game not found"))?;

    // Get the battlesnakes for the game
    let battlesnakes = get_battlesnakes_by_game_id(pool, game_id).await?;

    Ok((game, battlesnakes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::battlesnake::EngineRegion;

    #[sqlx::test(migrations = "../migrations")]
    async fn game_projection_carries_engine_region(pool: PgPool) -> cja::Result<()> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1480001, 'region-owner', 'test') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, visibility, engine_region)
             VALUES ($1, 'East', 'https://example.com/east', 'public', 'us-east4')
             RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'waiting') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
            .bind(game_id)
            .bind(snake_id)
            .execute(&pool)
            .await?;
        let rows = get_battlesnakes_by_game_id(&pool, game_id).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].engine_region, EngineRegion::UsEast4);
        Ok(())
    }
    async fn test_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'https://example.invalid', 'public') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .fetch_one(pool)
        .await?)
    }

    async fn test_entry(pool: &PgPool, snake_id: Uuid, offset: i64) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
             SELECT leaderboard_id, $1 FROM leaderboards ORDER BY created_at, leaderboard_id
             OFFSET $2 LIMIT 1 RETURNING leaderboard_entry_id",
        )
        .bind(snake_id)
        .bind(offset)
        .fetch_one(pool)
        .await?)
    }

    async fn test_game(
        pool: &PgPool,
        game_type: &str,
        status: &str,
        age_minutes: i64,
    ) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO games (board_size, game_type, status, created_at)
             VALUES ('11x11', $1, $2, now() - $3 * interval '1 minute') RETURNING game_id",
        )
        .bind(game_type)
        .bind(status)
        .bind(age_minutes)
        .fetch_one(pool)
        .await?)
    }

    async fn test_link(
        pool: &PgPool,
        game_id: Uuid,
        snake_id: Option<Uuid>,
        entry_id: Option<Uuid>,
        placement: Option<i32>,
    ) -> cja::Result<()> {
        sqlx::query(
            "INSERT INTO game_battlesnakes
             (game_id, battlesnake_id, leaderboard_entry_id, placement, created_at)
             SELECT $1, $2, $3, $4, created_at FROM games WHERE game_id = $1",
        )
        .bind(game_id)
        .bind(snake_id)
        .bind(entry_id)
        .bind(placement)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn profile_membership_winner_and_stat_semantics(pool: PgPool) -> cja::Result<()> {
        let user_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1482001, 'profile-model', 'test') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let direct = test_snake(&pool, user_id, "Direct").await?;
        let ladder = test_snake(&pool, user_id, "Ladder Winner").await?;
        let mixed = test_snake(&pool, user_id, "Mixed").await?;
        let solo_only = test_snake(&pool, user_id, "Solo Only").await?;
        let empty = test_snake(&pool, user_id, "Empty").await?;
        let ladder_entry = test_entry(&pool, ladder, 0).await?;
        let mixed_entry_a = test_entry(&pool, mixed, 0).await?;
        let mixed_entry_b = test_entry(&pool, mixed, 1).await?;

        let direct_win = test_game(&pool, "Standard", "finished", 15).await?;
        test_link(&pool, direct_win, Some(direct), None, Some(1)).await?;
        let direct_second = test_game(&pool, "Standard", "finished", 16).await?;
        test_link(&pool, direct_second, Some(direct), None, Some(2)).await?;
        let ladder_win = test_game(&pool, "Standard", "finished", 14).await?;
        test_link(&pool, ladder_win, None, Some(ladder_entry), Some(1)).await?;
        test_link(&pool, ladder_win, Some(mixed), None, Some(2)).await?;
        let mixed_a = test_game(&pool, "Standard", "finished", 13).await?;
        test_link(&pool, mixed_a, None, Some(mixed_entry_a), Some(2)).await?;
        let mixed_b = test_game(&pool, "Standard", "finished", 12).await?;
        test_link(&pool, mixed_b, None, Some(mixed_entry_b), Some(3)).await?;
        let both = test_game(&pool, "Standard", "finished", 11).await?;
        test_link(&pool, both, Some(mixed), Some(mixed_entry_a), Some(1)).await?;
        let duplicate = test_game(&pool, "Standard", "finished", 10).await?;
        test_link(&pool, duplicate, Some(mixed), None, Some(2)).await?;
        test_link(&pool, duplicate, Some(mixed), None, Some(3)).await?;
        let running = test_game(&pool, "Standard", "running", 9).await?;
        test_link(&pool, running, None, Some(mixed_entry_a), None).await?;
        let failed_ladder = test_game(&pool, "Standard", "failed", 8).await?;
        test_link(&pool, failed_ladder, None, Some(mixed_entry_b), None).await?;
        let failed_direct = test_game(&pool, "Standard", "failed", 7).await?;
        test_link(&pool, failed_direct, Some(mixed), None, Some(1)).await?;
        let solo = test_game(&pool, "Solo", "finished", 6).await?;
        test_link(&pool, solo, Some(mixed), None, Some(1)).await?;
        for age in [20, 21] {
            let game = test_game(&pool, "Solo", "finished", age).await?;
            test_link(&pool, game, Some(solo_only), None, Some(1)).await?;
        }
        let unplaced = test_game(&pool, "Standard", "finished", 5).await?;
        test_link(&pool, unplaced, Some(mixed), None, None).await?;

        let direct_rows = get_game_history_for_battlesnake(&pool, direct, 50, 0).await?;
        assert_eq!(direct_rows.len(), 2);
        assert_eq!(direct_rows[0].winner_name.as_deref(), Some("Direct"));
        let direct_stats = get_game_stats_for_battlesnake(&pool, direct).await?;
        assert_eq!(
            (
                direct_stats.total_games,
                direct_stats.finished_games,
                direct_stats.wins
            ),
            (2, 2, 1)
        );
        assert_eq!(
            (direct_stats.win_rate, direct_stats.average_placement),
            (50.0, 1.5)
        );

        let ladder_rows = get_game_history_for_battlesnake(&pool, ladder, 50, 0).await?;
        assert_eq!(ladder_rows.len(), 1);
        assert_eq!(ladder_rows[0].winner_name.as_deref(), Some("Ladder Winner"));
        let ladder_stats = get_game_stats_for_battlesnake(&pool, ladder).await?;
        assert_eq!((ladder_stats.total_games, ladder_stats.wins), (1, 1));

        let mixed_rows = get_game_history_for_battlesnake(&pool, mixed, 50, 0).await?;
        assert_eq!(mixed_rows.len(), 11);
        assert_eq!(mixed_rows.iter().filter(|r| r.game_id == both).count(), 1);
        assert_eq!(
            mixed_rows.iter().filter(|r| r.game_id == duplicate).count(),
            2
        );
        assert_eq!(
            mixed_rows
                .iter()
                .find(|r| r.game_id == ladder_win)
                .unwrap()
                .winner_name
                .as_deref(),
            Some("Ladder Winner")
        );
        assert!(
            mixed_rows
                .iter()
                .find(|r| r.game_id == mixed_a)
                .unwrap()
                .winner_name
                .is_none()
        );
        let stats = get_game_stats_for_battlesnake(&pool, mixed).await?;
        assert_eq!(
            (stats.total_games, stats.finished_games, stats.wins),
            (11, 7, 1)
        );
        assert_eq!(
            (stats.second_places, stats.third_places, stats.fourth_places),
            (3, 2, 0)
        );
        assert!((stats.win_rate - 100.0 / 7.0).abs() < 1e-9);
        assert!((stats.average_placement - 13.0 / 6.0).abs() < 1e-9);
        let empty_stats = get_game_stats_for_battlesnake(&pool, empty).await?;
        let solo_stats = get_game_stats_for_battlesnake(&pool, solo_only).await?;
        assert_eq!(
            (
                solo_stats.total_games,
                solo_stats.finished_games,
                solo_stats.wins
            ),
            (2, 0, 0)
        );
        assert_eq!(
            (solo_stats.win_rate, solo_stats.average_placement),
            (0.0, 0.0)
        );
        assert_eq!(
            (
                empty_stats.total_games,
                empty_stats.finished_games,
                empty_stats.win_rate
            ),
            (0, 0, 0.0)
        );
        assert!(
            get_game_history_for_battlesnake(&pool, empty, 50, 0)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn profile_paging_keeps_full_history_stats(pool: PgPool) -> cja::Result<()> {
        let user_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1482002, 'profile-paging', 'test') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let snake = test_snake(&pool, user_id, "Paged").await?;
        let entry_a = test_entry(&pool, snake, 0).await?;
        let entry_b = test_entry(&pool, snake, 1).await?;
        for n in 0..51 {
            let game = test_game(&pool, "Standard", "finished", n).await?;
            if n % 3 == 0 {
                test_link(
                    &pool,
                    game,
                    Some(snake),
                    None,
                    Some(if n == 0 { 1 } else { 2 }),
                )
                .await?;
            } else {
                test_link(
                    &pool,
                    game,
                    None,
                    Some(if n % 2 == 0 { entry_a } else { entry_b }),
                    Some(2),
                )
                .await?;
            }
        }
        let first = get_game_history_for_battlesnake(&pool, snake, 50, 0).await?;
        let last = get_game_history_for_battlesnake(&pool, snake, 50, 50).await?;
        assert_eq!((first.len(), last.len()), (50, 1));
        assert!(first.last().unwrap().created_at > last[0].created_at);
        let stats = get_game_stats_for_battlesnake(&pool, snake).await?;
        assert_eq!(
            (stats.total_games, stats.finished_games, stats.wins),
            (51, 51, 1)
        );
        assert!((stats.win_rate - 100.0 / 51.0).abs() < 1e-9);
        assert!((stats.average_placement - 101.0 / 51.0).abs() < 1e-9);
        assert_eq!(
            crate::routes::pagination::resolve_page(Some(-1), stats.total_games, 50),
            (0, 2)
        );
        assert_eq!(
            crate::routes::pagination::resolve_page(Some(999), stats.total_games, 50),
            (1, 2)
        );
        sqlx::query("UPDATE game_battlesnakes SET created_at = '2026-01-01'::timestamptz")
            .execute(&pool)
            .await?;
        let tied_first = get_game_history_for_battlesnake(&pool, snake, 50, 0).await?;
        let tied_last = get_game_history_for_battlesnake(&pool, snake, 50, 50).await?;
        let first_ids: std::collections::HashSet<_> =
            tied_first.iter().map(|row| row.game_id).collect();
        assert_eq!(first_ids.len(), 50);
        assert!(!first_ids.contains(&tied_last[0].game_id));
        Ok(())
    }
}
