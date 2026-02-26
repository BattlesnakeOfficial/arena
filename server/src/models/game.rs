use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, PgPool, Postgres};
use std::str::FromStr;
use uuid::Uuid;

use super::game_battlesnake::AddBattlesnakeToGame;

// Game board size enum
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum GameBoardSize {
    Small,  // 7x7
    Medium, // 11x11
    Large,  // 19x19
}

impl GameBoardSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            GameBoardSize::Small => "7x7",
            GameBoardSize::Medium => "11x11",
            GameBoardSize::Large => "19x19",
        }
    }

    /// Returns the (width, height) dimensions of the board
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            GameBoardSize::Small => (7, 7),
            GameBoardSize::Medium => (11, 11),
            GameBoardSize::Large => (19, 19),
        }
    }
}

impl FromStr for GameBoardSize {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "7x7" => Ok(GameBoardSize::Small),
            "11x11" => Ok(GameBoardSize::Medium),
            "19x19" => Ok(GameBoardSize::Large),
            _ => Err(color_eyre::eyre::eyre!("Invalid board size: {}", s)),
        }
    }
}

// Game type enum
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum GameType {
    Standard,
    Royale,
    Constrictor,
    SnailMode,
}

impl GameType {
    pub fn as_str(&self) -> &'static str {
        match self {
            GameType::Standard => "Standard",
            GameType::Royale => "Royale",
            GameType::Constrictor => "Constrictor",
            GameType::SnailMode => "Snail Mode",
        }
    }
}

impl FromStr for GameType {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Standard" => Ok(GameType::Standard),
            "Royale" => Ok(GameType::Royale),
            "Constrictor" => Ok(GameType::Constrictor),
            "Snail Mode" => Ok(GameType::SnailMode),
            _ => Err(color_eyre::eyre::eyre!("Invalid game type: {}", s)),
        }
    }
}

// Game status enum
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum GameStatus {
    Waiting,
    Running,
    Finished,
}

impl GameStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GameStatus::Waiting => "waiting",
            GameStatus::Running => "running",
            GameStatus::Finished => "finished",
        }
    }
}

impl FromStr for GameStatus {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "waiting" => Ok(GameStatus::Waiting),
            "running" => Ok(GameStatus::Running),
            "finished" => Ok(GameStatus::Finished),
            _ => Err(color_eyre::eyre::eyre!("Invalid game status: {}", s)),
        }
    }
}

// Game model for our application
#[derive(Debug, Serialize, Deserialize)]
pub struct Game {
    pub game_id: Uuid,
    pub board_size: GameBoardSize,
    pub game_type: GameType,
    pub status: GameStatus,
    pub enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// For creating a new game
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateGame {
    pub board_size: GameBoardSize,
    pub game_type: GameType,
}

// Create a game with battlesnakes in a single transaction
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateGameWithSnakes {
    pub board_size: GameBoardSize,
    pub game_type: GameType,
    pub battlesnake_ids: Vec<Uuid>,
}

// Struct to hold the game with winner query result
#[derive(Debug)]
struct GameWithWinnerRow {
    game_id: Uuid,
    board_size: String,
    game_type: String,
    status: String,
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    winner_name: Option<String>,
}

// Database functions for game management

// Get all games
pub async fn get_all_games(pool: &PgPool) -> cja::Result<Vec<Game>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            game_id,
            board_size,
            game_type,
            status,
            enqueued_at,
            created_at,
            updated_at
        FROM games
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch games from database")?;

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

// Get a single game by ID
pub async fn get_game_by_id(pool: &PgPool, game_id: Uuid) -> cja::Result<Option<Game>> {
    let row = sqlx::query!(
        r#"
        SELECT
            game_id,
            board_size,
            game_type,
            status,
            enqueued_at,
            created_at,
            updated_at
        FROM games
        WHERE game_id = $1
        "#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch game from database")?;

    let game = match row {
        Some(row) => {
            let board_size = GameBoardSize::from_str(&row.board_size)
                .wrap_err_with(|| format!("Invalid board size: {}", row.board_size))?;
            let game_type = GameType::from_str(&row.game_type)
                .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
            let status = GameStatus::from_str(&row.status)
                .wrap_err_with(|| format!("Invalid game status: {}", row.status))?;

            Some(Game {
                game_id: row.game_id,
                board_size,
                game_type,
                status,
                enqueued_at: row.enqueued_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        }
        None => None,
    };

    Ok(game)
}

// Delete a game
pub async fn delete_game(pool: &PgPool, game_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        r#"
        DELETE FROM games
        WHERE game_id = $1
        "#,
        game_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to delete game from database")?;

    Ok(())
}

// Create a new game with all battlesnakes in a single transaction
pub async fn create_game_with_snakes(
    pool: &PgPool,
    data: CreateGameWithSnakes,
) -> cja::Result<Game> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start database transaction")?;

    let game = create_game_with_snakes_tx(&mut tx, data).await?;

    tx.commit()
        .await
        .wrap_err("Failed to commit database transaction")?;

    Ok(game)
}

/// Create a game with battlesnakes using a mutable connection reference.
/// Use this when you need to compose game creation with other operations in a single transaction.
pub async fn create_game_with_snakes_tx(
    conn: &mut sqlx::PgConnection,
    data: CreateGameWithSnakes,
) -> cja::Result<Game> {
    // Validate number of battlesnakes
    if data.battlesnake_ids.is_empty() {
        return Err(cja::color_eyre::eyre::eyre!(
            "At least one battlesnake is required for a game"
        ));
    }

    if data.battlesnake_ids.len() > 4 {
        return Err(cja::color_eyre::eyre::eyre!(
            "A maximum of 4 battlesnakes are allowed in a game"
        ));
    }

    // Create the game
    let game = create_game(
        &mut *conn,
        CreateGame {
            board_size: data.board_size,
            game_type: data.game_type,
        },
    )
    .await
    .wrap_err("Failed to create game in database")?;

    // Add each battlesnake to the game
    for battlesnake_id in data.battlesnake_ids {
        add_battlesnake_to_game(
            &mut *conn,
            game.game_id,
            AddBattlesnakeToGame { battlesnake_id },
        )
        .await
        .wrap_err_with(|| format!("Failed to add battlesnake {} to game", battlesnake_id))?;
    }

    Ok(game)
}

// Generic function to create a game with any executor
pub async fn create_game<'e, E>(executor: E, data: CreateGame) -> cja::Result<Game>
where
    E: Executor<'e, Database = Postgres>,
{
    let board_size_str = data.board_size.as_str();
    let game_type_str = data.game_type.as_str();
    let status_str = GameStatus::Waiting.as_str();

    let row = sqlx::query!(
        r#"
        INSERT INTO games (
            board_size,
            game_type,
            status
        )
        VALUES ($1, $2, $3)
        RETURNING
            game_id,
            board_size,
            game_type,
            status,
            enqueued_at,
            created_at,
            updated_at
        "#,
        board_size_str,
        game_type_str,
        status_str
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create game in database")?;

    Ok(Game {
        game_id: row.game_id,
        board_size: data.board_size,
        game_type: data.game_type,
        status: GameStatus::from_str(&row.status)
            .wrap_err_with(|| format!("Invalid game status: {}", row.status))?,
        enqueued_at: row.enqueued_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

// Generic function to add a battlesnake to a game with any executor
pub async fn add_battlesnake_to_game<'e, E>(
    executor: E,
    game_id: Uuid,
    data: AddBattlesnakeToGame,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"
        INSERT INTO game_battlesnakes (
            game_id,
            battlesnake_id
        )
        VALUES ($1, $2)
        "#,
        game_id,
        data.battlesnake_id
    )
    .execute(executor)
    .await
    .wrap_err_with(|| format!("Failed to add battlesnake {} to game", data.battlesnake_id))?;

    Ok(())
}

/// Add a leaderboard entry to a game without redundantly copying battlesnake_id.
/// The effective battlesnake is resolved via JOIN with leaderboard_entries when needed.
/// Use this instead of `add_battlesnake_to_game` when creating leaderboard games.
pub async fn add_leaderboard_entry_to_game<'e, E>(
    executor: E,
    game_id: Uuid,
    leaderboard_entry_id: Uuid,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"
        INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id)
        VALUES ($1, $2)
        "#,
        game_id,
        leaderboard_entry_id
    )
    .execute(executor)
    .await
    .wrap_err_with(|| {
        format!("Failed to add leaderboard entry {leaderboard_entry_id} to game {game_id}")
    })?;

    Ok(())
}

// Update the status of a game
pub async fn update_game_status(
    pool: &PgPool,
    game_id: Uuid,
    status: GameStatus,
) -> cja::Result<Game> {
    let status_str = status.as_str();

    let row = sqlx::query!(
        r#"
        UPDATE games
        SET status = $2
        WHERE game_id = $1
        RETURNING
            game_id,
            board_size,
            game_type,
            status,
            enqueued_at,
            created_at,
            updated_at
        "#,
        game_id,
        status_str
    )
    .fetch_one(pool)
    .await
    .wrap_err_with(|| format!("Failed to update status for game {}", game_id))?;

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
}

// Set the enqueued_at timestamp for a game
pub async fn set_game_enqueued_at(
    pool: &PgPool,
    game_id: Uuid,
    enqueued_at: chrono::DateTime<chrono::Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE games
        SET enqueued_at = $2
        WHERE game_id = $1
        "#,
        game_id,
        enqueued_at
    )
    .execute(pool)
    .await
    .wrap_err_with(|| format!("Failed to set enqueued_at for game {}", game_id))?;

    Ok(())
}

/// Set the enqueued_at timestamp using a mutable connection reference (for transaction composition).
pub async fn set_game_enqueued_at_tx(
    conn: &mut sqlx::PgConnection,
    game_id: Uuid,
    enqueued_at: chrono::DateTime<chrono::Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE games
        SET enqueued_at = $2
        WHERE game_id = $1
        "#,
        game_id,
        enqueued_at
    )
    .execute(&mut *conn)
    .await
    .wrap_err_with(|| format!("Failed to set enqueued_at for game {}", game_id))?;

    Ok(())
}

// Get all games with their winners (if available)
pub async fn get_all_games_with_winners(pool: &PgPool) -> cja::Result<Vec<(Game, Option<String>)>> {
    let rows = sqlx::query_as!(
        GameWithWinnerRow,
        r#"
        SELECT
            g.game_id,
            g.board_size,
            g.game_type,
            g.status,
            g.enqueued_at,
            g.created_at,
            g.updated_at,
            b.name as "winner_name?"
        FROM games g
        LEFT JOIN game_battlesnakes gb ON g.game_id = gb.game_id AND gb.placement = 1
        LEFT JOIN battlesnakes b ON gb.battlesnake_id = b.battlesnake_id
        ORDER BY g.created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch games with winners from database")?;

    let games_with_winners = rows
        .into_iter()
        .map(|row| {
            let board_size = GameBoardSize::from_str(&row.board_size)
                .wrap_err_with(|| format!("Invalid board size: {}", row.board_size))?;
            let game_type = GameType::from_str(&row.game_type)
                .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
            let status = GameStatus::from_str(&row.status)
                .wrap_err_with(|| format!("Invalid game status: {}", row.status))?;

            let game = Game {
                game_id: row.game_id,
                board_size,
                game_type,
                status,
                enqueued_at: row.enqueued_at,
                created_at: row.created_at,
                updated_at: row.updated_at,
            };

            Ok((game, row.winner_name))
        })
        .collect::<cja::Result<Vec<_>>>()?;

    Ok(games_with_winners)
}
