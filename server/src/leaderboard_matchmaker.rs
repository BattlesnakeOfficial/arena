use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::{
    cron::MATCHMAKER_INTERVAL_SECS,
    jobs::GameRunnerJob,
    models::{
        game::{self, CreateGame, GameBoardSize, GameType},
        leaderboard::{self, GAMES_PER_DAY, LeaderboardEntry, MATCH_SIZE, MIN_MATCH_SIZE},
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

    // Play short-handed (down to MIN_MATCH_SIZE) rather than freezing the
    // ladder when snakes drop out — a health-disabled snake once starved
    // matchmaking for 10 days because this was a silent 4-or-nothing check.
    if entries.len() < MIN_MATCH_SIZE {
        tracing::warn!(
            leaderboard_id = %leaderboard_id,
            active_snakes = entries.len(),
            "Matchmaking starved: not enough active snakes (need at least {})",
            MIN_MATCH_SIZE
        );
        return Ok(());
    }
    let match_size = entries.len().min(MATCH_SIZE);

    // Calculate how many games to create this run
    // Derived from shared cron interval constant to avoid manual sync bugs
    let runs_per_day = (24 * 60 * 60 / MATCHMAKER_INTERVAL_SECS) as i32;
    let games_per_run = ((GAMES_PER_DAY + runs_per_day - 1) / runs_per_day).max(1);

    tracing::info!(
        leaderboard_id = %leaderboard_id,
        active_snakes = entries.len(),
        match_size,
        games_to_create = games_per_run,
        "Running matchmaker"
    );

    for _ in 0..games_per_run {
        let selected = select_match(&mut rand::thread_rng(), &entries, match_size);
        if selected.len() < match_size {
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
            disabled_reason: None,
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

    /// The matchmaker passes `min(pool, MATCH_SIZE)` — a 3-snake pool plays
    /// 3-snake games instead of freezing the ladder.
    #[test]
    fn test_select_match_short_handed() {
        for pool in MIN_MATCH_SIZE..MATCH_SIZE {
            let entries: Vec<LeaderboardEntry> =
                (0..pool).map(|i| make_entry(i as f64 * 5.0)).collect();
            let selected = select_match(&mut seeded_rng(), &entries, pool.min(MATCH_SIZE));
            assert_eq!(selected.len(), pool);
            let unique: std::collections::HashSet<Uuid> =
                selected.iter().map(|e| e.battlesnake_id).collect();
            assert_eq!(unique.len(), pool, "short-handed picks must be unique");
        }
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

    async fn leaderboard_with_snakes(pool: &sqlx::PgPool, snake_count: usize) -> cja::Result<Uuid> {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (88001, 'mm-owner', 'test-token')
             RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('mm-test') RETURNING leaderboard_id",
        )
        .fetch_one(pool)
        .await?;
        for i in 0..snake_count {
            let battlesnake_id = sqlx::query_scalar!(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, 'http://example.com/snake')
                 RETURNING battlesnake_id",
                user_id,
                format!("mm-snake-{i}"),
            )
            .fetch_one(pool)
            .await?;
            leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id).await?;
        }
        Ok(leaderboard_id)
    }

    async fn game_sizes(pool: &sqlx::PgPool, leaderboard_id: Uuid) -> cja::Result<Vec<i64>> {
        // Matchmaker rows carry leaderboard_entry_id only (battlesnake_id
        // stays NULL and is resolved via JOIN), so count rows, not that column.
        let sizes = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "size!"
             FROM leaderboard_games lg
             JOIN game_battlesnakes gb ON gb.game_id = lg.game_id
             WHERE lg.leaderboard_id = $1
             GROUP BY lg.game_id"#,
            leaderboard_id,
        )
        .fetch_all(pool)
        .await?;
        Ok(sizes)
    }

    /// A pool short of MATCH_SIZE still gets games — sized to the pool.
    #[sqlx::test(migrations = "../migrations")]
    async fn matchmaker_creates_short_handed_games(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let leaderboard_id = leaderboard_with_snakes(&pool, 3).await?;

        run_matchmaker_for_leaderboard(&app_state, leaderboard_id).await?;

        let sizes = game_sizes(&pool, leaderboard_id).await?;
        assert!(!sizes.is_empty(), "3 enabled snakes must produce games");
        assert!(sizes.iter().all(|&s| s == 3), "games use the whole pool");
        Ok(())
    }

    /// Below MIN_MATCH_SIZE the matchmaker pauses instead of erroring.
    #[sqlx::test(migrations = "../migrations")]
    async fn matchmaker_pauses_below_min_match_size(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let leaderboard_id = leaderboard_with_snakes(&pool, 1).await?;

        run_matchmaker_for_leaderboard(&app_state, leaderboard_id).await?;

        assert!(game_sizes(&pool, leaderboard_id).await?.is_empty());
        Ok(())
    }
}
