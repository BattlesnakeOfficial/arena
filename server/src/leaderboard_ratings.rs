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

// Re-export for backward compatibility
#[cfg(test)]
pub(crate) use crate::scoring::weng_lin::calculate_rating_updates;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::leaderboard::LeaderboardEntry;
    use uuid::Uuid;

    fn make_entry(mu: f64, sigma: f64) -> LeaderboardEntry {
        LeaderboardEntry {
            leaderboard_entry_id: Uuid::new_v4(),
            leaderboard_id: Uuid::new_v4(),
            battlesnake_id: Uuid::new_v4(),
            mu,
            sigma,
            display_score: mu - 3.0 * sigma,
            games_played: 5,
            first_place_finishes: 2,
            non_first_finishes: 3,
            disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_calculate_rating_updates_returns_correct_count() {
        let entries = vec![
            (make_entry(25.0, 8.333), 1),
            (make_entry(25.0, 8.333), 2),
            (make_entry(25.0, 8.333), 3),
            (make_entry(25.0, 8.333), 4),
        ];

        let updates = calculate_rating_updates(&entries);
        assert_eq!(updates.len(), 4);
    }

    #[test]
    fn test_winner_gains_rating() {
        let entries = vec![
            (make_entry(25.0, 8.333), 1), // winner
            (make_entry(25.0, 8.333), 2),
            (make_entry(25.0, 8.333), 3),
            (make_entry(25.0, 8.333), 4),
        ];

        let updates = calculate_rating_updates(&entries);

        // Winner (placement 1) should gain mu
        assert!(
            updates[0].new_mu > updates[0].old_mu,
            "Winner should gain mu: {} -> {}",
            updates[0].old_mu,
            updates[0].new_mu
        );
        assert!(updates[0].is_first_place);
        assert!(!updates[1].is_first_place);
    }

    #[test]
    fn test_last_place_loses_rating() {
        let entries = vec![
            (make_entry(25.0, 8.333), 1),
            (make_entry(25.0, 8.333), 2),
            (make_entry(25.0, 8.333), 3),
            (make_entry(25.0, 8.333), 4), // last place
        ];

        let updates = calculate_rating_updates(&entries);

        // Last place should lose mu
        assert!(
            updates[3].new_mu < updates[3].old_mu,
            "Last place should lose mu: {} -> {}",
            updates[3].old_mu,
            updates[3].new_mu
        );
    }

    #[test]
    fn test_sigma_decreases_after_game() {
        let entries = vec![
            (make_entry(25.0, 8.333), 1),
            (make_entry(25.0, 8.333), 2),
            (make_entry(25.0, 8.333), 3),
            (make_entry(25.0, 8.333), 4),
        ];

        let updates = calculate_rating_updates(&entries);

        // All snakes should have reduced uncertainty after a game
        for update in &updates {
            assert!(
                update.new_sigma < update.old_sigma,
                "Sigma should decrease: {} -> {}",
                update.old_sigma,
                update.new_sigma
            );
        }
    }

    #[test]
    fn test_display_score_calculation() {
        let entries = vec![(make_entry(25.0, 8.333), 1), (make_entry(25.0, 8.333), 2)];

        let updates = calculate_rating_updates(&entries);

        for update in &updates {
            let expected_display = update.new_mu - 3.0 * update.new_sigma;
            assert!(
                (update.new_display_score - expected_display).abs() < f64::EPSILON,
                "Display score should equal mu - 3*sigma"
            );
        }
    }

    #[test]
    fn test_higher_rated_snake_loses_less_when_winning() {
        // Strong snake beats weak snake — should gain less than if equal
        let strong = make_entry(35.0, 5.0);
        let weak = make_entry(15.0, 5.0);

        let expected_win = vec![(strong.clone(), 1), (weak.clone(), 2)];
        let updates = calculate_rating_updates(&expected_win);
        let strong_gain = updates[0].new_mu - updates[0].old_mu;

        // Upset: weak beats strong — weak should gain more
        let upset_win = vec![(weak, 1), (strong, 2)];
        let upset_updates = calculate_rating_updates(&upset_win);
        let weak_upset_gain = upset_updates[0].new_mu - upset_updates[0].old_mu;

        assert!(
            weak_upset_gain > strong_gain,
            "Upset winner should gain more ({:.4}) than expected winner ({:.4})",
            weak_upset_gain,
            strong_gain
        );
    }

    #[test]
    fn test_preserves_entry_ids() {
        let entries = vec![(make_entry(25.0, 8.333), 1), (make_entry(25.0, 8.333), 3)];

        let updates = calculate_rating_updates(&entries);

        assert_eq!(
            updates[0].leaderboard_entry_id,
            entries[0].0.leaderboard_entry_id
        );
        assert_eq!(
            updates[1].leaderboard_entry_id,
            entries[1].0.leaderboard_entry_id
        );
        assert_eq!(updates[0].placement, 1);
        assert_eq!(updates[1].placement, 3);
    }
}
