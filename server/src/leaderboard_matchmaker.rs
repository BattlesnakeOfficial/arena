use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::{
    cron::MATCHMAKER_INTERVAL_SECS,
    jobs::GameRunnerJob,
    models::{
        game::{self, CreateGame, GameBoardSize, GameType},
        leaderboard::{self, GAMES_PER_DAY, LeaderboardEntry, MATCH_SIZE},
    },
    state::AppState,
};

/// Run the matchmaker for all active leaderboards
pub async fn run_matchmaker(app_state: &AppState) -> cja::Result<()> {
    let pool = &app_state.db;

    let leaderboards = leaderboard::get_active_leaderboards(pool)
        .await
        .wrap_err("Failed to fetch active leaderboards")?;

    for lb in &leaderboards {
        if let Err(e) = run_matchmaker_for_leaderboard(app_state, lb.leaderboard_id).await {
            tracing::error!(
                leaderboard_id = %lb.leaderboard_id,
                leaderboard_name = %lb.name,
                error = ?e,
                "Failed to run matchmaker for leaderboard"
            );
        }
    }

    Ok(())
}

async fn run_matchmaker_for_leaderboard(
    app_state: &AppState,
    leaderboard_id: Uuid,
) -> cja::Result<()> {
    let pool = &app_state.db;

    let entries = leaderboard::get_active_entries(pool, leaderboard_id)
        .await
        .wrap_err("Failed to fetch active entries")?;

    if entries.len() < MATCH_SIZE {
        tracing::debug!(
            leaderboard_id = %leaderboard_id,
            active_snakes = entries.len(),
            "Not enough active snakes for matchmaking (need {})",
            MATCH_SIZE
        );
        return Ok(());
    }

    // Calculate how many games to create this run
    // Derived from shared cron interval constant to avoid manual sync bugs
    let runs_per_day = (24 * 60 * 60 / MATCHMAKER_INTERVAL_SECS) as i32;
    let games_per_run = ((GAMES_PER_DAY + runs_per_day - 1) / runs_per_day).max(1);

    tracing::info!(
        leaderboard_id = %leaderboard_id,
        active_snakes = entries.len(),
        games_to_create = games_per_run,
        "Running matchmaker"
    );

    for _ in 0..games_per_run {
        let selected = select_match(&mut rand::thread_rng(), &entries, MATCH_SIZE);
        if selected.len() < MATCH_SIZE {
            break;
        }

        // Use a transaction to atomically create the game, link it to the leaderboard,
        // and set enqueued_at. This prevents "zombie" games without a leaderboard record.
        let mut tx = pool
            .begin()
            .await
            .wrap_err("Failed to start matchmaker transaction")?;

        let game = game::create_game(
            &mut *tx,
            CreateGame {
                board_size: GameBoardSize::Medium, // 11x11
                game_type: GameType::Standard,
            },
        )
        .await
        .wrap_err("Failed to create game")?;

        // Add each selected entry by leaderboard_entry_id only — no redundant battlesnake_id copy.
        // The effective battlesnake is resolved via JOIN in get_battlesnakes_by_game_id when needed.
        for entry in &selected {
            game::add_leaderboard_entry_to_game(&mut *tx, game.game_id, entry.leaderboard_entry_id)
                .await
                .wrap_err_with(|| {
                    format!(
                        "Failed to add entry {} to game {}",
                        entry.leaderboard_entry_id, game.game_id
                    )
                })?;
        }

        game::set_game_enqueued_at_tx(&mut tx, game.game_id, chrono::Utc::now())
            .await
            .wrap_err("Failed to set enqueued_at")?;

        leaderboard::create_leaderboard_game(&mut *tx, leaderboard_id, game.game_id)
            .await
            .wrap_err("Failed to create leaderboard game record")?;

        tx.commit()
            .await
            .wrap_err("Failed to commit matchmaker transaction")?;

        // Enqueue outside the transaction — if this fails, the game + leaderboard record
        // still exist (consistent state). The game can be retried or discovered by a poller.
        let job = GameRunnerJob {
            game_id: game.game_id,
        };
        cja::jobs::Job::enqueue(
            job,
            app_state.clone(),
            format!("Leaderboard game {}", game.game_id),
        )
        .await
        .wrap_err("Failed to enqueue game runner job")?;

        tracing::info!(
            leaderboard_id = %leaderboard_id,
            game_id = %game.game_id,
            "Created leaderboard match game"
        );
    }

    Ok(())
}

/// Select snakes for a match using skill-band matching with jitter.
/// Picks a random seed snake, then selects nearest neighbors by score.
/// Accepts an RNG parameter for test determinism.
///
/// TODO: Add recently-matched deprioritization to prevent the same group of snakes
/// from being matched repeatedly in low-volume periods.
fn select_match(
    rng: &mut impl rand::Rng,
    entries: &[LeaderboardEntry],
    match_size: usize,
) -> Vec<LeaderboardEntry> {
    if entries.len() < match_size {
        return vec![];
    }

    // Sort by display_score
    let mut sorted: Vec<&LeaderboardEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.display_score
            .partial_cmp(&a.display_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Pick a random seed snake
    let seed_idx = rng.gen_range(0..sorted.len());
    let seed_score = sorted[seed_idx].display_score;

    // Score each snake by distance to seed, with jitter for variety
    let mut candidates: Vec<(usize, f64)> = sorted
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let distance = (entry.display_score - seed_score).abs();
            let jitter: f64 = rng.gen_range(0.0..5.0);
            (i, distance + jitter)
        })
        .collect();

    // Sort by jittered distance (closest first)
    candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take the first match_size snakes
    candidates
        .into_iter()
        .take(match_size)
        .map(|(i, _)| sorted[i].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use uuid::Uuid;

    fn make_entry(display_score: f64) -> LeaderboardEntry {
        LeaderboardEntry {
            leaderboard_entry_id: Uuid::new_v4(),
            leaderboard_id: Uuid::new_v4(),
            battlesnake_id: Uuid::new_v4(),
            mu: 25.0,
            sigma: 8.333,
            display_score,
            games_played: 0,
            first_place_finishes: 0,
            non_first_finishes: 0,
            disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn seeded_rng() -> rand::rngs::StdRng {
        rand::rngs::StdRng::seed_from_u64(42)
    }

    #[test]
    fn test_select_match_returns_correct_size() {
        let entries: Vec<LeaderboardEntry> = (0..10).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&mut seeded_rng(), &entries, 4);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn test_select_match_too_few_entries() {
        let entries: Vec<LeaderboardEntry> = (0..3).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&mut seeded_rng(), &entries, 4);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_select_match_exactly_enough() {
        let entries: Vec<LeaderboardEntry> = (0..4).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&mut seeded_rng(), &entries, 4);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn test_select_match_unique_snakes() {
        let entries: Vec<LeaderboardEntry> = (0..20).map(|i| make_entry(i as f64 * 2.0)).collect();
        let selected = select_match(&mut seeded_rng(), &entries, 4);
        let ids: Vec<Uuid> = selected.iter().map(|e| e.battlesnake_id).collect();
        let unique: std::collections::HashSet<Uuid> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "Selected snakes should be unique");
    }
}
