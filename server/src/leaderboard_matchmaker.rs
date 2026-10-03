use color_eyre::eyre::Context as _;
use std::str::FromStr;

use crate::{
    jobs::GameRunnerJob,
    models::{
        game::{self, CreateGame, GameBoardSize, GameType},
        leaderboard::{self, Leaderboard, LeaderboardEntry, MIN_MATCH_SIZE},
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
        if let Err(e) = run_matchmaker_for_leaderboard(app_state, lb).await {
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

async fn run_matchmaker_for_leaderboard(app_state: &AppState, lb: &Leaderboard) -> cja::Result<()> {
    let pool = &app_state.db;
    let leaderboard_id = lb.leaderboard_id;
    let now = chrono::Utc::now();
    let backlog = sqlx::query!(
        r#"SELECT COUNT(*) AS "backlog_count!",
                  MIN(g.enqueued_at) AS oldest_enqueued_at
           FROM games g
           JOIN leaderboard_games lg ON lg.game_id = g.game_id
           WHERE g.status = 'waiting' AND g.enqueued_at IS NOT NULL
             AND lg.leaderboard_id = $1"#,
        leaderboard_id,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check matchmaker backlog")?;
    if backlog.backlog_count > 0 {
        let oldest_waiting_age_secs = backlog
            .oldest_enqueued_at
            .map(|at| (now - at).num_seconds().max(0))
            .unwrap_or(0);
        tracing::warn!(
            leaderboard_id = %leaderboard_id,
            leaderboard_name = %lb.name,
            backlog_count = backlog.backlog_count,
            oldest_waiting_age_secs,
            "Skipping matchmaker round: waiting games remain"
        );
        return Ok(());
    }

    let entries = leaderboard::get_active_entries(pool, leaderboard_id)
        .await
        .wrap_err("Failed to fetch active entries")?;

    // Play short-handed (down to MIN_MATCH_SIZE) rather than freezing the
    // ladder when snakes drop out — a health-disabled snake once starved
    // matchmaking for 10 days because this was a silent 4-or-nothing check.
    if entries.len() < MIN_MATCH_SIZE {
        if entries.is_empty() {
            tracing::debug!(leaderboard_id = %leaderboard_id, "No snakes entered in leaderboard");
        } else {
            tracing::warn!(
                leaderboard_id = %leaderboard_id,
                active_snakes = entries.len(),
                "Matchmaking starved: not enough active snakes (need at least {})",
                MIN_MATCH_SIZE
            );
        }
        return Ok(());
    }
    let match_size = entries.len().min(lb.match_size as usize);
    let game_type = GameType::from_str(&lb.game_type)
        .wrap_err_with(|| format!("Invalid game type for leaderboard {}", lb.name))?;
    let board_size = GameBoardSize::from_str(&lb.board_size)
        .wrap_err_with(|| format!("Invalid board size for leaderboard {}", lb.name))?;

    let round = select_round(&mut rand::thread_rng(), &entries, lb.match_size as usize);

    tracing::info!(
        leaderboard_id = %leaderboard_id,
        active_snakes = entries.len(),
        match_size,
        configured_match_size = lb.match_size,
        games_to_create = round.len(),
        "Running matchmaker"
    );

    for selected in round {
        // Use a transaction to atomically create the game, link it to the leaderboard,
        // and set enqueued_at. This prevents "zombie" games without a leaderboard record.
        let mut tx = pool
            .begin()
            .await
            .wrap_err("Failed to start matchmaker transaction")?;

        let game = game::create_game(
            &mut *tx,
            CreateGame {
                board_size: board_size.clone(),
                game_type: game_type.clone(),
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
            None,
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

// Bounded rank jitter keeps groups local while varying opponents between ticks.
const RANK_JITTER: f64 = 4.0;
// Duels need a wider band to expose edge-ranked snakes to enough opponents.
const DUEL_RANK_JITTER: f64 = 5.0;

/// Form one rank-banded round, with a rotating full-size game for remainders.
///
/// TODO: Add recently-matched deprioritization to prevent the same group of snakes
/// from being matched repeatedly in low-volume periods.
fn select_round(
    rng: &mut impl rand::Rng,
    entries: &[LeaderboardEntry],
    match_size: usize,
) -> Vec<Vec<LeaderboardEntry>> {
    if entries.len() < MIN_MATCH_SIZE {
        return vec![];
    }
    if entries.len() < match_size {
        return vec![entries.to_vec()];
    }

    // Original rank is stable for ties and determines the skill gap for fillers.
    let mut sorted: Vec<&LeaderboardEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.display_score
            .total_cmp(&a.display_score)
            .then_with(|| a.leaderboard_entry_id.cmp(&b.leaderboard_entry_id))
    });
    let remainder = sorted.len() % match_size;
    let reserved_start = if remainder > 0 {
        rng.gen_range(0..=sorted.len() - remainder)
    } else {
        0
    };
    let reserved: Vec<usize> = (reserved_start..reserved_start + remainder).collect();
    let rank_jitter = if match_size == 2 {
        DUEL_RANK_JITTER
    } else {
        RANK_JITTER
    };
    let mut assigned: Vec<(usize, f64)> = (0..sorted.len())
        .filter(|rank| !reserved.contains(rank))
        .map(|rank| (rank, rank as f64 + rng.gen_range(-rank_jitter..rank_jitter)))
        .collect();
    assigned.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut groups: Vec<Vec<LeaderboardEntry>> = assigned
        .chunks(match_size)
        .map(|chunk| {
            chunk
                .iter()
                .map(|(rank, _)| (*sorted[*rank]).clone())
                .collect()
        })
        .collect();

    if remainder > 0 {
        const FILLER_NOISE: f64 = 6.0;
        let mut fillers: Vec<(usize, f64)> = assigned
            .iter()
            .map(|(rank, _)| {
                // remainder > 0 guarantees at least one reserved rank here.
                let distance = reserved
                    .iter()
                    .map(|r| rank.abs_diff(*r))
                    .min()
                    .unwrap_or(usize::MAX);
                (*rank, distance as f64 + rng.gen_range(0.0..FILLER_NOISE))
            })
            .collect();
        fillers.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let mut final_group: Vec<LeaderboardEntry> = reserved
            .iter()
            .map(|rank| (*sorted[*rank]).clone())
            .collect();
        final_group.extend(
            fillers
                .iter()
                .take(match_size - remainder)
                .map(|(rank, _)| (*sorted[*rank]).clone()),
        );
        groups.push(final_group);
    }
    groups
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

    fn assert_round(entries: &[LeaderboardEntry], size: usize, round: &[Vec<LeaderboardEntry>]) {
        use std::collections::HashSet;
        let expected = if entries.len() < MIN_MATCH_SIZE {
            0
        } else {
            entries.len().div_ceil(size)
        };
        assert_eq!(round.len(), expected);
        let mut covered = HashSet::new();
        for game in round {
            assert_eq!(game.len(), size.min(entries.len()));
            let ids: HashSet<_> = game.iter().map(|e| e.leaderboard_entry_id).collect();
            assert_eq!(ids.len(), game.len(), "duplicate snake within a game");
            covered.extend(ids);
        }
        let expected_ids: HashSet<_> = if expected == 0 {
            HashSet::new()
        } else {
            entries.iter().map(|e| e.leaderboard_entry_id).collect()
        };
        assert_eq!(covered, expected_ids);
    }

    #[test]
    fn round_shapes_and_coverage() {
        for size in [2, 4] {
            for n in 0..=21 {
                let entries: Vec<_> = (0..n).map(|i| make_entry(i as f64 * 5.0)).collect();
                let round = select_round(&mut seeded_rng(), &entries, size);
                assert_round(&entries, size, &round);
            }
        }
    }

    #[test]
    fn seeded_round_fairness() {
        use std::collections::{HashMap, HashSet};
        for (n, size) in [(21, 4), (17, 2), (8, 4)] {
            let entries: Vec<_> = (0..n).map(|i| make_entry((n - i) as f64)).collect();
            let ranks: HashMap<_, _> = entries
                .iter()
                .enumerate()
                .map(|(rank, e)| (e.leaderboard_entry_id, rank))
                .collect();
            for seed in [0, 1, 2, 3, 42] {
                let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
                let mut appearances = vec![0; n];
                let mut opponents: Vec<HashSet<Uuid>> = vec![HashSet::new(); n];
                let mut gap_sum = 0_usize;
                let mut pair_count = 0_usize;
                for _ in 0..100 {
                    let round = select_round(&mut rng, &entries, size);
                    assert_round(&entries, size, &round);
                    for game in &round {
                        for (i, left) in game.iter().enumerate() {
                            let left_rank = ranks[&left.leaderboard_entry_id];
                            appearances[left_rank] += 1;
                            for right in game.iter().skip(i + 1) {
                                let right_rank = ranks[&right.leaderboard_entry_id];
                                gap_sum += left_rank.abs_diff(right_rank);
                                pair_count += 1;
                                opponents[left_rank].insert(right.leaderboard_entry_id);
                                opponents[right_rank].insert(left.leaderboard_entry_id);
                            }
                        }
                    }
                }
                let min_games = *appearances.iter().min().unwrap();
                let max_games = *appearances.iter().max().unwrap();
                let mean_gap = gap_sum as f64 / pair_count as f64;
                let min_opponents = opponents.iter().map(HashSet::len).min().unwrap();
                eprintln!(
                    "N={n} size={size} seed={seed}: games={min_games}..{max_games}, mean_rank_gap={mean_gap:.3}, min_opponents={min_opponents}"
                );
                assert!(min_games >= 100 && max_games <= 130);
                assert!(mean_gap <= 4.0);
                assert!(min_opponents >= 6);
            }
        }
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

    /// A pool short of the configured match size still gets games — sized to the pool.
    #[sqlx::test(migrations = "../migrations")]
    async fn matchmaker_creates_short_handed_games(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let leaderboard_id = leaderboard_with_snakes(&pool, 3).await?;

        let lb = leaderboard::get_leaderboard_by_id(&pool, leaderboard_id)
            .await?
            .expect("test leaderboard exists");
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;

        let sizes = game_sizes(&pool, leaderboard_id).await?;
        assert_eq!(sizes, vec![3], "one short-handed game uses the whole pool");
        Ok(())
    }

    /// Below MIN_MATCH_SIZE the matchmaker pauses instead of erroring.
    #[sqlx::test(migrations = "../migrations")]
    async fn matchmaker_pauses_below_min_match_size(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let leaderboard_id = leaderboard_with_snakes(&pool, 1).await?;

        let lb = leaderboard::get_leaderboard_by_id(&pool, leaderboard_id)
            .await?
            .expect("test leaderboard exists");
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;

        assert!(game_sizes(&pool, leaderboard_id).await?.is_empty());
        Ok(())
    }

    async fn seeded_mode(
        pool: &sqlx::PgPool,
        name: &str,
        snake_count: usize,
    ) -> cja::Result<(Leaderboard, std::collections::HashSet<Uuid>)> {
        let lb = sqlx::query_as!(
            Leaderboard,
            r#"SELECT leaderboard_id, name, game_type, board_size, match_size,
                      disabled_at, created_at, updated_at
               FROM leaderboards WHERE name = $1"#,
            name,
        )
        .fetch_one(pool)
        .await?;
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (88010, 'mode-owner', 'test-token') RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let mut ids = std::collections::HashSet::new();
        for i in 0..snake_count {
            let battlesnake_id = sqlx::query_scalar!(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, 'http://example.com/snake') RETURNING battlesnake_id",
                user_id,
                format!("mode-snake-{i}"),
            )
            .fetch_one(pool)
            .await?;
            let entry =
                leaderboard::get_or_create_entry(pool, lb.leaderboard_id, battlesnake_id).await?;
            ids.insert(entry.leaderboard_entry_id);
        }
        Ok((lb, ids))
    }

    async fn assert_created_round(
        pool: &sqlx::PgPool,
        lb: &Leaderboard,
        expected_ids: &std::collections::HashSet<Uuid>,
        expected_games: usize,
        expected_size: usize,
    ) -> cja::Result<()> {
        use std::collections::{HashMap, HashSet};
        let rows = sqlx::query!(
            r#"SELECT g.game_id, g.game_type, g.board_size, g.enqueued_at,
                      gb.leaderboard_entry_id
               FROM leaderboard_games lg
               JOIN games g ON g.game_id = lg.game_id
               JOIN game_battlesnakes gb ON gb.game_id = g.game_id
               WHERE lg.leaderboard_id = $1"#,
            lb.leaderboard_id,
        )
        .fetch_all(pool)
        .await?;
        assert!(!rows.is_empty(), "a round must produce participant rows");
        let mut games: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        let mut covered = HashSet::new();
        for row in rows {
            assert_eq!(row.game_type, lb.game_type);
            assert_eq!(row.board_size, lb.board_size);
            assert!(row.enqueued_at.is_some());
            let id = row.leaderboard_entry_id.expect("ranked game participant");
            games.entry(row.game_id).or_default().push(id);
            covered.insert(id);
        }
        assert_eq!(games.len(), expected_games);
        for ids in games.values() {
            assert_eq!(ids.len(), expected_size);
            assert_eq!(ids.iter().copied().collect::<HashSet<_>>().len(), ids.len());
        }
        assert_eq!(&covered, expected_ids);
        Ok(())
    }

    async fn mode_round(
        pool: sqlx::PgPool,
        name: &str,
        game_type: &str,
        match_size: i32,
        snake_count: usize,
        expected_games: usize,
    ) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, name, snake_count).await?;
        assert_eq!(lb.match_size, match_size);
        assert_eq!(lb.game_type, game_type);
        assert_eq!(lb.board_size, "11x11");
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, expected_games, match_size as usize).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn standard_round(pool: sqlx::PgPool) -> cja::Result<()> {
        mode_round(pool, "Standard 11x11", "Standard", 4, 21, 6).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn duels_round(pool: sqlx::PgPool) -> cja::Result<()> {
        mode_round(pool, "Duels 11x11", "Standard", 2, 17, 9).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn royale_round(pool: sqlx::PgPool) -> cja::Result<()> {
        mode_round(pool, "Royale 11x11", "Royale", 4, 12, 3).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn constrictor_round(pool: sqlx::PgPool) -> cja::Result<()> {
        mode_round(pool, "Constrictor 11x11", "Constrictor", 4, 8, 2).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_entries_do_not_join_round(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, mut ids) = seeded_mode(&pool, "Standard 11x11", 6).await?;
        let disabled = *ids.iter().next().unwrap();
        sqlx::query!(
            "UPDATE leaderboard_entries SET disabled_at = NOW(), disabled_reason = 'test' WHERE leaderboard_entry_id = $1",
            disabled,
        )
        .execute(&pool)
        .await?;
        ids.remove(&disabled);
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 2, 4).await
    }

    async fn insert_backlog_game(
        pool: &sqlx::PgPool,
        leaderboard_id: Uuid,
        enqueued: bool,
    ) -> cja::Result<Uuid> {
        let game_id = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, enqueued_at)
             VALUES ('11x11', 'Standard', CASE WHEN $1 THEN NOW() ELSE NULL END)
             RETURNING game_id",
            enqueued,
        )
        .fetch_one(pool)
        .await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard_id,
            game_id,
        )
        .execute(pool)
        .await?;
        Ok(game_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn waiting_backlog_skips_then_running_allows_round(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let game_id = insert_backlog_game(&pool, lb.leaderboard_id, true).await?;
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_eq!(game_sizes(&pool, lb.leaderboard_id).await?.len(), 0);
        sqlx::query!(
            "UPDATE games SET status = 'running' WHERE game_id = $1",
            game_id
        )
        .execute(&pool)
        .await?;
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 1, 4).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn old_waiting_game_blocks_round(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, _) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let game_id = insert_backlog_game(&pool, lb.leaderboard_id, true).await?;
        let old_enqueued_at = chrono::Utc::now() - chrono::Duration::hours(3);
        sqlx::query!(
            "UPDATE games SET enqueued_at = $1 WHERE game_id = $2",
            old_enqueued_at,
            game_id,
        )
        .execute(&pool)
        .await?;

        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_eq!(game_sizes(&pool, lb.leaderboard_id).await?.len(), 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn other_ladder_backlog_does_not_block(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let other = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Duels 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        insert_backlog_game(&pool, other, true).await?;
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 1, 4).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unenqueued_waiting_game_does_not_block(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        insert_backlog_game(&pool, lb.leaderboard_id, false).await?;
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 1, 4).await
    }
}
