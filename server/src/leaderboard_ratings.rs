use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::{
    models::{
        game_battlesnake,
        leaderboard::{self, LeaderboardGame},
    },
    scoring::{GameResultEntry, GameResultEvent},
    state::AppState,
};

/// Update ratings for all snakes in a completed leaderboard game.
/// Idempotent: safe to call multiple times (e.g. job retries).
/// Uses a database transaction with row locking (FOR UPDATE) to prevent
/// race conditions when concurrent games finish for the same snakes.
pub async fn update_ratings(app_state: &AppState, leaderboard_game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    // Idempotency check: bail if ratings were already applied for this game
    let existing: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM leaderboard_game_results WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check existing game results")?;

    if existing.0 > 0 {
        tracing::info!(
            leaderboard_game_id = %leaderboard_game_id,
            "Ratings already applied for this game, skipping"
        );
        return Ok(());
    }

    // Fetch the leaderboard game (outside transaction — immutable data)
    let lb_game = sqlx::query_as::<_, LeaderboardGame>(
        "SELECT leaderboard_game_id, leaderboard_id, game_id, created_at
         FROM leaderboard_games
         WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch leaderboard game")?;

    // Fetch all game_battlesnakes with their placements (outside transaction — immutable after game finishes)
    let game_snakes = game_battlesnake::get_battlesnakes_by_game_id(pool, lb_game.game_id).await?;

    if game_snakes.is_empty() {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "No snakes found for leaderboard game"
        );
        return Ok(());
    }

    // Start a transaction for the rating update (locks entries to prevent concurrent overwrites)
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start transaction for rating update")?;

    // Authoritative idempotency check INSIDE the transaction.
    // The early check above is a fast-path optimization; this is the real guard
    // against concurrent job execution (e.g., timeout-triggered retry while original still runs).
    let existing_in_tx: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM leaderboard_game_results WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to check existing game results inside transaction")?;

    if existing_in_tx.0 > 0 {
        tracing::info!(
            leaderboard_game_id = %leaderboard_game_id,
            "Ratings already applied (detected inside transaction), skipping"
        );
        return Ok(());
    }

    // Look up each snake's leaderboard entry with FOR UPDATE to lock the rows
    let mut entries_with_placements: Vec<(leaderboard::LeaderboardEntry, i32)> = Vec::new();

    for gs in &game_snakes {
        let placement = gs.placement.unwrap_or(game_snakes.len() as i32);

        // Use leaderboard_entry_id if stored (deterministic lookup by PK).
        // Fall back to battlesnake_id lookup for games created before this column was added.
        let entry = if let Some(entry_id) = gs.leaderboard_entry_id {
            leaderboard::get_entry_for_update_by_id(&mut *tx, entry_id)
                .await
                .wrap_err_with(|| {
                    format!("Failed to get leaderboard entry {entry_id} for update")
                })?
        } else {
            leaderboard::get_entry_for_update(&mut *tx, lb_game.leaderboard_id, gs.battlesnake_id)
                .await
                .wrap_err_with(|| {
                    format!(
                        "Failed to get leaderboard entry for snake {}",
                        gs.battlesnake_id
                    )
                })?
        };

        if let Some(entry) = entry {
            entries_with_placements.push((entry, placement));
        } else {
            tracing::warn!(
                battlesnake_id = %gs.battlesnake_id,
                leaderboard_id = %lb_game.leaderboard_id,
                "Snake has no leaderboard entry, skipping"
            );
        }
    }

    if entries_with_placements.len() < 2 {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "Fewer than 2 snakes with leaderboard entries, skipping rating update"
        );
        return Ok(());
    }

    // Build a GameResultEvent for the scoring algorithms
    let event = GameResultEvent {
        leaderboard_game_id,
        leaderboard_id: lb_game.leaderboard_id,
        game_id: lb_game.game_id,
        results: entries_with_placements
            .iter()
            .map(|(entry, placement)| GameResultEntry {
                leaderboard_entry_id: entry.leaderboard_entry_id,
                battlesnake_id: entry.battlesnake_id,
                placement: *placement,
            })
            .collect(),
    };

    // Run all scoring algorithms
    for algo in app_state.scoring.algorithms() {
        algo.process_game_result(&mut tx, &event).await?;
    }

    // Commit the transaction — all rating updates are atomic
    tx.commit()
        .await
        .wrap_err("Failed to commit rating update transaction")?;

    tracing::info!(
        leaderboard_game_id = %leaderboard_game_id,
        game_id = %lb_game.game_id,
        snakes_updated = entries_with_placements.len(),
        "Ratings updated for leaderboard game"
    );

    Ok(())
}
