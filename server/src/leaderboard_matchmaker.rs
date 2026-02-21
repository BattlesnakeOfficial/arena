use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::{
    jobs::GameRunnerJob,
    models::{
        game::{self, CreateGameWithSnakes, GameBoardSize, GameType},
        leaderboard::{self, GAMES_PER_DAY, LeaderboardEntry, MATCH_SIZE},
    },
    state::AppState,
};

/// Number of cron runs per day (every 15 minutes = 96 runs/day)
const RUNS_PER_DAY: i32 = 96;

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
    let games_per_run = (GAMES_PER_DAY / RUNS_PER_DAY).max(1);

    tracing::info!(
        leaderboard_id = %leaderboard_id,
        active_snakes = entries.len(),
        games_to_create = games_per_run,
        "Running matchmaker"
    );

    for _ in 0..games_per_run {
        let selected = select_match(&entries, MATCH_SIZE);
        if selected.len() < MATCH_SIZE {
            break;
        }

        let battlesnake_ids: Vec<Uuid> = selected.iter().map(|e| e.battlesnake_id).collect();

        // Create the game
        let game = game::create_game_with_snakes(
            pool,
            CreateGameWithSnakes {
                board_size: GameBoardSize::Medium, // 11x11
                game_type: GameType::Standard,
                battlesnake_ids,
            },
        )
        .await
        .wrap_err("Failed to create leaderboard game")?;

        // Set enqueued timestamp
        game::set_game_enqueued_at(pool, game.game_id, chrono::Utc::now())
            .await
            .wrap_err("Failed to set enqueued_at")?;

        // Link game to leaderboard
        leaderboard::create_leaderboard_game(pool, leaderboard_id, game.game_id)
            .await
            .wrap_err("Failed to create leaderboard game record")?;

        // Enqueue the game runner job
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
fn select_match(entries: &[LeaderboardEntry], match_size: usize) -> Vec<LeaderboardEntry> {
    if entries.len() < match_size {
        return vec![];
    }

    let mut rng = rand::thread_rng();

    // Sort by display_score
    let mut sorted: Vec<&LeaderboardEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.display_score
            .partial_cmp(&a.display_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Pick a random seed snake
    let seed_idx = rand::Rng::gen_range(&mut rng, 0..sorted.len());
    let seed_score = sorted[seed_idx].display_score;

    // Score each snake by distance to seed, with jitter for variety
    let mut candidates: Vec<(usize, f64)> = sorted
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let distance = (entry.display_score - seed_score).abs();
            let jitter: f64 = rand::Rng::gen_range(&mut rng, 0.0..5.0);
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
            wins: 0,
            losses: 0,
            disabled_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_select_match_returns_correct_size() {
        let entries: Vec<LeaderboardEntry> = (0..10).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&entries, 4);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn test_select_match_too_few_entries() {
        let entries: Vec<LeaderboardEntry> = (0..3).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&entries, 4);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_select_match_exactly_enough() {
        let entries: Vec<LeaderboardEntry> = (0..4).map(|i| make_entry(i as f64 * 5.0)).collect();
        let selected = select_match(&entries, 4);
        assert_eq!(selected.len(), 4);
    }

    #[test]
    fn test_select_match_unique_snakes() {
        let entries: Vec<LeaderboardEntry> = (0..20).map(|i| make_entry(i as f64 * 2.0)).collect();
        let selected = select_match(&entries, 4);
        let ids: Vec<Uuid> = selected.iter().map(|e| e.battlesnake_id).collect();
        let unique: std::collections::HashSet<Uuid> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "Selected snakes should be unique");
    }
}
