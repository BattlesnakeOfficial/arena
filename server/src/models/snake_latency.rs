//! Recent `/move` latency for a battlesnake, summarized per game for the
//! snake profile chart.
//!
//! Reads `snake_turns.latency_ms` / `timed_out`, which the game runner writes
//! for every move request. A timeout and a network error both store
//! `latency_ms = NULL, timed_out = true`, so percentiles only cover answered
//! moves and timeouts are counted separately — folding them in as NULLs
//! would silently hide a snake's worst turns.

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// How many of a snake's most recent finished games the profile charts.
pub const RECENT_GAMES_LIMIT: i64 = 50;

/// Latency percentiles over a set of move requests.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LatencyStats {
    /// Moves the snake answered before the timeout.
    pub answered: i64,
    /// Moves that timed out or failed at the network level.
    pub timeouts: i64,
    /// Median latency of answered moves; `None` when nothing was answered.
    pub p50_ms: Option<f64>,
    /// 95th percentile latency of answered moves; `None` when nothing was
    /// answered.
    pub p95_ms: Option<f64>,
}

/// Latency for one game the snake played.
#[derive(Debug, Clone, PartialEq)]
pub struct GameLatency {
    pub game_id: Uuid,
    pub game_created_at: DateTime<Utc>,
    pub stats: LatencyStats,
}

/// Latency across a snake's recent games.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecentLatency {
    /// One entry per game with latency data, oldest first.
    pub games: Vec<GameLatency>,
    /// Percentiles over every move in `games` (not an average of per-game
    /// percentiles).
    pub overall: LatencyStats,
}

/// Fetch per-game latency for a snake's `limit` most recent finished games.
///
/// Snakes join games two ways: directly via `game_battlesnakes.battlesnake_id`
/// (custom games) or via `leaderboard_entry_id` with a NULL `battlesnake_id`
/// (leaderboard games), so both are matched. Imported legacy engine games
/// (`engine_game_id IS NOT NULL`) never have turn rows and are skipped so they
/// can't crowd real games out of the window. Moves recorded before latency
/// tracking existed (NULL latency, not timed out) are ignored.
pub async fn get_recent_latency_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    limit: i64,
) -> cja::Result<RecentLatency> {
    // GROUPING SETS returns one row per game plus a single grand-total row
    // (game_id NULL) whose percentiles span every move in the window.
    let rows = sqlx::query!(
        r#"
        WITH recent AS (
            SELECT gb.game_battlesnake_id, g.game_id, g.created_at
            FROM game_battlesnakes gb
            JOIN games g ON g.game_id = gb.game_id
            WHERE (
                gb.battlesnake_id = $1
                OR gb.leaderboard_entry_id IN (
                    SELECT le.leaderboard_entry_id
                    FROM leaderboard_entries le
                    WHERE le.battlesnake_id = $1
                )
            )
              AND g.status = 'finished'
              AND g.engine_game_id IS NULL
            ORDER BY g.created_at DESC
            LIMIT $2
        )
        SELECT
            r.game_id AS "game_id?",
            MAX(r.created_at) AS "game_created_at?",
            COUNT(st.latency_ms) AS "answered!",
            COUNT(*) FILTER (WHERE st.timed_out) AS "timeouts!",
            percentile_cont(0.5) WITHIN GROUP (ORDER BY st.latency_ms::float8) AS p50_ms,
            percentile_cont(0.95) WITHIN GROUP (ORDER BY st.latency_ms::float8) AS p95_ms
        FROM recent r
        JOIN snake_turns st ON st.game_battlesnake_id = r.game_battlesnake_id
        WHERE st.latency_ms IS NOT NULL OR st.timed_out
        GROUP BY GROUPING SETS ((r.game_id), ())
        ORDER BY MAX(r.created_at) ASC
        "#,
        battlesnake_id,
        limit
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch recent latency for battlesnake")?;

    let mut recent = RecentLatency::default();
    for row in rows {
        let stats = LatencyStats {
            answered: row.answered,
            timeouts: row.timeouts,
            p50_ms: row.p50_ms,
            p95_ms: row.p95_ms,
        };
        match (row.game_id, row.game_created_at) {
            (Some(game_id), Some(game_created_at)) => recent.games.push(GameLatency {
                game_id,
                game_created_at,
                stats,
            }),
            _ => recent.overall = stats,
        }
    }

    Ok(recent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `percentile_cont` interpolates in floating point, so compare with a
    /// tolerance rather than `==`.
    fn assert_stats(
        actual: LatencyStats,
        answered: i64,
        timeouts: i64,
        p50_ms: Option<f64>,
        p95_ms: Option<f64>,
    ) {
        assert_eq!((actual.answered, actual.timeouts), (answered, timeouts));
        for (name, got, want) in [
            ("p50", actual.p50_ms, p50_ms),
            ("p95", actual.p95_ms, p95_ms),
        ] {
            match (got, want) {
                (Some(got), Some(want)) => {
                    assert!((got - want).abs() < 1e-6, "{name}: got {got}, want {want}");
                }
                (got, want) => assert_eq!(got, want, "{name}"),
            }
        }
    }

    struct Seed {
        pool: PgPool,
        battlesnake_id: Uuid,
    }

    impl Seed {
        async fn new(pool: PgPool) -> cja::Result<Self> {
            let user_id = sqlx::query_scalar!(
                "INSERT INTO users (external_github_id, github_login, github_access_token)
                 VALUES (4242, 'latency-owner', 'test-token')
                 RETURNING user_id"
            )
            .fetch_one(&pool)
            .await?;
            let battlesnake_id = Self::snake(&pool, user_id, "Latency Snake").await?;
            Ok(Self {
                pool,
                battlesnake_id,
            })
        }

        async fn snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
            Ok(sqlx::query_scalar!(
                "INSERT INTO battlesnakes (user_id, name, url, visibility)
                 VALUES ($1, $2, 'http://localhost:8000', 'public')
                 RETURNING battlesnake_id",
                user_id,
                name
            )
            .fetch_one(pool)
            .await?)
        }

        async fn game(&self, status: &str, minutes_ago: i32) -> cja::Result<Uuid> {
            Ok(sqlx::query_scalar!(
                "INSERT INTO games (board_size, game_type, status, created_at)
                 VALUES ('11x11', 'Standard', $1, NOW() - make_interval(mins => $2))
                 RETURNING game_id",
                status,
                minutes_ago
            )
            .fetch_one(&self.pool)
            .await?)
        }

        async fn join_direct(&self, game_id: Uuid, battlesnake_id: Uuid) -> cja::Result<Uuid> {
            Ok(sqlx::query_scalar!(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id)
                 VALUES ($1, $2)
                 RETURNING game_battlesnake_id",
                game_id,
                battlesnake_id
            )
            .fetch_one(&self.pool)
            .await?)
        }

        async fn join_via_leaderboard(&self, game_id: Uuid) -> cja::Result<Uuid> {
            let entry_id = sqlx::query_scalar!(
                "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
                 SELECT leaderboard_id, $1 FROM leaderboards ORDER BY created_at LIMIT 1
                 RETURNING leaderboard_entry_id",
                self.battlesnake_id
            )
            .fetch_one(&self.pool)
            .await?;
            Ok(sqlx::query_scalar!(
                "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id)
                 VALUES ($1, $2)
                 RETURNING game_battlesnake_id",
                game_id,
                entry_id
            )
            .fetch_one(&self.pool)
            .await?)
        }

        /// Record one move per entry: `Some(ms)` answered, `None` timed out.
        async fn moves(
            &self,
            game_id: Uuid,
            game_battlesnake_id: Uuid,
            latencies: &[Option<i32>],
        ) -> cja::Result<()> {
            for latency_ms in latencies {
                self.raw_move(
                    game_id,
                    game_battlesnake_id,
                    *latency_ms,
                    latency_ms.is_none(),
                )
                .await?;
            }
            Ok(())
        }

        async fn raw_move(
            &self,
            game_id: Uuid,
            game_battlesnake_id: Uuid,
            latency_ms: Option<i32>,
            timed_out: bool,
        ) -> cja::Result<()> {
            let turn_id = sqlx::query_scalar!(
                "INSERT INTO turns (game_id, turn_number)
                 VALUES ($1, (SELECT COUNT(*)::int FROM turns WHERE game_id = $1))
                 RETURNING turn_id",
                game_id
            )
            .fetch_one(&self.pool)
            .await?;
            sqlx::query!(
                "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction, latency_ms, timed_out)
                 VALUES ($1, $2, 'up', $3, $4)",
                turn_id,
                game_battlesnake_id,
                latency_ms,
                timed_out
            )
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        async fn fetch(&self, limit: i64) -> cja::Result<RecentLatency> {
            get_recent_latency_for_battlesnake(&self.pool, self.battlesnake_id, limit).await
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn summarizes_each_game_and_the_whole_window(pool: PgPool) -> cja::Result<()> {
        let seed = Seed::new(pool).await?;

        let older = seed.game("finished", 20).await?;
        let older_gb = seed.join_direct(older, seed.battlesnake_id).await?;
        seed.moves(older, older_gb, &[Some(10), Some(20), Some(30), None])
            .await?;

        let newer = seed.game("finished", 10).await?;
        let newer_gb = seed.join_via_leaderboard(newer).await?;
        seed.moves(newer, newer_gb, &[Some(100), Some(200)]).await?;

        let recent = seed.fetch(RECENT_GAMES_LIMIT).await?;

        let ids: Vec<Uuid> = recent.games.iter().map(|g| g.game_id).collect();
        assert_eq!(ids, vec![older, newer], "oldest first, both link kinds");

        assert_stats(recent.games[0].stats, 3, 1, Some(20.0), Some(29.0));
        assert_stats(recent.games[1].stats, 2, 0, Some(150.0), Some(195.0));

        // Window percentiles come from the raw moves [10,20,30,100,200], not
        // from averaging the two games' percentiles.
        assert_stats(recent.overall, 5, 1, Some(30.0), Some(180.0));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn a_fully_timed_out_game_has_no_percentiles(pool: PgPool) -> cja::Result<()> {
        let seed = Seed::new(pool).await?;
        let game = seed.game("finished", 5).await?;
        let gb = seed.join_direct(game, seed.battlesnake_id).await?;
        seed.moves(game, gb, &[None, None, None]).await?;

        let recent = seed.fetch(RECENT_GAMES_LIMIT).await?;

        assert_eq!(recent.games.len(), 1);
        assert_stats(recent.games[0].stats, 0, 3, None, None);
        assert_eq!(recent.overall, recent.games[0].stats);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn skips_unfinished_foreign_and_pre_tracking_moves(pool: PgPool) -> cja::Result<()> {
        let seed = Seed::new(pool).await?;

        let running = seed.game("running", 1).await?;
        let running_gb = seed.join_direct(running, seed.battlesnake_id).await?;
        seed.moves(running, running_gb, &[Some(1)]).await?;

        // Another snake in the same finished game must not leak in.
        let owner_id = sqlx::query_scalar!(
            "SELECT user_id FROM battlesnakes WHERE battlesnake_id = $1",
            seed.battlesnake_id
        )
        .fetch_one(&seed.pool)
        .await?;
        let rival = Seed::snake(&seed.pool, owner_id, "Rival").await?;
        let shared = seed.game("finished", 3).await?;
        let mine = seed.join_direct(shared, seed.battlesnake_id).await?;
        let theirs = seed.join_direct(shared, rival).await?;
        seed.moves(shared, mine, &[Some(40)]).await?;
        seed.moves(shared, theirs, &[Some(400), None]).await?;

        // Rows from before latency tracking: NULL latency, not a timeout.
        let legacy = seed.game("finished", 30).await?;
        let legacy_gb = seed.join_direct(legacy, seed.battlesnake_id).await?;
        seed.raw_move(legacy, legacy_gb, None, false).await?;

        let recent = seed.fetch(RECENT_GAMES_LIMIT).await?;

        assert_eq!(recent.games.len(), 1);
        assert_eq!(recent.games[0].game_id, shared);
        assert_stats(recent.overall, 1, 0, Some(40.0), Some(40.0));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn limit_keeps_the_most_recent_games(pool: PgPool) -> cja::Result<()> {
        let seed = Seed::new(pool).await?;
        let mut games = Vec::new();
        for minutes_ago in [40, 30, 20, 10] {
            let game = seed.game("finished", minutes_ago).await?;
            let gb = seed.join_direct(game, seed.battlesnake_id).await?;
            seed.moves(game, gb, &[Some(minutes_ago)]).await?;
            games.push(game);
        }

        let recent = seed.fetch(2).await?;

        let ids: Vec<Uuid> = recent.games.iter().map(|g| g.game_id).collect();
        assert_eq!(ids, games[2..].to_vec());
        assert_eq!(recent.overall.answered, 2);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn no_games_means_an_empty_summary(pool: PgPool) -> cja::Result<()> {
        let seed = Seed::new(pool).await?;

        let recent = seed.fetch(RECENT_GAMES_LIMIT).await?;

        assert!(recent.games.is_empty());
        assert_eq!(recent.overall, LatencyStats::default());

        Ok(())
    }
}
