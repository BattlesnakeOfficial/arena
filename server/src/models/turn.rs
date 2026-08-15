use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::game_channels::{GameChannels, TurnNotification};

/// A turn in a game with its frame data
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Turn {
    pub turn_id: Uuid,
    pub game_id: Uuid,
    pub turn_number: i32,
    pub frame_data: Option<serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Get all turns for a game, ordered by turn number
pub async fn get_turns_by_game_id(pool: &PgPool, game_id: Uuid) -> cja::Result<Vec<Turn>> {
    let turns = sqlx::query_as::<_, Turn>(
        r#"
        SELECT
            turn_id,
            game_id,
            turn_number,
            frame_data,
            created_at
        FROM turns
        WHERE game_id = $1
        ORDER BY turn_number ASC
        "#,
    )
    .bind(game_id)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch turns from database")?;

    Ok(turns)
}

/// Get a page of turns (with frame data) for a game, ordered by turn number.
///
/// Used by the engine-compatible frames API. Games can have up to ~5000
/// turns, so the limit MUST always be applied in SQL — never load all turns
/// unbounded. Turns without frame data are filtered out in SQL so that
/// `offset` indexes stably into the sequence of renderable frames (a short
/// page signals "no more frames" to clients like the GIF exporter).
pub async fn get_turn_frames_page(
    pool: &PgPool,
    game_id: Uuid,
    offset: i64,
    limit: i64,
) -> cja::Result<Vec<Turn>> {
    let turns = sqlx::query_as::<_, Turn>(
        r#"
        SELECT
            turn_id,
            game_id,
            turn_number,
            frame_data,
            created_at
        FROM turns
        WHERE game_id = $1 AND frame_data IS NOT NULL
        ORDER BY turn_number ASC
        OFFSET $2
        LIMIT $3
        "#,
    )
    .bind(game_id)
    .bind(offset)
    .bind(limit)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch turn frames page from database")?;

    Ok(turns)
}

/// Get turns for a game starting from a specific turn number
/// Used for reconnection catch-up
pub async fn get_turns_from(
    pool: &PgPool,
    game_id: Uuid,
    from_turn: i32,
) -> cja::Result<Vec<Turn>> {
    let turns = sqlx::query_as::<_, Turn>(
        r#"
        SELECT
            turn_id,
            game_id,
            turn_number,
            frame_data,
            created_at
        FROM turns
        WHERE game_id = $1 AND turn_number >= $2
        ORDER BY turn_number ASC
        "#,
    )
    .bind(game_id)
    .bind(from_turn)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch turns from database")?;

    Ok(turns)
}

/// Create a new turn for a game and notify WebSocket subscribers
pub async fn create_turn(
    pool: &PgPool,
    game_channels: &GameChannels,
    game_id: Uuid,
    turn_number: i32,
    frame_data: Option<serde_json::Value>,
) -> cja::Result<Turn> {
    let turn = sqlx::query_as::<_, Turn>(
        r#"
        INSERT INTO turns (game_id, turn_number, frame_data)
        VALUES ($1, $2, $3)
        RETURNING turn_id, game_id, turn_number, frame_data, created_at
        "#,
    )
    .bind(game_id)
    .bind(turn_number)
    .bind(frame_data)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create turn")?;

    game_channels
        .notify(TurnNotification {
            game_id,
            turn_number,
        })
        .await;

    Ok(turn)
}

/// Update turn frame data (used after computing game state)
pub async fn update_turn_frame_data(
    pool: &PgPool,
    turn_id: Uuid,
    frame_data: serde_json::Value,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE turns
        SET frame_data = $2
        WHERE turn_id = $1
        "#,
        turn_id,
        frame_data
    )
    .execute(pool)
    .await
    .wrap_err("Failed to update turn frame data")?;

    Ok(())
}

/// Survival stats for a finished Solo game, read from its final persisted
/// frame (`turns.frame_data`). `turns_survived` is the final frame's turn
/// number verbatim; `cause_of_death` is the wire-protocol elimination slug
/// from the frame's `Snakes[0].Death.Cause` (None for a snake still alive
/// at the MAX_TURNS cap, where `Death` serializes as JSON null).
#[derive(Debug, PartialEq, Eq)]
pub struct SoloGameStats {
    pub turns_survived: i32,
    pub cause_of_death: Option<String>,
}

/// Get the survival stats for a finished Solo game from its final frame.
///
/// Returns `Ok(None)` when the game has no persisted frames (archived or
/// imported finished games store their frames elsewhere). Reads the last
/// turn row by `turn_number DESC`: snakes are never removed from a board,
/// so `Snakes[0]` is the game's only snake. The nested `Death.Cause` path
/// (not the flat `EliminatedCause`, which is `""` for a live snake)
/// distinguishes "starved on turn N" from "alive at the turn cap".
pub async fn get_solo_game_stats(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<SoloGameStats>> {
    let row = sqlx::query_as!(
        SoloGameStats,
        r#"
        SELECT
            turn_number AS "turns_survived!",
            frame_data #>> '{Snakes,0,Death,Cause}' AS "cause_of_death?"
        FROM turns
        WHERE game_id = $1
          AND frame_data IS NOT NULL
        ORDER BY turn_number DESC
        LIMIT 1
        "#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch solo game stats")?;

    Ok(row)
}

/// A snake's move for a specific turn
#[derive(Debug, Serialize, Deserialize)]
pub struct SnakeTurn {
    pub snake_turn_id: Uuid,
    pub turn_id: Uuid,
    pub game_battlesnake_id: Uuid,
    pub direction: String,
    pub latency_ms: Option<i32>,
    pub timed_out: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
/// Create a snake turn record
pub async fn create_snake_turn(
    pool: &PgPool,
    turn_id: Uuid,
    game_battlesnake_id: Uuid,
    direction: &str,
    latency_ms: Option<i64>,
    timed_out: bool,
) -> cja::Result<SnakeTurn> {
    let latency_i32 = latency_ms.map(|ms| ms as i32);
    let row = sqlx::query!(
        r#"
        INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction, latency_ms, timed_out)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING snake_turn_id, turn_id, game_battlesnake_id, direction, latency_ms, timed_out, created_at
        "#,
        turn_id,
        game_battlesnake_id,
        direction,
        latency_i32,
        timed_out
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create snake turn")?;

    Ok(SnakeTurn {
        snake_turn_id: row.snake_turn_id,
        turn_id: row.turn_id,
        game_battlesnake_id: row.game_battlesnake_id,
        direction: row.direction,
        latency_ms: row.latency_ms,
        timed_out: row.timed_out,
        created_at: row.created_at,
    })
}

/// Get all snake turns for a specific turn
pub async fn get_snake_turns_by_turn_id(
    pool: &PgPool,
    turn_id: Uuid,
) -> cja::Result<Vec<SnakeTurn>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            snake_turn_id,
            turn_id,
            game_battlesnake_id,
            direction,
            latency_ms,
            timed_out,
            created_at
        FROM snake_turns
        WHERE turn_id = $1
        "#,
        turn_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch snake turns")?;

    let turns = rows
        .into_iter()
        .map(|row| SnakeTurn {
            snake_turn_id: row.snake_turn_id,
            turn_id: row.turn_id,
            game_battlesnake_id: row.game_battlesnake_id,
            direction: row.direction,
            latency_ms: row.latency_ms,
            timed_out: row.timed_out,
            created_at: row.created_at,
        })
        .collect();

    Ok(turns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turn_struct_serialization() {
        let turn = Turn {
            turn_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            game_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001").unwrap(),
            turn_number: 42,
            frame_data: Some(serde_json::json!({"test": "data"})),
            created_at: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let json = serde_json::to_string(&turn).unwrap();
        assert!(json.contains("\"turn_id\":"));
        assert!(json.contains("\"game_id\":"));
        assert!(json.contains("\"turn_number\":42"));
        assert!(json.contains("\"frame_data\":{\"test\":\"data\"}"));
    }

    #[test]
    fn test_turn_struct_deserialization() {
        let json = r#"{
            "turn_id": "550e8400-e29b-41d4-a716-446655440000",
            "game_id": "550e8400-e29b-41d4-a716-446655440001",
            "turn_number": 10,
            "frame_data": null,
            "created_at": "2024-01-01T00:00:00Z"
        }"#;

        let turn: Turn = serde_json::from_str(json).unwrap();
        assert_eq!(turn.turn_number, 10);
        assert!(turn.frame_data.is_none());
    }

    #[test]
    fn test_turn_with_frame_data() {
        let frame_data = serde_json::json!({
            "Turn": 5,
            "Snakes": [{"ID": "snake-1", "Health": 100}],
            "Food": [{"X": 5, "Y": 5}],
            "Hazards": []
        });

        let turn = Turn {
            turn_id: Uuid::new_v4(),
            game_id: Uuid::new_v4(),
            turn_number: 5,
            frame_data: Some(frame_data.clone()),
            created_at: chrono::Utc::now(),
        };

        assert_eq!(turn.frame_data.as_ref().unwrap()["Turn"], 5);
        assert!(turn.frame_data.as_ref().unwrap()["Snakes"].is_array());
    }

    #[test]
    fn test_snake_turn_struct_serialization() {
        let snake_turn = SnakeTurn {
            snake_turn_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            game_battlesnake_id: Uuid::new_v4(),
            direction: "up".to_string(),
            latency_ms: Some(123),
            timed_out: false,
            created_at: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&snake_turn).unwrap();
        assert!(json.contains("\"direction\":\"up\""));
        assert!(json.contains("\"latency_ms\":123"));
        assert!(json.contains("\"timed_out\":false"));
    }

    #[test]
    fn test_snake_turn_directions() {
        for direction in ["up", "down", "left", "right"] {
            let snake_turn = SnakeTurn {
                snake_turn_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                game_battlesnake_id: Uuid::new_v4(),
                direction: direction.to_string(),
                latency_ms: None,
                timed_out: false,
                created_at: chrono::Utc::now(),
            };
            assert_eq!(snake_turn.direction, direction);
        }
    }

    #[test]
    fn test_snake_turn_with_timeout() {
        let snake_turn = SnakeTurn {
            snake_turn_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            game_battlesnake_id: Uuid::new_v4(),
            direction: "up".to_string(),
            latency_ms: None,
            timed_out: true,
            created_at: chrono::Utc::now(),
        };
        assert!(snake_turn.timed_out);
        assert!(snake_turn.latency_ms.is_none());
    }

    // --- Solo stats (get_solo_game_stats) ---

    /// Insert a bare finished Solo game row (no snakes), mirroring what
    /// `GameType::as_str()` writes for real Solo games.
    async fn fixture_solo_game(pool: &sqlx::PgPool) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Solo', 'finished') RETURNING game_id",
        )
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    async fn fixture_turn(
        pool: &sqlx::PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: Option<serde_json::Value>,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn_number)
            .bind(frame_data)
            .execute(pool)
            .await?;
        Ok(())
    }

    fn frame(turn: i32, death_cause: Option<&str>) -> serde_json::Value {
        let death = match death_cause {
            Some(cause) => serde_json::json!({"Cause": cause, "Turn": turn, "EliminatedBy": ""}),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": "snake-1",
                "Name": "Solo Snake",
                "Body": [{"X": 5, "Y": 5}],
                "Health": 0,
                "Death": death,
                "EliminatedCause": death_cause.unwrap_or(""),
                "EliminatedBy": "",
            }],
            "Food": [],
            "Hazards": [],
        })
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn solo_stats_reads_death_cause_from_final_frame(pool: sqlx::PgPool) -> cja::Result<()> {
        let game_id = fixture_solo_game(&pool).await?;
        fixture_turn(&pool, game_id, 0, Some(frame(0, None))).await?;
        fixture_turn(&pool, game_id, 41, Some(frame(41, None))).await?;
        fixture_turn(&pool, game_id, 42, Some(frame(42, Some("out-of-health")))).await?;

        let stats = get_solo_game_stats(&pool, game_id)
            .await?
            .expect("stats present");
        assert_eq!(stats.turns_survived, 42);
        assert_eq!(stats.cause_of_death.as_deref(), Some("out-of-health"));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn solo_stats_max_turns_frame_has_no_death_cause(pool: sqlx::PgPool) -> cja::Result<()> {
        let game_id = fixture_solo_game(&pool).await?;
        fixture_turn(&pool, game_id, 5000, Some(frame(5000, None))).await?;

        let stats = get_solo_game_stats(&pool, game_id)
            .await?
            .expect("stats present");
        assert_eq!(stats.turns_survived, 5000);
        assert_eq!(
            stats.cause_of_death, None,
            "alive at the cap: Death is null"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn solo_stats_missing_frames_returns_none(pool: sqlx::PgPool) -> cja::Result<()> {
        let game_id = fixture_solo_game(&pool).await?;

        let stats = get_solo_game_stats(&pool, game_id).await?;
        assert_eq!(stats, None);
        Ok(())
    }
}
