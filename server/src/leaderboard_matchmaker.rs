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
    let backlog = sqlx::query!(
        r#"SELECT COUNT(*) AS "backlog_count!", MIN(oldest_job_at) AS oldest_job_at
           FROM (SELECT MIN(j.created_at) AS oldest_job_at
                 FROM games g
                 JOIN leaderboard_games lg ON lg.game_id = g.game_id
                 JOIN jobs j ON j.name = 'GameRunnerJob'
                   AND j.payload->>'game_id' = g.game_id::text
                 WHERE lg.leaderboard_id = $1 AND g.status = 'waiting'
                   AND j.locked_at IS NULL AND j.locked_by IS NULL
                   AND j.run_at <= clock_timestamp()
                   AND j.created_at < clock_timestamp() - interval '30 seconds'
                 GROUP BY g.game_id) pending"#,
        leaderboard_id,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check unclaimed runner backlog")?;
    if backlog.backlog_count > 0 {
        tracing::warn!(leaderboard_id = %leaderboard_id, leaderboard_name = %lb.name,
            backlog_count = backlog.backlog_count,
            oldest_unclaimed_job_age_secs = backlog.oldest_job_at.map(|at| (chrono::Utc::now() - at).num_seconds().max(0)).unwrap_or(0),
            "Skipping matchmaker round: old unclaimed runner jobs remain");
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

        tracing::info!(
            leaderboard_id = %leaderboard_id,
            game_id = %game.game_id,
            "Created leaderboard match game"
        );
    }

    Ok(())
}

/// Discover and enqueue eligible waiting ladder games. The runner repeats the
/// eligibility check under participant locks before the game can start.
pub async fn dispatch_pending_ladder_games(app_state: &AppState) -> cja::Result<()> {
    let mut conn = app_state
        .db
        .acquire()
        .await
        .wrap_err("Failed to acquire schedule connection")?;
    let snapshot = game::load_schedule_snapshot(&mut conn)
        .await
        .wrap_err("Failed to discover pending ladder games")?;
    drop(conn);
    for waiting in snapshot
        .games
        .iter()
        .filter(|g| g.status == "waiting" && g.leaderboard_id.is_some() && g.disabled_at.is_none())
    {
        let held_age = snapshot
            .at
            .signed_duration_since(waiting.created_at)
            .num_seconds();
        if held_age < app_state.config.ladder_start_deadline_secs {
            continue;
        }
        let blocking: Vec<_> = snapshot
            .games
            .iter()
            .filter(|running| {
                running.status == "running"
                    && running.leaderboard_id == waiting.leaderboard_id
                    && running
                        .participants
                        .iter()
                        .any(|id| waiting.participants.contains(id))
            })
            .map(|g| g.game_id)
            .collect();
        if !blocking.is_empty() {
            tracing::warn!(game_id = %waiting.game_id, leaderboard_id = ?waiting.leaderboard_id,
                blocking_game_ids = ?blocking, held_age_secs = held_age,
                "Past-deadline ladder game held by same-ladder running game");
        }
    }
    let mut candidates: Vec<_> = snapshot
        .games
        .iter()
        .filter(|g| {
            game::ladder_eligibility(&snapshot, g, app_state.config.ladder_start_deadline_secs)
                .is_some()
        })
        .collect();
    candidates.sort_by_key(|g| (g.created_at, g.game_id));
    let mut first_error = None;
    for candidate in candidates {
        let result: cja::Result<()> = async {
            let mut tx = app_state.db.begin().await.wrap_err("Failed to begin ladder dispatch")?;
            let row = sqlx::query!(
                r#"SELECT g.status, lb.disabled_at,
                          COALESCE(lg.last_dispatch_at > clock_timestamp() - interval '5 seconds', false) AS "throttled!"
                   FROM leaderboard_games lg
                   JOIN games g ON g.game_id = lg.game_id
                   JOIN leaderboards lb ON lb.leaderboard_id = lg.leaderboard_id
                   WHERE lg.game_id = $1 FOR UPDATE OF lg"#,
                candidate.game_id,
            )
            .fetch_optional(&mut *tx)
            .await
            .wrap_err("Failed to lock ladder dispatch row")?;
            let Some(row) = row else { return Ok(()); };
            if row.status != "waiting" || row.disabled_at.is_some() || row.throttled {
                return Ok(());
            }
            // This must be a separate READ COMMITTED statement after the row lock.
            // A pass that waited for the lock needs a fresh snapshot of jobs
            // committed by the previous holder, even if that holder rolled back.
            let has_job = sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM jobs WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1) AS "has_job!""#,
                candidate.game_id.to_string(),
            )
            .fetch_one(&mut *tx)
            .await
            .wrap_err("Failed to check outstanding ladder runner after dispatch lock")?;
            if has_job {
                return Ok(());
            }
            cja::jobs::Job::enqueue(GameRunnerJob { game_id: candidate.game_id }, app_state.clone(),
                format!("Leaderboard game {}", candidate.game_id), None)
                .await.wrap_err("Failed to enqueue ladder game runner")?;
            sqlx::query!("UPDATE leaderboard_games SET last_dispatch_at = clock_timestamp() WHERE game_id = $1", candidate.game_id)
                .execute(&mut *tx).await.wrap_err("Failed to record ladder dispatch")?;
            tx.commit().await.wrap_err("Failed to commit ladder dispatch")?;
            tracing::info!(event_type = "ladder_game_eligible", game_id = %candidate.game_id,
                leaderboard_id = ?candidate.leaderboard_id,
                eligible_observed_at = %snapshot.at,
                dispatched_at = %chrono::Utc::now(), "ladder game eligible for dispatch");
            Ok(())
        }.await;
        if let Err(error) = result {
            tracing::error!(game_id = %candidate.game_id, error = %format!("{error:#}"), "Ladder dispatch failed");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
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
    use crate::cron::MATCHMAKER_INTERVAL_SECS;
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

    async fn insert_old_unclaimed_job(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            "INSERT INTO jobs (job_id, name, payload, priority, context, created_at)
             VALUES (gen_random_uuid(), 'GameRunnerJob', jsonb_build_object('game_id', $1::text), 0, 'test', clock_timestamp() - interval '31 seconds')",
            game_id.to_string(),
        ).execute(pool).await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn waiting_backlog_skips_then_running_allows_round(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let game_id = insert_backlog_game(&pool, lb.leaderboard_id, true).await?;
        insert_old_unclaimed_job(&pool, game_id).await?;
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
    async fn old_waiting_game_does_not_block_round(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let game_id = insert_backlog_game(&pool, lb.leaderboard_id, true).await?;
        let old_enqueued_at = chrono::Utc::now()
            - chrono::Duration::seconds((2 * MATCHMAKER_INTERVAL_SECS + 1) as i64);
        sqlx::query!(
            "UPDATE games SET enqueued_at = $1 WHERE game_id = $2",
            old_enqueued_at,
            game_id,
        )
        .execute(&pool)
        .await?;

        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 1, 4).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn other_ladder_backlog_does_not_block(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let other = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Duels 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        let other_game = insert_backlog_game(&pool, other, true).await?;
        insert_old_unclaimed_job(&pool, other_game).await?;
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app_state, &lb).await?;
        assert_created_round(&pool, &lb, &ids, 1, 4).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claimed_old_job_does_not_block_new_round(pool: sqlx::PgPool) -> cja::Result<()> {
        let (lb, ids) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let game_id = insert_backlog_game(&pool, lb.leaderboard_id, true).await?;
        insert_old_unclaimed_job(&pool, game_id).await?;
        sqlx::query!(
            "UPDATE jobs SET locked_at = clock_timestamp(), locked_by = 'test-worker'
            WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1",
            game_id.to_string()
        )
        .execute(&pool)
        .await?;
        let app = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app, &lb).await?;
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

    #[sqlx::test(migrations = "../migrations")]
    async fn held_rounds_dispatch_once_and_same_ladder_never_overlap(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, _) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let limited_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect_with((*pool.connect_options()).clone())
            .await?;
        let app = crate::state::AppState::test_from_pool(limited_pool);
        run_matchmaker_for_leaderboard(&app, &lb).await?;
        run_matchmaker_for_leaderboard(&app, &lb).await?;
        let games = sqlx::query_scalar!(
            "SELECT g.game_id FROM games g JOIN leaderboard_games lg ON lg.game_id = g.game_id
             WHERE lg.leaderboard_id = $1 ORDER BY g.created_at, g.game_id",
            lb.leaderboard_id
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(games.len(), 2, "a held round must not skip the next round");
        let (left, right) = tokio::join!(
            dispatch_pending_ladder_games(&app),
            dispatch_pending_ladder_games(&app)
        );
        left?;
        right?;
        let queued = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob'")
            .fetch_one(&pool)
            .await?
            .unwrap_or(0);
        assert_eq!(queued, 1, "overlapping dispatches must create one job");
        let queued_game: String =
            sqlx::query_scalar("SELECT payload->>'game_id' FROM jobs WHERE name = 'GameRunnerJob'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            queued_game,
            games[0].to_string(),
            "oldest shared-snake game must dispatch first"
        );
        let (left, right) = tokio::join!(
            game::claim_game_start(&pool, games[0], 480),
            game::claim_game_start(&pool, games[0], 480),
        );
        let claims = [left?, right?];
        assert_eq!(
            claims
                .iter()
                .filter(|c| matches!(c, game::StartClaim::Started { .. }))
                .count(),
            1
        );
        sqlx::query!("UPDATE games SET created_at = clock_timestamp() - interval '600 seconds' WHERE game_id = $1", games[1])
            .execute(&pool).await?;
        assert_eq!(
            game::claim_game_start(&pool, games[1], 480).await?,
            game::StartClaim::Busy
        );
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            games[0]
        )
        .execute(&pool)
        .await?;
        assert!(matches!(
            game::claim_game_start(&pool, games[1], 480).await?,
            game::StartClaim::Started { .. }
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_busy_job_is_redispatched_after_throttle(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, _) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let app = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app, &lb).await?;
        dispatch_pending_ladder_games(&app).await?;
        let game_id = sqlx::query_scalar!(
            "SELECT game_id FROM leaderboard_games WHERE leaderboard_id = $1",
            lb.leaderboard_id
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query!(
            "DELETE FROM jobs WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1",
            game_id.to_string()
        )
        .execute(&pool)
        .await?;
        dispatch_pending_ladder_games(&app).await?;
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob'")
                .fetch_one(&pool)
                .await?
                .unwrap_or(0),
            0
        );
        sqlx::query!("UPDATE leaderboard_games SET last_dispatch_at = clock_timestamp() - interval '6 seconds' WHERE game_id = $1", game_id)
            .execute(&pool).await?;
        dispatch_pending_ladder_games(&app).await?;
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob'")
                .fetch_one(&pool)
                .await?
                .unwrap_or(0),
            1
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_leaderboard_waits_then_dispatches_after_enable(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, _) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let app = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app, &lb).await?;
        sqlx::query!(
            "UPDATE leaderboards SET disabled_at = clock_timestamp() WHERE leaderboard_id = $1",
            lb.leaderboard_id
        )
        .execute(&pool)
        .await?;
        dispatch_pending_ladder_games(&app).await?;
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob'")
                .fetch_one(&pool)
                .await?
                .unwrap_or(0),
            0
        );
        sqlx::query!(
            "UPDATE leaderboards SET disabled_at = NULL WHERE leaderboard_id = $1",
            lb.leaderboard_id
        )
        .execute(&pool)
        .await?;
        dispatch_pending_ladder_games(&app).await?;
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob'")
                .fetch_one(&pool)
                .await?
                .unwrap_or(0),
            1
        );
        Ok(())
    }

    /// Plan DEV-1613.4 step 6: "A crash after enqueue but before commit leaves
    /// the durable job; a concurrent pass waits on the row and sees it." A pass
    /// blocked on the `leaderboard_games` lock must not enqueue a second runner
    /// once the holder's job is committed and the holder rolls back.
    #[sqlx::test(migrations = "../migrations")]
    async fn blocked_dispatch_sees_job_enqueued_by_rolled_back_holder(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (lb, _) = seeded_mode(&pool, "Standard 11x11", 4).await?;
        let app = crate::state::AppState::test_from_pool(pool.clone());
        run_matchmaker_for_leaderboard(&app, &lb).await?;
        let game_id: Uuid =
            sqlx::query_scalar("SELECT game_id FROM leaderboard_games WHERE leaderboard_id = $1")
                .bind(lb.leaderboard_id)
                .fetch_one(&pool)
                .await?;

        // First dispatcher: holds the row lock, enqueues (committed on another
        // connection), then dies before recording last_dispatch_at.
        let mut holder = pool.begin().await?;
        sqlx::query("SELECT game_id FROM leaderboard_games WHERE game_id = $1 FOR UPDATE")
            .bind(game_id)
            .fetch_one(&mut *holder)
            .await?;

        let concurrent_app = app.clone();
        let concurrent =
            tokio::spawn(async move { dispatch_pending_ladder_games(&concurrent_app).await });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND query LIKE '%FOR UPDATE OF lg%'",
            )
            .fetch_one(&pool)
            .await?;
            if blocked > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "concurrent dispatch never blocked on the leaderboard_games lock"
            );
            tokio::task::yield_now().await;
        }

        cja::jobs::Job::enqueue(
            GameRunnerJob { game_id },
            app.clone(),
            "first dispatcher".to_string(),
            None,
        )
        .await?;
        holder.rollback().await?;

        tokio::time::timeout(std::time::Duration::from_secs(10), concurrent)
            .await
            .expect("concurrent dispatch timed out")
            .expect("concurrent dispatch panicked")?;

        let jobs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs
             WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1",
        )
        .bind(game_id.to_string())
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            jobs, 1,
            "a blocked dispatch pass enqueued a duplicate GameRunnerJob; the duplicate \
             reaches run_game's Running branch and resets a live game"
        );
        Ok(())
    }

    #[test]
    fn ladder_dispatch_sim() {
        use crate::models::game::{ScheduleGame, ScheduleSnapshot, ladder_eligibility};
        use rand::Rng;
        use std::collections::{HashMap, HashSet};
        // Seed 1613. Ratings: 1000 - 10 * seeded rotated rank + 0.01 * ladder index.
        // Assumptions: instant worker pickup on each 5s tick; no non-ladder games.
        let mut rng = rand::rngs::StdRng::seed_from_u64(1613);
        let snakes: Vec<Uuid> = (1..=24).map(Uuid::from_u128).collect();
        let membership: [Vec<usize>; 4] = [
            (0..22).collect(),
            (0..16).chain(std::iter::once(22)).collect(),
            (0..11).chain(std::iter::once(16)).collect(),
            (0..8).chain(std::iter::once(23)).collect(),
        ];
        let sizes = [4, 2, 4, 4];
        let lengths: [(f64, f64, i64); 4] = [
            (162.0, 343.0, 619),
            (91.0, 315.0, 467),
            (122.0, 182.0, 226),
            (27.0, 37.0, 52),
        ];
        let ladders: Vec<Uuid> = (100..104).map(Uuid::from_u128).collect();
        let rating_rotations: Vec<usize> = membership
            .iter()
            .map(|members| rng.gen_range(0..members.len()))
            .collect();
        let entries: Vec<Vec<LeaderboardEntry>> = membership
            .iter()
            .enumerate()
            .map(|(mode, members)| {
                members
                    .iter()
                    .enumerate()
                    .map(|(rank, &snake)| {
                        let mut entry = make_entry(
                            1000.0
                                - ((rank + rating_rotations[mode]) % members.len()) as f64 * 10.0
                                + mode as f64 * 0.01,
                        );
                        entry.battlesnake_id = snakes[snake];
                        entry.leaderboard_id = ladders[mode];
                        entry.leaderboard_entry_id =
                            Uuid::from_u128(1000 + mode as u128 * 100 + snake as u128);
                        entry
                    })
                    .collect()
            })
            .collect();
        #[derive(Clone)]
        struct Sim {
            schedule: ScheduleGame,
            mode: usize,
            start: Option<i64>,
            end: Option<i64>,
        }
        let origin = chrono::Utc::now();
        let mut games: Vec<Sim> = Vec::new();
        let mut next_id = 10_000_u128;
        let mut next_round = 0_i64;
        let mut deadline_starts = 0;
        let mut waits = Vec::new();
        for tick in (0..86_400).step_by(5) {
            for game in &mut games {
                if game.schedule.status == "running" && game.end.is_some_and(|end| end <= tick) {
                    game.schedule.status = "finished".into();
                }
            }
            if tick >= next_round {
                next_round += 864;
                for mode in 0..4 {
                    let round = select_round(&mut rng, &entries[mode], sizes[mode]);
                    assert_eq!(round.len(), entries[mode].len().div_ceil(sizes[mode]));
                    let covered: HashSet<_> = round
                        .iter()
                        .flatten()
                        .map(|e| e.leaderboard_entry_id)
                        .collect();
                    assert_eq!(covered.len(), entries[mode].len(), "round skipped an entry");
                    for group in round {
                        games.push(Sim {
                            schedule: ScheduleGame {
                                game_id: Uuid::from_u128(next_id),
                                status: "waiting".into(),
                                created_at: origin + chrono::Duration::seconds(tick),
                                leaderboard_id: Some(ladders[mode]),
                                disabled_at: None,
                                participants: group.iter().map(|e| e.battlesnake_id).collect(),
                            },
                            mode,
                            start: None,
                            end: None,
                        });
                        next_id += 1;
                    }
                }
            }
            loop {
                let snapshot = ScheduleSnapshot {
                    at: origin + chrono::Duration::seconds(tick),
                    games: games
                        .iter()
                        .filter(|g| g.schedule.status != "finished")
                        .map(|g| g.schedule.clone())
                        .collect(),
                };
                let winner = games
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.schedule.status == "waiting")
                    .filter(|(_, g)| ladder_eligibility(&snapshot, &g.schedule, 480).is_some())
                    .min_by_key(|(_, g)| (g.schedule.created_at, g.schedule.game_id))
                    .map(|(i, _)| i);
                let Some(index) = winner else { break };
                let via = ladder_eligibility(&snapshot, &games[index].schedule, 480).unwrap();
                if via == "deadline" {
                    deadline_starts += 1;
                }
                let (p50, p90, max) = lengths[games[index].mode];
                let u1 = rng.gen_range(f64::EPSILON..1.0);
                let u2 = rng.gen_range(0.0..1.0);
                let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
                let duration = (p50.ln() + (p90 / p50).ln() / 1.2816 * z).exp().round() as i64;
                let duration = duration.clamp(1, max);
                let created = games[index]
                    .schedule
                    .created_at
                    .signed_duration_since(origin)
                    .num_seconds();
                waits.push((tick - created) * 1000);
                games[index].schedule.status = "running".into();
                games[index].start = Some(tick);
                games[index].end = Some(tick + duration);
            }
        }
        let completed: Vec<_> = games
            .iter()
            .filter(|g| g.end.is_some_and(|end| end <= 86_400))
            .collect();
        let mut counts: HashMap<(Uuid, Uuid), usize> = HashMap::new();
        let mut buckets = [(0usize, 0usize); 4];
        let mut same_ladder_overlap = 0;
        let mut max_four_live = 0;
        for game in &completed {
            let (Some(start), Some(end)) = (game.start, game.end) else {
                continue;
            };
            for &snake in &game.schedule.participants {
                *counts
                    .entry((game.schedule.leaderboard_id.unwrap(), snake))
                    .or_default() += 1;
                let ladder_count = membership
                    .iter()
                    .filter(|members| members.iter().any(|&n| snakes[n] == snake))
                    .count();
                // Include games still running at the day boundary as overlap
                // partners for completed games, even though they do not count
                // toward completed volume.
                let overlapping: Vec<_> = games
                    .iter()
                    .filter(|other| {
                        other.schedule.game_id != game.schedule.game_id
                            && other.schedule.participants.contains(&snake)
                            && other.start.is_some_and(|other_start| other_start < end)
                            && other.end.is_some_and(|other_end| start < other_end)
                    })
                    .collect();
                same_ladder_overlap += overlapping
                    .iter()
                    .filter(|other| other.schedule.leaderboard_id == game.schedule.leaderboard_id)
                    .count();
                buckets[ladder_count - 1].0 += usize::from(overlapping.is_empty());
                buckets[ladder_count - 1].1 += 1;
                if ladder_count == 4 {
                    max_four_live = max_four_live.max(overlapping.len() + 1);
                }
            }
        }
        let share = |good: usize, total: usize| {
            if total == 0 {
                0.0
            } else {
                100.0 * good as f64 / total as f64
            }
        };
        let one = share(buckets[0].0, buckets[0].1);
        let two_three = share(buckets[1].0 + buckets[2].0, buckets[1].1 + buckets[2].1);
        let all = share(
            buckets.iter().map(|x| x.0).sum(),
            buckets.iter().map(|x| x.1).sum(),
        );
        let four = share(buckets[3].0, buckets[3].1);
        let min_entry = entries
            .iter()
            .flatten()
            .map(|e| {
                counts
                    .get(&(e.leaderboard_id, e.battlesnake_id))
                    .copied()
                    .unwrap_or(0)
            })
            .min()
            .unwrap();
        waits.sort_unstable();
        let percentile = |p: f64| waits[((waits.len() - 1) as f64 * p).round() as usize];
        let mut starts: Vec<_> = games.iter().filter_map(|g| g.start).collect();
        starts.sort_unstable();
        let max_burst = starts
            .iter()
            .map(|&s| {
                starts
                    .iter()
                    .filter(|&&other| other >= s && other < s + 10)
                    .count()
            })
            .max()
            .unwrap_or(0);
        println!(
            "metric | result | gate\n---|---:|---\nsame-ladder overlap | {same_ladder_overlap} | zero\none-ladder no-overlap | {one:.1}% | {}\ntwo/three-ladder no-overlap | {two_three:.1}% | {}\noverall no-overlap | {all:.1}% | {}\nfour-ladder no-overlap | {four:.1}% | report\nfour-ladder max live | {max_four_live} | report\ncompleted/day | {} | {}\nminimum completed/entry | {min_entry} | {}\ndeadline starts/day | {deadline_starts} | report\nmax starts/rolling 10s | {max_burst} | report\nwait p50/p95/max ms | {}/{}/{} | report",
            if one >= 95.0 { "PASS" } else { "MISS" },
            if two_three >= 75.0 { "PASS" } else { "MISS" },
            if all >= 50.0 { "PASS" } else { "MISS" },
            completed.len(),
            if completed.len() >= 2000 {
                "PASS"
            } else {
                "MISS"
            },
            if min_entry >= 95 { "PASS" } else { "MISS" },
            percentile(0.5),
            percentile(0.95),
            waits[waits.len() - 1]
        );
        assert_eq!(same_ladder_overlap, 0);
    }
}
