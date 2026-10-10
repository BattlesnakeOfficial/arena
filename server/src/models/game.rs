use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, PgPool, Postgres};
use std::collections::HashSet;
use std::str::FromStr;
use uuid::Uuid;

use super::battlesnake;
use super::game_battlesnake::AddBattlesnakeToGame;

#[derive(Debug, thiserror::Error)]
#[error("one or more battlesnakes are unavailable")]
pub struct InaccessibleBattlesnake;

#[derive(Debug, Clone)]
pub struct GameRematchMetadata {
    pub created_by_user_id: Option<Uuid>,
    pub rematch_battlesnake_ids: Option<Vec<Uuid>>,
}

// Game board size enum
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum GameBoardSize {
    Small,  // 7x7
    Medium, // 11x11
    Large,  // 19x19
    Custom(String),
}

impl GameBoardSize {
    pub fn as_str(&self) -> &str {
        match self {
            GameBoardSize::Small => "7x7",
            GameBoardSize::Medium => "11x11",
            GameBoardSize::Large => "19x19",
            GameBoardSize::Custom(s) => s,
        }
    }

    /// Returns the (width, height) dimensions of the board
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            GameBoardSize::Small => (7, 7),
            GameBoardSize::Medium => (11, 11),
            GameBoardSize::Large => (19, 19),
            GameBoardSize::Custom(s) => {
                let parts: Vec<&str> = s.split('x').collect();
                if parts.len() == 2
                    && let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse())
                {
                    return (w, h);
                }
                (11, 11) // Fallback for malformed custom sizes
            }
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
            _ => Ok(GameBoardSize::Custom(s.to_string())),
        }
    }
}

// Game type enum
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum GameType {
    Standard,
    Royale,
    Constrictor,
    SnailMode,
    Solo,
    Other(String),
}

impl GameType {
    pub fn as_str(&self) -> &str {
        match self {
            GameType::Standard => "Standard",
            GameType::Royale => "Royale",
            GameType::Constrictor => "Constrictor",
            GameType::SnailMode => "Snail Mode",
            GameType::Solo => "Solo",
            GameType::Other(s) => s,
        }
    }

    /// Whether this mode puts food on the board. Constrictor never spawns
    /// any, so food stats there are always zero.
    pub fn has_food(&self) -> bool {
        !matches!(self, GameType::Constrictor)
    }
}

impl FromStr for GameType {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "standard" => Ok(GameType::Standard),
            "royale" => Ok(GameType::Royale),
            "constrictor" => Ok(GameType::Constrictor),
            "snail mode" | "snail_mode" => Ok(GameType::SnailMode),
            "solo" => Ok(GameType::Solo),
            _ => Ok(GameType::Other(s.to_string())),
        }
    }
}

// Game status enum
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum GameStatus {
    Waiting,
    Running,
    /// Terminal success: the game ran to completion and has results.
    Finished,
    /// Terminal failure: the runner died and never finished the game (e.g.
    /// OOM-killed worker whose job exhausted its retries). Failed games have
    /// no results and never affect ratings; without this state they sat in
    /// `running` forever and counted as live.
    Failed,
}

impl GameStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GameStatus::Waiting => "waiting",
            GameStatus::Running => "running",
            GameStatus::Finished => "finished",
            GameStatus::Failed => "failed",
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
            "failed" => Ok(GameStatus::Failed),
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

/// Validate the number of battlesnakes for a game type.
///
/// Shared by the web flow, the API handler, and the transactional create
/// path. Solo games require exactly one battlesnake (survival mode); every
/// other mode requires 1-4.
pub fn validate_battlesnake_count(game_type: &GameType, count: usize) -> cja::Result<()> {
    if count == 0 {
        return Err(cja::color_eyre::eyre::eyre!(
            "At least one battlesnake is required for a game"
        ));
    }

    if matches!(game_type, GameType::Solo) && count != 1 {
        return Err(cja::color_eyre::eyre::eyre!(
            "Solo games require exactly one battlesnake"
        ));
    }

    if count > 4 {
        return Err(cja::color_eyre::eyre::eyre!(
            "A maximum of 4 battlesnakes are allowed in a game"
        ));
    }

    Ok(())
}

// Database functions for game management

/// Result of atomically moving a waiting game into the running state.
#[derive(Debug, PartialEq, Eq)]
pub enum StartClaim {
    Started {
        leaderboard_id: Option<Uuid>,
        wait_ms: i64,
        via: Option<&'static str>,
        /// Read under the claim's row lock, so a dispatcher that stamped it in a
        /// still-open transaction is waited for rather than read stale.
        enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    Busy,
    AlreadyRunning,
    Terminal,
}

#[derive(Debug, Clone)]
pub(crate) struct ScheduleGame {
    pub game_id: Uuid,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub leaderboard_id: Option<Uuid>,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub participants: Vec<Uuid>,
}

#[derive(Debug)]
pub(crate) struct ScheduleSnapshot {
    pub at: chrono::DateTime<chrono::Utc>,
    pub games: Vec<ScheduleGame>,
}

/// One view of all running games and waiting ladder games. Caller-held participant
/// locks make this authoritative for a claim; dispatch uses it only as a hint.
pub(crate) async fn load_schedule_snapshot(
    conn: &mut sqlx::PgConnection,
) -> cja::Result<ScheduleSnapshot> {
    let rows = sqlx::query!(
        r#"SELECT g.game_id, g.status, g.created_at, lg.leaderboard_id,
                  lb.disabled_at,
                  ARRAY(SELECT DISTINCT COALESCE(gb.battlesnake_id, le.battlesnake_id)
                        FROM game_battlesnakes gb
                        LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
                        WHERE gb.game_id = g.game_id AND COALESCE(gb.battlesnake_id, le.battlesnake_id) IS NOT NULL
                        ORDER BY COALESCE(gb.battlesnake_id, le.battlesnake_id)) AS "participants!: Vec<Uuid>"
           FROM games g
           LEFT JOIN leaderboard_games lg ON lg.game_id = g.game_id
           LEFT JOIN leaderboards lb ON lb.leaderboard_id = lg.leaderboard_id
           WHERE g.status = 'running' OR (g.status = 'waiting' AND lg.game_id IS NOT NULL)"#
    )
    .fetch_all(&mut *conn)
    .await
    .wrap_err("Failed to load game schedule")?;
    let at = sqlx::query_scalar!(r#"SELECT clock_timestamp() AS "at!""#)
        .fetch_one(&mut *conn)
        .await
        .wrap_err("Failed to read schedule clock")?;
    Ok(ScheduleSnapshot {
        at,
        games: rows
            .into_iter()
            .map(|r| ScheduleGame {
                game_id: r.game_id,
                status: r.status,
                created_at: r.created_at,
                leaderboard_id: r.leaderboard_id,
                disabled_at: r.disabled_at,
                participants: r.participants,
            })
            .collect(),
    })
}

fn shares_snake(a: &ScheduleGame, b: &ScheduleGame) -> bool {
    let ids: HashSet<_> = a.participants.iter().collect();
    b.participants.iter().any(|id| ids.contains(id))
}

/// Eligibility before FIFO, including the non-overridable same-ladder rule.
fn base_eligibility(
    snapshot: &ScheduleSnapshot,
    game: &ScheduleGame,
    deadline_secs: i64,
) -> Option<&'static str> {
    if game.status != "waiting"
        || game.leaderboard_id.is_none()
        || game.disabled_at.is_some()
        || game.participants.is_empty()
    {
        return None;
    }
    let conflicts: Vec<_> = snapshot
        .games
        .iter()
        .filter(|other| other.status == "running" && shares_snake(game, other))
        .collect();
    if conflicts
        .iter()
        .any(|other| other.leaderboard_id == game.leaderboard_id)
    {
        return None;
    }
    if conflicts.is_empty() {
        return Some("free");
    }
    if snapshot
        .at
        .signed_duration_since(game.created_at)
        .num_seconds()
        >= deadline_secs
    {
        Some("deadline")
    } else {
        None
    }
}

/// A waiting ladder game past the deadline can only be blocked by same-ladder
/// running games. It reserves its snakes against younger games of its own
/// ladder; otherwise each new round could keep taking its free snakes and
/// starve it (the stuck-game sweeper exempts it while it's held).
fn reserves_same_ladder(
    snapshot: &ScheduleSnapshot,
    game: &ScheduleGame,
    deadline_secs: i64,
) -> bool {
    game.status == "waiting"
        && game.leaderboard_id.is_some()
        && game.disabled_at.is_none()
        && !game.participants.is_empty()
        && snapshot
            .at
            .signed_duration_since(game.created_at)
            .num_seconds()
            >= deadline_secs
}

/// Oldest base-eligible game sharing a participant wins this dispatch cycle,
/// and a past-deadline game holds its snakes against younger same-ladder games.
pub(crate) fn ladder_eligibility(
    snapshot: &ScheduleSnapshot,
    game: &ScheduleGame,
    deadline_secs: i64,
) -> Option<&'static str> {
    let via = base_eligibility(snapshot, game, deadline_secs)?;
    let older = snapshot.games.iter().filter(|other| {
        other.game_id != game.game_id
            && (other.created_at, other.game_id) < (game.created_at, game.game_id)
            && shares_snake(game, other)
    });
    if older.into_iter().any(|other| {
        base_eligibility(snapshot, other, deadline_secs).is_some()
            || (other.leaderboard_id == game.leaderboard_id
                && reserves_same_ladder(snapshot, other, deadline_secs))
    }) {
        None
    } else {
        Some(via)
    }
}

/// Claim exactly one waiting game before any snake HTTP request.
pub async fn claim_game_start(
    pool: &PgPool,
    game_id: Uuid,
    deadline_secs: i64,
) -> cja::Result<StartClaim> {
    use color_eyre::eyre::eyre;
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin game start claim")?;
    let target = sqlx::query!(
        "SELECT status, created_at, enqueued_at FROM games WHERE game_id = $1 FOR UPDATE",
        game_id
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to lock game for start")?;
    match target.status.as_str() {
        "running" => return Ok(StartClaim::AlreadyRunning),
        "finished" | "failed" => return Ok(StartClaim::Terminal),
        "waiting" => {}
        other => return Err(eyre!("Unknown game status {other}")),
    }
    let roster = sqlx::query_scalar!(
        r#"SELECT DISTINCT COALESCE(gb.battlesnake_id, le.battlesnake_id) AS "snake_id!"
           FROM game_battlesnakes gb
           LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
           WHERE gb.game_id = $1
           ORDER BY "snake_id!""#,
        game_id
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to resolve game roster")?;
    if roster.is_empty() {
        return Err(eyre!("Game {game_id} has empty roster"));
    }
    for snake_id in &roster {
        let found = sqlx::query_scalar!(
            "SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 FOR NO KEY UPDATE",
            snake_id
        )
        .fetch_optional(&mut *tx)
        .await
        .wrap_err("Failed to lock game participant")?;
        if found.is_none() {
            return Err(eyre!(
                "Game {game_id} has unresolved participant {snake_id}"
            ));
        }
    }
    let snapshot = load_schedule_snapshot(&mut tx)
        .await
        .wrap_err("Failed to recheck schedule under participant locks")?;
    let target_game = snapshot.games.iter().find(|g| g.game_id == game_id);
    let (leaderboard_id, via) = if let Some(game) = target_game {
        (
            game.leaderboard_id,
            ladder_eligibility(&snapshot, game, deadline_secs),
        )
    } else {
        (None, None)
    };
    if leaderboard_id.is_some() && via.is_none() {
        return Ok(StartClaim::Busy);
    }
    let updated = sqlx::query_scalar!("UPDATE games SET status = 'running' WHERE game_id = $1 AND status = 'waiting' RETURNING game_id", game_id)
        .fetch_optional(&mut *tx).await.wrap_err("Failed to start claimed game")?;
    if updated.is_none() {
        return Ok(StartClaim::AlreadyRunning);
    }
    let wait_ms = snapshot
        .at
        .signed_duration_since(target.created_at)
        .num_milliseconds()
        .max(0);
    tx.commit()
        .await
        .wrap_err("Failed to commit game start claim")?;
    Ok(StartClaim::Started {
        leaderboard_id,
        wait_ms,
        via,
        enqueued_at: target.enqueued_at,
    })
}

#[cfg(test)]
mod ladder_dispatch_tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn game(
        id: u128,
        status: &str,
        age: i64,
        ladder: Option<Uuid>,
        snakes: &[Uuid],
    ) -> ScheduleGame {
        ScheduleGame {
            game_id: Uuid::from_u128(id),
            status: status.into(),
            created_at: Utc::now() - Duration::seconds(age),
            leaderboard_id: ladder,
            disabled_at: None,
            participants: snakes.to_vec(),
        }
    }

    #[test]
    fn busy_deadline_and_same_ladder_hold() {
        let snake = Uuid::from_u128(1);
        let ladder = Uuid::from_u128(2);
        let running = game(10, "running", 20, None, &[snake]);
        let waiting = game(11, "waiting", 479, Some(ladder), &[snake]);
        let mut snapshot = ScheduleSnapshot {
            at: Utc::now(),
            games: vec![running, waiting],
        };
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[1], 480), None);
        snapshot.games[1].created_at = snapshot.at - Duration::seconds(480);
        assert_eq!(
            ladder_eligibility(&snapshot, &snapshot.games[1], 480),
            Some("deadline")
        );
        snapshot.games[0].leaderboard_id = Some(ladder);
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[1], 480), None);
    }

    #[test]
    fn oldest_base_eligible_wins_without_reserving_blocked_snakes() {
        let snake = Uuid::from_u128(1);
        let ladder = Uuid::from_u128(2);
        let mut snapshot = ScheduleSnapshot {
            at: Utc::now(),
            games: vec![
                game(1, "waiting", 20, Some(ladder), &[snake]),
                game(2, "waiting", 10, Some(ladder), &[snake]),
            ],
        };
        assert_eq!(
            ladder_eligibility(&snapshot, &snapshot.games[0], 480),
            Some("free")
        );
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[1], 480), None);
        snapshot
            .games
            .push(game(3, "running", 30, Some(ladder), &[snake]));
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[1], 480), None);
        snapshot.games[0].disabled_at = Some(snapshot.at);
        snapshot.games[2].leaderboard_id = None;
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[1], 480), None);
        snapshot.games[2].participants.clear();
        assert_eq!(
            ladder_eligibility(&snapshot, &snapshot.games[1], 480),
            Some("free")
        );
    }

    #[test]
    fn equal_creation_time_uses_game_id_for_fifo() {
        let snake = Uuid::from_u128(1);
        let ladder = Uuid::from_u128(2);
        let at = Utc::now();
        let mut younger_id = game(20, "waiting", 0, Some(ladder), &[snake]);
        let mut older_id = game(10, "waiting", 0, Some(ladder), &[snake]);
        younger_id.created_at = at;
        older_id.created_at = at;
        let snapshot = ScheduleSnapshot {
            at,
            games: vec![younger_id, older_id],
        };
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[0], 480), None);
        assert_eq!(
            ladder_eligibility(&snapshot, &snapshot.games[1], 480),
            Some("free")
        );
    }

    #[test]
    fn blocked_older_game_does_not_reserve_free_participant() {
        let shared = Uuid::from_u128(1);
        let blocked = Uuid::from_u128(2);
        let ladder = Uuid::from_u128(3);
        let snapshot = ScheduleSnapshot {
            at: Utc::now(),
            games: vec![
                game(10, "waiting", 30, Some(ladder), &[shared, blocked]),
                game(11, "waiting", 20, Some(ladder), &[shared]),
                game(12, "running", 40, Some(ladder), &[blocked]),
            ],
        };
        assert_eq!(ladder_eligibility(&snapshot, &snapshot.games[0], 480), None);
        assert_eq!(
            ladder_eligibility(&snapshot, &snapshot.games[1], 480),
            Some("free")
        );
    }

    #[test]
    fn past_deadline_blocked_game_reserves_snakes_against_its_own_ladder() {
        let shared = Uuid::from_u128(1);
        let blocked = Uuid::from_u128(2);
        let ladder = Uuid::from_u128(3);
        let other_ladder = Uuid::from_u128(4);
        let held = game(10, "waiting", 500, Some(ladder), &[shared, blocked]);
        let blocker = game(12, "running", 600, Some(ladder), &[blocked]);
        let same_ladder = ScheduleSnapshot {
            at: Utc::now(),
            games: vec![
                held.clone(),
                game(11, "waiting", 20, Some(ladder), &[shared]),
                blocker.clone(),
            ],
        };
        assert_eq!(
            ladder_eligibility(&same_ladder, &same_ladder.games[0], 480),
            None
        );
        assert_eq!(
            ladder_eligibility(&same_ladder, &same_ladder.games[1], 480),
            None,
            "a younger same-ladder game must not take a past-deadline game's snake"
        );
        let other = ScheduleSnapshot {
            at: Utc::now(),
            games: vec![
                held,
                blocker,
                game(13, "waiting", 20, Some(other_ladder), &[shared]),
            ],
        };
        assert_eq!(
            ladder_eligibility(&other, &other.games[2], 480),
            Some("free"),
            "the reservation is same-ladder only; other ladders may still use the snake"
        );
    }
}

/// Where a game came from, sent to snakes as `game.source`. Matchmaker and
/// tournament games link their `leaderboard_games` / `match_games` row in the
/// same transaction that creates the game, so the link exists before the
/// game runner starts.
pub async fn get_game_source(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<crate::engine::GameSource> {
    let row = sqlx::query!(
        r#"
        SELECT
            EXISTS (SELECT 1 FROM leaderboard_games WHERE game_id = $1) AS "ladder!",
            EXISTS (SELECT 1 FROM match_games WHERE game_id = $1) AS "tournament!"
        "#,
        game_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to look up game source")?;

    Ok(if row.ladder {
        crate::engine::GameSource::Ladder
    } else if row.tournament {
        crate::engine::GameSource::Tournament
    } else {
        crate::engine::GameSource::Custom
    })
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

pub async fn get_game_rematch_metadata(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<GameRematchMetadata>> {
    let row = sqlx::query!(
        "SELECT created_by_user_id, rematch_battlesnake_ids FROM games WHERE game_id = $1",
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch game rematch metadata")?;
    Ok(row.map(|row| GameRematchMetadata {
        created_by_user_id: row.created_by_user_id,
        rematch_battlesnake_ids: row.rematch_battlesnake_ids,
    }))
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

/// Create a user-owned custom game. Eligibility is enforced while shared row
/// locks are held through commit; route-level checks are only UX hints.
pub async fn create_game_with_snakes_for_user(
    pool: &PgPool,
    data: CreateGameWithSnakes,
    user_id: Uuid,
) -> cja::Result<Game> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start database transaction")?;
    validate_battlesnake_count(&data.game_type, data.battlesnake_ids.len())?;

    let original_ids = data.battlesnake_ids.clone();
    let mut lock_ids = original_ids.clone();
    lock_ids.sort();
    lock_ids.dedup();
    let eligible = battlesnake::lock_eligible_battlesnake_ids(&mut tx, user_id, &lock_ids).await?;
    if lock_ids.iter().any(|id| !eligible.contains(id)) {
        return Err(InaccessibleBattlesnake.into());
    }

    let row = sqlx::query!(
        r#"INSERT INTO games
           (board_size, game_type, status, created_by_user_id, rematch_battlesnake_ids)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING game_id, status, enqueued_at, created_at, updated_at"#,
        data.board_size.as_str(),
        data.game_type.as_str(),
        GameStatus::Waiting.as_str(),
        user_id,
        &original_ids
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to create owned game")?;

    for battlesnake_id in original_ids {
        add_battlesnake_to_game(
            &mut *tx,
            row.game_id,
            AddBattlesnakeToGame { battlesnake_id },
        )
        .await?;
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit database transaction")?;
    Ok(Game {
        game_id: row.game_id,
        board_size: data.board_size,
        game_type: data.game_type,
        status: GameStatus::from_str(&row.status)?,
        enqueued_at: row.enqueued_at,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

/// Create a game with battlesnakes using a mutable connection reference.
/// Use this when you need to compose game creation with other operations in a single transaction.
pub async fn create_game_with_snakes_tx(
    conn: &mut sqlx::PgConnection,
    data: CreateGameWithSnakes,
) -> cja::Result<Game> {
    // Validate number of battlesnakes for this game type
    validate_battlesnake_count(&data.game_type, data.battlesnake_ids.len())?;

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

/// Update a game's status inside an existing transaction. Used by the game
/// runner's atomic finish sequence, where the status flip must commit together
/// with the placements and the tournament match result.
pub async fn update_game_status_tx(
    conn: &mut sqlx::PgConnection,
    game_id: Uuid,
    status: GameStatus,
) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE games SET status = $2 WHERE game_id = $1",
        game_id,
        status.as_str(),
    )
    .execute(&mut *conn)
    .await
    .wrap_err_with(|| format!("Failed to update status for game {game_id}"))?;

    Ok(())
}

/// Bump a game's `updated_at` without changing anything else (the
/// `update_games_updated_at` trigger stamps `NOW()` on any UPDATE). Used to
/// mark a stalled game as "being handled" when its runner job is re-enqueued,
/// so overlapping staleness checks don't enqueue duplicate runners.
pub async fn touch_game_updated_at(pool: &PgPool, game_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE games SET updated_at = NOW() WHERE game_id = $1",
        game_id
    )
    .execute(pool)
    .await
    .wrap_err_with(|| format!("Failed to touch updated_at for game {game_id}"))?;

    Ok(())
}

/// Latest sign of life for a game: its `updated_at` (bumped on status changes
/// and explicit touches) or the `created_at` of its most recently persisted
/// turn, whichever is later. `None` if the game does not exist. Used to judge
/// whether an in-flight game has stalled (its runner job died).
pub async fn get_game_last_activity(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let last_activity = sqlx::query_scalar!(
        r#"
        SELECT GREATEST(
            g.updated_at,
            (SELECT MAX(t.created_at) FROM turns t WHERE t.game_id = g.game_id)
        ) as "last_activity!"
        FROM games g
        WHERE g.game_id = $1
        "#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch game last activity")?;

    Ok(last_activity)
}

/// Wipe the per-game state a previous (crashed) run left behind so `run_game`
/// can restart cleanly from turn 0: turns (snake_turns cascade with them) and
/// any partially written placements. Runs in a single transaction.
pub async fn reset_game_state_for_retry(pool: &PgPool, game_id: Uuid) -> cja::Result<()> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start game reset transaction")?;

    sqlx::query!("DELETE FROM turns WHERE game_id = $1", game_id)
        .execute(&mut *tx)
        .await
        .wrap_err_with(|| format!("Failed to delete turns for game {game_id} reset"))?;

    sqlx::query!(
        "UPDATE game_battlesnakes SET placement = NULL, food_eaten = NULL WHERE game_id = $1",
        game_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err_with(|| format!("Failed to clear results for game {game_id} reset"))?;

    tx.commit()
        .await
        .wrap_err("Failed to commit game reset transaction")?;

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_status_round_trips_through_strings() {
        for status in [
            GameStatus::Waiting,
            GameStatus::Running,
            GameStatus::Finished,
            GameStatus::Failed,
        ] {
            assert_eq!(GameStatus::from_str(status.as_str()).unwrap(), status);
        }
        assert!(GameStatus::from_str("exploded").is_err());
    }

    #[test]
    fn game_type_from_str_case_insensitive() {
        assert_eq!(GameType::from_str("Standard").unwrap(), GameType::Standard);
        assert_eq!(GameType::from_str("standard").unwrap(), GameType::Standard);
        assert_eq!(GameType::from_str("STANDARD").unwrap(), GameType::Standard);
        assert_eq!(GameType::from_str("royale").unwrap(), GameType::Royale);
        assert_eq!(GameType::from_str("Royale").unwrap(), GameType::Royale);
        assert_eq!(
            GameType::from_str("constrictor").unwrap(),
            GameType::Constrictor
        );
        assert_eq!(
            GameType::from_str("snail mode").unwrap(),
            GameType::SnailMode
        );
        assert_eq!(
            GameType::from_str("Snail Mode").unwrap(),
            GameType::SnailMode
        );
        assert_eq!(
            GameType::from_str("snail_mode").unwrap(),
            GameType::SnailMode
        );
        assert_eq!(
            GameType::from_str("SNAIL_MODE").unwrap(),
            GameType::SnailMode
        );
        assert_eq!(GameType::from_str("solo").unwrap(), GameType::Solo);
        assert_eq!(GameType::from_str("Solo").unwrap(), GameType::Solo);
        assert_eq!(GameType::from_str("SOLO").unwrap(), GameType::Solo);
        assert_eq!(GameType::Solo.as_str(), "Solo");
    }

    #[test]
    fn game_type_from_str_unknown_returns_other() {
        assert_eq!(
            GameType::from_str("wrapped").unwrap(),
            GameType::Other("wrapped".to_string())
        );
        assert_eq!(
            GameType::from_str("").unwrap(),
            GameType::Other("".to_string())
        );
    }

    #[test]
    fn validate_battlesnake_count_solo_requires_exactly_one() {
        // Solo: 0, 2, and 4 are all invalid; only exactly 1 passes.
        assert!(validate_battlesnake_count(&GameType::Solo, 0).is_err());
        assert!(validate_battlesnake_count(&GameType::Solo, 1).is_ok());
        assert!(validate_battlesnake_count(&GameType::Solo, 2).is_err());
        assert!(validate_battlesnake_count(&GameType::Solo, 4).is_err());

        // Error message distinguishes empty (rule order: empty wins) from
        // wrong-count Solo.
        let err = validate_battlesnake_count(&GameType::Solo, 0).unwrap_err();
        assert!(err.to_string().contains("At least one battlesnake"));
        let err = validate_battlesnake_count(&GameType::Solo, 2).unwrap_err();
        assert!(err.to_string().contains("exactly one battlesnake"));
    }

    #[test]
    fn validate_battlesnake_count_standard_allows_one_to_four() {
        assert!(validate_battlesnake_count(&GameType::Standard, 0).is_err());
        assert!(validate_battlesnake_count(&GameType::Standard, 1).is_ok());
        assert!(validate_battlesnake_count(&GameType::Standard, 4).is_ok());
        assert!(validate_battlesnake_count(&GameType::Standard, 5).is_err());

        let err = validate_battlesnake_count(&GameType::Standard, 5).unwrap_err();
        assert!(err.to_string().contains("maximum of 4 battlesnakes"));
    }

    /// End-to-end DB round trip: a created Solo game reads back as
    /// `GameType::Solo` (not `Other("Solo")`), proving the hand-mapped
    /// text read path covers the new variant.
    #[sqlx::test(migrations = "../migrations")]
    async fn solo_game_round_trips_through_db(pool: sqlx::PgPool) -> cja::Result<()> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (7302, 'solo-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;

        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, 'Solo Snake', 'http://localhost:8000', 'public')
             RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;

        let created = create_game_with_snakes(
            &pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Solo,
                battlesnake_ids: vec![battlesnake_id],
            },
        )
        .await?;
        assert_eq!(created.game_type, GameType::Solo);

        let reread = get_game_by_id(&pool, created.game_id)
            .await?
            .expect("game should be persisted");
        assert_eq!(reread.game_type, GameType::Solo);

        Ok(())
    }

    #[test]
    fn board_size_from_str_custom() {
        assert_eq!(
            GameBoardSize::from_str("13x13").unwrap(),
            GameBoardSize::Custom("13x13".to_string())
        );
        assert_eq!(
            GameBoardSize::from_str("25x25").unwrap(),
            GameBoardSize::Custom("25x25".to_string())
        );
        assert_eq!(
            GameBoardSize::Custom("13x13".to_string()).dimensions(),
            (13, 13)
        );
        assert_eq!(GameBoardSize::Custom("13x13".to_string()).as_str(), "13x13");
    }

    async fn rematch_user(pool: &PgPool, id: i64) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token') RETURNING user_id",
        )
        .bind(id)
        .bind(format!("rematch-user-{id}"))
        .fetch_one(pool)
        .await?)
    }

    async fn rematch_snake(
        pool: &PgPool,
        user_id: Uuid,
        name: &str,
        visibility: &str,
    ) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'https://example.com', $3) RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .bind(visibility)
        .fetch_one(pool)
        .await?)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owned_creation_preserves_creator_and_duplicate_order(pool: PgPool) -> cja::Result<()> {
        let creator = rematch_user(&pool, 81281).await?;
        let a = rematch_snake(&pool, creator, "A", "private").await?;
        let b = rematch_snake(&pool, creator, "B", "public").await?;
        let lineup = vec![a, a, b];
        let game = create_game_with_snakes_for_user(
            &pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Large,
                game_type: GameType::Royale,
                battlesnake_ids: lineup.clone(),
            },
            creator,
        )
        .await?;
        let metadata = get_game_rematch_metadata(&pool, game.game_id)
            .await?
            .unwrap();
        assert_eq!(metadata.created_by_user_id, Some(creator));
        assert_eq!(metadata.rematch_battlesnake_ids, Some(lineup));
        let inserted: Vec<Uuid> = sqlx::query_scalar(
            "SELECT battlesnake_id FROM game_battlesnakes WHERE game_id = $1 ORDER BY created_at, game_battlesnake_id",
        )
        .bind(game.game_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(inserted.len(), 3);
        assert_eq!(inserted.iter().filter(|&&id| id == a).count(), 2);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owned_creation_rejects_inaccessible_without_partial_game(
        pool: PgPool,
    ) -> cja::Result<()> {
        let creator = rematch_user(&pool, 81282).await?;
        let other = rematch_user(&pool, 81283).await?;
        let private = rematch_snake(&pool, other, "Private", "private").await?;
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM games")
            .fetch_one(&pool)
            .await?;
        let error = create_game_with_snakes_for_user(
            &pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
                battlesnake_ids: vec![private],
            },
            creator,
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<InaccessibleBattlesnake>().is_some());
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM games")
            .fetch_one(&pool)
            .await?;
        assert_eq!(before, after);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn privacy_change_race_waits_then_rejects_without_partial_rows(
        pool: PgPool,
    ) -> cja::Result<()> {
        let creator = rematch_user(&pool, 81285).await?;
        let other = rematch_user(&pool, 81286).await?;
        let public = rematch_snake(&pool, other, "Racing public", "public").await?;

        let mut privacy_tx = pool.begin().await?;
        sqlx::query("UPDATE battlesnakes SET visibility = 'private' WHERE battlesnake_id = $1")
            .bind(public)
            .execute(&mut *privacy_tx)
            .await?;

        let create_pool = pool.clone();
        let create = tokio::spawn(async move {
            create_game_with_snakes_for_user(
                &create_pool,
                CreateGameWithSnakes {
                    board_size: GameBoardSize::Medium,
                    game_type: GameType::Standard,
                    battlesnake_ids: vec![public],
                },
                creator,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !create.is_finished(),
            "eligibility lock must wait for privacy update"
        );
        privacy_tx.commit().await?;
        let error = create.await.expect("creation task panicked").unwrap_err();
        assert!(error.downcast_ref::<InaccessibleBattlesnake>().is_some());
        let games: i64 = sqlx::query_scalar("SELECT count(*) FROM games")
            .fetch_one(&pool)
            .await?;
        let participants: i64 = sqlx::query_scalar("SELECT count(*) FROM game_battlesnakes")
            .fetch_one(&pool)
            .await?;
        assert_eq!((games, participants), (0, 0));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn system_creation_has_no_rematch_provenance(pool: PgPool) -> cja::Result<()> {
        let owner = rematch_user(&pool, 81284).await?;
        let snake = rematch_snake(&pool, owner, "System", "public").await?;
        let game = create_game_with_snakes(
            &pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Small,
                game_type: GameType::Standard,
                battlesnake_ids: vec![snake],
            },
        )
        .await?;
        let metadata = get_game_rematch_metadata(&pool, game.game_id)
            .await?
            .unwrap();
        assert_eq!(metadata.created_by_user_id, None);
        assert_eq!(metadata.rematch_battlesnake_ids, None);
        assert!(
            get_game_rematch_metadata(&pool, Uuid::new_v4())
                .await?
                .is_none()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn custom_busy_holds_ladder_until_deadline_but_custom_starts_immediately(
        pool: PgPool,
    ) -> cja::Result<()> {
        let owner = rematch_user(&pool, 161300).await?;
        let snake = rematch_snake(&pool, owner, "Dispatch", "public").await?;
        let new_game = || CreateGameWithSnakes {
            board_size: GameBoardSize::Medium,
            game_type: GameType::Standard,
            battlesnake_ids: vec![snake],
        };
        let custom = create_game_with_snakes(&pool, new_game()).await?;
        assert!(matches!(
            claim_game_start(&pool, custom.game_id, 480).await?,
            StartClaim::Started {
                leaderboard_id: None,
                ..
            }
        ));
        let ladder = create_game_with_snakes(&pool, new_game()).await?;
        let leaderboard_id = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard_id,
            ladder.game_id
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            claim_game_start(&pool, ladder.game_id, 480).await?,
            StartClaim::Busy
        );
        let other_custom = create_game_with_snakes(&pool, new_game()).await?;
        assert!(matches!(
            claim_game_start(&pool, other_custom.game_id, 480).await?,
            StartClaim::Started {
                leaderboard_id: None,
                ..
            }
        ));
        sqlx::query!("UPDATE games SET created_at = clock_timestamp() - interval '480 seconds' WHERE game_id = $1", ladder.game_id)
            .execute(&pool).await?;
        assert!(matches!(
            claim_game_start(&pool, ladder.game_id, 480).await?,
            StartClaim::Started {
                via: Some("deadline"),
                ..
            }
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn running_tournament_game_holds_ladder_claim(pool: PgPool) -> cja::Result<()> {
        let owner = rematch_user(&pool, 161305).await?;
        let snake = rematch_snake(&pool, owner, "Tournament", "public").await?;
        let tournament: Uuid = sqlx::query_scalar("INSERT INTO tournaments (name, user_id) VALUES ('claim test', $1) RETURNING tournament_id")
            .bind(owner).fetch_one(&pool).await?;
        let match_id: Uuid = sqlx::query_scalar("INSERT INTO tournament_matches (tournament_id, round, position, visual_column, visual_row) VALUES ($1, 1, 0, 0, 0) RETURNING match_id")
            .bind(tournament).fetch_one(&pool).await?;
        let new_game = || CreateGameWithSnakes {
            board_size: GameBoardSize::Medium,
            game_type: GameType::Standard,
            battlesnake_ids: vec![snake],
        };
        let tournament_game = create_game_with_snakes(&pool, new_game()).await?;
        sqlx::query("INSERT INTO match_games (match_id, game_id, game_number) VALUES ($1, $2, 1)")
            .bind(match_id)
            .bind(tournament_game.game_id)
            .execute(&pool)
            .await?;
        assert!(matches!(
            claim_game_start(&pool, tournament_game.game_id, 480).await?,
            StartClaim::Started {
                leaderboard_id: None,
                ..
            }
        ));
        let ladder_game = create_game_with_snakes(&pool, new_game()).await?;
        let ladder: Uuid = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
            .bind(ladder)
            .bind(ladder_game.game_id)
            .execute(&pool)
            .await?;
        assert_eq!(
            claim_game_start(&pool, ladder_game.game_id, 480).await?,
            StartClaim::Busy
        );
        Ok(())
    }
    #[sqlx::test(migrations = "../migrations")]
    async fn selected_entry_survives_pause_and_leaderboard_disable_holds_claim(
        pool: PgPool,
    ) -> cja::Result<()> {
        let owner = rematch_user(&pool, 161301).await?;
        let snake = rematch_snake(&pool, owner, "Selected", "public").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
        )
        .fetch_one(&pool)
        .await?;
        let entry = sqlx::query_scalar!(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
            VALUES ($1, $2) RETURNING leaderboard_entry_id",
            leaderboard_id,
            snake
        )
        .fetch_one(&pool)
        .await?;
        let game_id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type) VALUES ('11x11', 'Standard') RETURNING game_id")
            .fetch_one(&pool).await?;
        add_leaderboard_entry_to_game(&pool, game_id, entry).await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard_id,
            game_id
        )
        .execute(&pool)
        .await?;
        sqlx::query!("UPDATE leaderboard_entries SET disabled_at = clock_timestamp() WHERE leaderboard_entry_id = $1", entry)
            .execute(&pool).await?;
        sqlx::query!(
            "UPDATE leaderboards SET disabled_at = clock_timestamp() WHERE leaderboard_id = $1",
            leaderboard_id
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            claim_game_start(&pool, game_id, 480).await?,
            StartClaim::Busy
        );
        sqlx::query!(
            "UPDATE leaderboards SET disabled_at = NULL WHERE leaderboard_id = $1",
            leaderboard_id
        )
        .execute(&pool)
        .await?;
        assert!(matches!(
            claim_game_start(&pool, game_id, 480).await?,
            StartClaim::Started {
                via: Some("free"),
                ..
            }
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn empty_roster_cannot_start(pool: PgPool) -> cja::Result<()> {
        let game_id = sqlx::query_scalar!("INSERT INTO games (board_size, game_type) VALUES ('11x11', 'Standard') RETURNING game_id")
            .fetch_one(&pool).await?;
        assert!(claim_game_start(&pool, game_id, 480).await.is_err());
        assert_eq!(
            get_game_by_id(&pool, game_id).await?.unwrap().status,
            GameStatus::Waiting
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn duplicate_custom_snake_rows_claim_once(pool: PgPool) -> cja::Result<()> {
        let owner = rematch_user(&pool, 161303).await?;
        let snake = rematch_snake(&pool, owner, "Duplicate", "public").await?;
        let game = create_game_with_snakes(
            &pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
                battlesnake_ids: vec![snake, snake],
            },
        )
        .await?;
        assert!(matches!(
            claim_game_start(&pool, game.game_id, 480).await?,
            StartClaim::Started {
                leaderboard_id: None,
                ..
            }
        ));
        assert_eq!(
            claim_game_start(&pool, game.game_id, 480).await?,
            StartClaim::AlreadyRunning
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_same_ladder_claims_serialize_on_participant(
        pool: PgPool,
    ) -> cja::Result<()> {
        let owner = rematch_user(&pool, 161304).await?;
        let snake = rematch_snake(&pool, owner, "Contested", "public").await?;
        let second = rematch_snake(&pool, owner, "Second", "public").await?;
        let first_lock = snake.min(second);
        let ladder: Uuid = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        let mut games = Vec::new();
        for index in 0..2 {
            let game = create_game_with_snakes(
                &pool,
                CreateGameWithSnakes {
                    board_size: GameBoardSize::Medium,
                    game_type: GameType::Standard,
                    battlesnake_ids: if index == 0 {
                        vec![snake, second]
                    } else {
                        vec![second, snake]
                    },
                },
            )
            .await?;
            sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
                .bind(ladder)
                .bind(game.game_id)
                .execute(&pool)
                .await?;
            games.push(game.game_id);
        }
        // Hold the first participant lock so both claims queue before either
        // can inspect the running roster.
        let mut blocker = pool.begin().await?;
        sqlx::query(
            "SELECT battlesnake_id FROM battlesnakes WHERE battlesnake_id = $1 FOR NO KEY UPDATE",
        )
        .bind(first_lock)
        .fetch_one(&mut *blocker)
        .await?;
        let mut claims = Vec::new();
        for game_id in games {
            let pool = pool.clone();
            claims.push(tokio::spawn(async move {
                claim_game_start(&pool, game_id, 480).await
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let blocked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%FOR NO KEY UPDATE%'")
                .fetch_one(&pool).await?;
            if blocked >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "claims did not both reach participant lock"
            );
            tokio::task::yield_now().await;
        }
        // FK KEY SHARE must remain compatible with the held participant lock.
        let extra: Uuid = sqlx::query_scalar("INSERT INTO games (board_size, game_type) VALUES ('11x11', 'Standard') RETURNING game_id")
            .fetch_one(&pool).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
                .bind(extra)
                .bind(first_lock)
                .execute(&pool),
        )
        .await
        .expect("FK insert blocked by participant lock")?;
        blocker.rollback().await?;
        let mut started = 0;
        let mut busy = 0;
        for claim in claims {
            match tokio::time::timeout(std::time::Duration::from_secs(10), claim)
                .await
                .expect("claim deadlocked")
                .expect("claim panicked")?
            {
                StartClaim::Started { .. } => started += 1,
                StartClaim::Busy => busy += 1,
                other => panic!("unexpected claim result: {other:?}"),
            }
        }
        assert_eq!((started, busy), (1, 1));
        Ok(())
    }

    /// Older O {a,t} is held by a running same-ladder R {t}; younger Y {a}
    /// queues on a's row lock; R finishes; O queues on a. Exactly one of O/Y may
    /// start. Pins that the claim re-reads the schedule only after taking its
    /// participant locks: with the snapshot read first, both start and overlap
    /// on a in the same ladder.
    #[sqlx::test(migrations = "../migrations")]
    async fn claim_rechecks_schedule_after_participant_locks(pool: PgPool) -> cja::Result<()> {
        let owner = rematch_user(&pool, 999_001).await?;
        let a = rematch_snake(&pool, owner, "ClaimRaceA", "public").await?;
        let t = rematch_snake(&pool, owner, "ClaimRaceT", "public").await?;
        let ladder: Uuid = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        let mk = |ids: Vec<Uuid>| CreateGameWithSnakes {
            board_size: GameBoardSize::Medium,
            game_type: GameType::Standard,
            battlesnake_ids: ids,
        };
        let mut ids = Vec::new();
        for (snakes, age) in [(vec![t], 200), (vec![a, t], 100), (vec![a], 50)] {
            let g = create_game_with_snakes(&pool, mk(snakes)).await?;
            sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
                .bind(ladder)
                .bind(g.game_id)
                .execute(&pool)
                .await?;
            sqlx::query("UPDATE games SET created_at = clock_timestamp() - make_interval(secs => $2) WHERE game_id = $1")
                .bind(g.game_id)
                .bind(age as f64)
                .execute(&pool)
                .await?;
            ids.push(g.game_id);
        }
        let (r, o, y) = (ids[0], ids[1], ids[2]);
        sqlx::query("UPDATE games SET status = 'running' WHERE game_id = $1")
            .bind(r)
            .execute(&pool)
            .await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT 1 FROM battlesnakes WHERE battlesnake_id = $1 FOR NO KEY UPDATE")
            .bind(a)
            .fetch_one(&mut *blocker)
            .await?;
        async fn wait_blocked(pool: &PgPool, n: i64) -> cja::Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let blocked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'")
                    .fetch_one(pool)
                    .await?;
                if blocked >= n {
                    return Ok(());
                }
                assert!(std::time::Instant::now() < deadline, "claims never blocked");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        let p = pool.clone();
        let claim_y = tokio::spawn(async move { claim_game_start(&p, y, 480).await });
        wait_blocked(&pool, 1).await?;
        sqlx::query("UPDATE games SET status = 'finished' WHERE game_id = $1")
            .bind(r)
            .execute(&pool)
            .await?;
        let p = pool.clone();
        let claim_o = tokio::spawn(async move { claim_game_start(&p, o, 480).await });
        wait_blocked(&pool, 2).await?;
        blocker.rollback().await?;
        let ry = claim_y.await.expect("join")?;
        let ro = claim_o.await.expect("join")?;
        let started = [&ry, &ro]
            .iter()
            .filter(|c| matches!(c, StartClaim::Started { .. }))
            .count();
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM games WHERE game_id = ANY($1) AND status = 'running'",
        )
        .bind(vec![o, y])
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            (started, running),
            (1, 1),
            "same-ladder overlap on snake a: y={ry:?} o={ro:?}"
        );
        Ok(())
    }
}
