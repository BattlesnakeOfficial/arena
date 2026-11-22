use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use super::leaderboard::MIN_GAMES_FOR_RANKING;

#[derive(Debug, Clone)]
pub struct GlobalPlayerScore {
    pub user_id: Uuid,
    pub best_scores: Vec<f64>,
    pub total_score: f64,
    pub enabled_leaderboards: i64,
}

impl GlobalPlayerScore {
    pub fn contributing_leaderboards(&self) -> usize {
        self.best_scores.len()
    }

    /// Combined rating in tenths: the sum of the player's best score on each
    /// leaderboard, each rounded to one decimal the way leaderboards display
    /// it, so the shown total adds up from the shown parts. A leaderboard the
    /// player isn't on adds nothing. Rankings sort by the unrounded
    /// `total_score`.
    fn total_tenths(&self) -> i64 {
        self.best_scores
            .iter()
            .map(|score| (score * 10.0).round() as i64)
            .sum()
    }

    /// The combined rating as displayed.
    pub fn display_total(&self) -> String {
        format_score(self.total_tenths() as f64 / 10.0)
    }

    /// Average best score over the leaderboards the player is on. Unlike the
    /// combined rating, skipping a leaderboard doesn't lower it.
    pub fn display_average(&self) -> String {
        let boards = self.contributing_leaderboards().max(1) as f64;
        format_score(self.total_tenths() as f64 / 10.0 / boards)
    }
}

/// One decimal, matching per-leaderboard scores, without a "-0.0".
fn format_score(value: f64) -> String {
    let formatted = format!("{value:.1}");
    if formatted == "-0.0" {
        "0.0".to_string()
    } else {
        formatted
    }
}

#[derive(Debug, Clone)]
pub struct GlobalRankingEntry {
    pub score: GlobalPlayerScore,
    pub github_login: String,
    pub public_name: String,
    pub github_avatar_url: Option<String>,
    /// ISO-3166-1 alpha-2 country ('' when unset). Display-only.
    pub country: String,
}

struct FlatRankingEntry {
    user_id: Uuid,
    best_scores: Vec<f64>,
    total_score: f64,
    enabled_leaderboards: i64,
    github_login: String,
    public_name: String,
    github_avatar_url: Option<String>,
    country: String,
}

impl From<FlatRankingEntry> for GlobalRankingEntry {
    fn from(row: FlatRankingEntry) -> Self {
        Self {
            score: GlobalPlayerScore {
                user_id: row.user_id,
                best_scores: row.best_scores,
                total_score: row.total_score,
                enabled_leaderboards: row.enabled_leaderboards,
            },
            github_login: row.github_login,
            public_name: row.public_name,
            github_avatar_url: row.github_avatar_url,
            country: row.country,
        }
    }
}

pub async fn count_global_players(pool: &PgPool) -> cja::Result<i64> {
    sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"count!\" FROM global_player_scores($1)",
        MIN_GAMES_FOR_RANKING
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count globally ranked players")
}

pub async fn get_global_players_paginated(
    pool: &PgPool,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<GlobalRankingEntry>> {
    let rows: Vec<FlatRankingEntry> = sqlx::query_as!(
        FlatRankingEntry,
        r#"SELECT g.user_id AS "user_id!", g.best_scores AS "best_scores!", g.total_score AS "total_score!",
                  g.enabled_leaderboards AS "enabled_leaderboards!",
                  u.github_login, u.github_avatar_url, u.country,
                  COALESCE(NULLIF(u.display_name, ''), u.github_login) AS "public_name!"
           FROM global_player_scores($1) g
           JOIN users u ON u.user_id = g.user_id
           ORDER BY g.total_score DESC,
                    LOWER(COALESCE(NULLIF(u.display_name, ''), u.github_login)) ASC,
                    LOWER(u.github_login) ASC, u.user_id ASC
           LIMIT $2 OFFSET $3"#,
        MIN_GAMES_FOR_RANKING,
        per_page,
        page * per_page
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch global rankings")?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn get_global_player_score(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<Option<GlobalPlayerScore>> {
    sqlx::query_as!(
        GlobalPlayerScore,
        "SELECT g.user_id AS \"user_id!\", g.best_scores AS \"best_scores!\", g.total_score AS \"total_score!\",
                g.enabled_leaderboards AS \"enabled_leaderboards!\"
         FROM global_player_scores($1) g WHERE g.user_id = $2",
        MIN_GAMES_FOR_RANKING,
        user_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch global player score")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    async fn user(pool: &PgPool, id: i64, login: &str) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login)
             VALUES ($1, $2) RETURNING user_id",
        )
        .bind(id)
        .bind(login)
        .fetch_one(pool)
        .await?)
    }

    async fn board(pool: &PgPool) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar("SELECT leaderboard_id FROM leaderboards WHERE disabled_at IS NULL ORDER BY name LIMIT 1")
            .fetch_one(pool).await?)
    }

    async fn entry(
        pool: &PgPool,
        user_id: Uuid,
        board_id: Uuid,
        score: f64,
        games: i32,
    ) -> cja::Result<(Uuid, Uuid)> {
        let snake_id = snake_for(pool, user_id).await?;
        let entry_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, season_id, battlesnake_id, display_score, games_played)
             VALUES ($1, (SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1), $2, $3, $4) RETURNING leaderboard_entry_id",
        )
        .bind(board_id).bind(snake_id).bind(score).bind(games)
        .fetch_one(pool).await?;
        crate::routes::test_support::register_snake_entries(pool, snake_id).await;
        Ok((snake_id, entry_id))
    }

    /// A fresh snake owned by `user_id`, with a unique name.
    async fn snake_for(pool: &PgPool, user_id: Uuid) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(Uuid::new_v4().to_string())
        .fetch_one(pool)
        .await?)
    }

    /// A public league; the insert trigger gives it an open-ended Preseason.
    async fn league(pool: &PgPool, name: &str) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
        )
        .bind(name)
        .fetch_one(pool)
        .await?)
    }

    /// Insert a season for a league and return its id. Bound the auto-created
    /// open-ended Preseason first or the exclusion constraint rejects overlaps.
    async fn season(
        pool: &PgPool,
        leaderboard_id: Uuid,
        name: &str,
        starts_at: &str,
        ends_at: Option<&str>,
    ) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at)
             VALUES ($1, $2, $3::timestamptz, $4::timestamptz) RETURNING season_id",
        )
        .bind(leaderboard_id)
        .bind(name)
        .bind(starts_at)
        .bind(ends_at)
        .fetch_one(pool)
        .await?)
    }

    /// The league's auto-created Preseason (filtered by name so leagues with a
    /// second season still resolve to one row).
    async fn preseason(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1 AND name = 'Preseason'",
        )
        .bind(leaderboard_id)
        .fetch_one(pool)
        .await?)
    }

    /// Registration for (league, snake). Only call when none exists yet:
    /// `entry()` already registers its snake.
    async fn register(
        pool: &PgPool,
        leaderboard_id: Uuid,
        snake_id: Uuid,
        disabled: bool,
    ) -> cja::Result<()> {
        sqlx::query(
            "INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id, disabled_at)
             VALUES ($1, $2, CASE WHEN $3 THEN now() ELSE NULL END)",
        )
        .bind(leaderboard_id)
        .bind(snake_id)
        .bind(disabled)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Entry for an existing snake in an explicit season. Does not register.
    async fn entry_in_season(
        pool: &PgPool,
        snake_id: Uuid,
        leaderboard_id: Uuid,
        season_id: Uuid,
        score: f64,
        games: i32,
    ) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, season_id, battlesnake_id, display_score, games_played)
             VALUES ($1, $2, $3, $4, $5) RETURNING leaderboard_entry_id",
        )
        .bind(leaderboard_id)
        .bind(season_id)
        .bind(snake_id)
        .bind(score)
        .bind(games)
        .fetch_one(pool)
        .await?)
    }

    /// Denominator oracle: non-disabled public leagues with an active season.
    async fn active_public_count(pool: &PgPool) -> cja::Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM leaderboards l
             WHERE l.disabled_at IS NULL AND l.visibility = 'public'
               AND EXISTS (SELECT 1 FROM leaderboard_seasons s
                           WHERE s.leaderboard_id = l.leaderboard_id
                             AND s.starts_at <= clock_timestamp()
                             AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at))",
        )
        .fetch_one(pool)
        .await?)
    }

    async fn public_enabled_count(pool: &PgPool) -> cja::Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM leaderboards WHERE disabled_at IS NULL AND visibility = 'public'",
        )
        .fetch_one(pool)
        .await?)
    }

    async fn timestamp_from_now(pool: &PgPool, offset: &str) -> cja::Result<String> {
        Ok(sqlx::query_scalar("SELECT (now() + $1::interval)::text")
            .bind(offset)
            .fetch_one(pool)
            .await?)
    }

    fn sorted(mut scores: Vec<f64>) -> Vec<f64> {
        scores.sort_by(f64::total_cmp);
        scores
    }

    fn score(best_scores: Vec<f64>) -> GlobalPlayerScore {
        GlobalPlayerScore {
            user_id: Uuid::nil(),
            total_score: best_scores.iter().sum(),
            best_scores,
            enabled_leaderboards: 4,
        }
    }

    #[test]
    fn total_adds_up_from_the_displayed_parts() {
        // Odiodin on prod, 2026-10-01: the parts display as 56.1 + 45.7 + 41.4 + 25.7.
        let odiodin = score(vec![
            56.053_639_800_838_26,
            45.703_613_265_834_47,
            41.394_509_841_322_66,
            25.681_469_501_569_946,
        ]);
        assert_eq!(odiodin.display_total(), "168.9");
        assert_eq!(odiodin.display_average(), "42.2");
    }

    #[test]
    fn missing_board_lowers_total_but_not_average() {
        let everywhere = score(vec![15.0, 15.0]);
        let one_board = score(vec![15.0]);
        assert_eq!(everywhere.display_total(), "30.0");
        assert_eq!(one_board.display_total(), "15.0");
        assert_eq!(everywhere.display_average(), one_board.display_average());
    }

    #[test]
    fn negative_scores_display_without_negative_zero() {
        assert_eq!(score(vec![-0.04]).display_total(), "0.0");
        assert_eq!(score(vec![-0.1, 0.0, 0.0, 0.0]).display_average(), "0.0");
        assert_eq!(score(vec![-3.25, 1.0]).display_total(), "-2.3");
    }

    proptest! {
        #[test]
        fn total_is_monotone_in_each_best_score(
            scores in proptest::collection::vec(-100.0f64..100.0, 1..=8),
            increase in 0.0f64..50.0,
        ) {
            let before = score(scores.clone());
            let mut improved = scores;
            improved[0] += increase;
            let after = score(improved);
            prop_assert!(after.total_tenths() >= before.total_tenths());
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn weaker_second_snake_does_not_change_score(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "best").await?;
        let board_id = board(&pool).await?;
        entry(&pool, user_id, board_id, 16.0, 10).await?;
        let before = get_global_player_score(&pool, user_id).await?.unwrap();
        entry(&pool, user_id, board_id, 8.0, 10).await?;
        let after = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(before.best_scores, after.best_scores);
        assert_eq!(before.total_score, after.total_score);
        assert_eq!(after.contributing_leaderboards(), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn weaker_second_snake_property_through_production_query(
        pool: PgPool,
    ) -> cja::Result<()> {
        use proptest::strategy::ValueTree as _;
        let boards: Vec<Uuid> = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE disabled_at IS NULL ORDER BY leaderboard_id",
        ).fetch_all(&pool).await?;
        let mut runner = proptest::test_runner::TestRunner::default();
        let strategy = (
            1usize..=boards.len(),
            proptest::collection::vec(-80.0f64..80.0, boards.len()),
        );
        for case in 0..24 {
            let (count, scores) = strategy
                .new_tree(&mut runner)
                .map_err(|error| color_eyre::eyre::eyre!("{error}"))?
                .current();
            let user_id = user(&pool, 1000 + case, &format!("property-{case}")).await?;
            for (board_id, score) in boards.iter().zip(scores).take(count) {
                entry(&pool, user_id, *board_id, score, 10).await?;
            }
            let before = get_global_player_score(&pool, user_id).await?.unwrap();
            entry(&pool, user_id, boards[0], before.best_scores[0] - 1.0, 10).await?;
            let after = get_global_player_score(&pool, user_id).await?.unwrap();
            assert_eq!(before.best_scores, after.best_scores);
            assert_eq!(before.total_score, after.total_score);
            assert_eq!(
                before.contributing_leaderboards(),
                after.contributing_leaderboards()
            );
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn nine_games_do_not_qualify_ten_do(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "threshold").await?;
        let (_, entry_id) = entry(&pool, user_id, board(&pool).await?, 16.0, 9).await?;
        assert!(get_global_player_score(&pool, user_id).await?.is_none());
        assert_eq!(count_global_players(&pool).await?, 0);
        sqlx::query(
            "UPDATE leaderboard_entries SET games_played = 10 WHERE leaderboard_entry_id = $1",
        )
        .bind(entry_id)
        .execute(&pool)
        .await?;
        assert_eq!(
            get_global_player_score(&pool, user_id)
                .await?
                .unwrap()
                .best_scores,
            vec![16.0]
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_entry_is_excluded(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "entry-disabled").await?;
        let (_, entry_id) = entry(&pool, user_id, board(&pool).await?, 16.0, 10).await?;
        sqlx::query(
            "UPDATE leaderboard_registrations r SET disabled_at = now() FROM leaderboard_entries le WHERE le.leaderboard_entry_id = $1 AND r.leaderboard_id = le.leaderboard_id AND r.battlesnake_id = le.battlesnake_id",
        )
        .bind(entry_id)
        .execute(&pool)
        .await?;
        assert!(get_global_player_score(&pool, user_id).await?.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_board_is_excluded(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "board-disabled").await?;
        let board_id = board(&pool).await?;
        entry(&pool, user_id, board_id, 16.0, 10).await?;
        sqlx::query("UPDATE leaderboards SET disabled_at = now() WHERE leaderboard_id = $1")
            .bind(board_id)
            .execute(&pool)
            .await?;
        assert!(get_global_player_score(&pool, user_id).await?.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_snake_is_excluded(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "deleted").await?;
        let (snake_id, _) = entry(&pool, user_id, board(&pool).await?, 16.0, 10).await?;
        sqlx::query("UPDATE battlesnakes SET deleted_at = now() WHERE battlesnake_id = $1")
            .bind(snake_id)
            .execute(&pool)
            .await?;
        assert!(get_global_player_score(&pool, user_id).await?.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn private_snake_counts_and_empty_boards_are_in_denominator(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = user(&pool, 1, "private").await?;
        let (snake_id, _) = entry(&pool, user_id, board(&pool).await?, 16.0, 10).await?;
        sqlx::query("UPDATE battlesnakes SET visibility = 'private' WHERE battlesnake_id = $1")
            .bind(snake_id)
            .execute(&pool)
            .await?;
        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(score.best_scores, vec![16.0]);
        let enabled: i64 =
            sqlx::query_scalar("SELECT count(*) FROM leaderboards WHERE disabled_at IS NULL")
                .fetch_one(&pool)
                .await?;
        assert_eq!(score.enabled_leaderboards, enabled);
        assert!(enabled > 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn count_page_and_profile_agree(pool: PgPool) -> cja::Result<()> {
        let ranked = user(&pool, 1, "ranked").await?;
        let unranked = user(&pool, 2, "unranked").await?;
        entry(&pool, ranked, board(&pool).await?, 16.0, 10).await?;
        assert_eq!(count_global_players(&pool).await?, 1);
        let page = get_global_players_paginated(&pool, 0, 50).await?;
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].score.user_id, ranked);
        assert!(get_global_player_score(&pool, unranked).await?.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn ties_sort_by_public_name_then_login_then_uuid(pool: PgPool) -> cja::Result<()> {
        let board_id = board(&pool).await?;
        let a = user(&pool, 1, "zed").await?;
        let b = user(&pool, 2, "bee").await?;
        let c = user(&pool, 3, "ace").await?;
        let d = user(&pool, 4, "bee").await?;
        for user_id in [a, b, c, d] {
            entry(&pool, user_id, board_id, 15.0, 10).await?;
        }
        sqlx::query("UPDATE users SET display_name = 'Same' WHERE user_id = ANY($1)")
            .bind([a, b, d])
            .execute(&pool)
            .await?;
        let (first_bee, second_bee) = if b < d { (b, d) } else { (d, b) };
        let page = get_global_players_paginated(&pool, 0, 50).await?;
        assert_eq!(
            page.iter().map(|row| row.score.user_id).collect::<Vec<_>>(),
            vec![c, first_bee, second_bee, a]
        );
        assert_eq!(page[0].public_name, "ace");
        assert!(page.iter().all(|row| row.github_avatar_url.is_none()));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn committed_rating_changes_are_visible_to_both_players(pool: PgPool) -> cja::Result<()> {
        let board_id = board(&pool).await?;
        let first = user(&pool, 1, "first").await?;
        let second = user(&pool, 2, "second").await?;
        let (_, first_entry) = entry(&pool, first, board_id, 10.0, 10).await?;
        let (_, second_entry) = entry(&pool, second, board_id, 10.0, 10).await?;
        let mut tx = pool.begin().await?;
        super::super::leaderboard::update_rating(&mut *tx, first_entry, 30.0, 5.0, 15.0, true)
            .await?;
        super::super::leaderboard::update_rating(&mut *tx, second_entry, 27.0, 5.0, 12.0, false)
            .await?;
        tx.commit().await?;
        assert_eq!(
            get_global_player_score(&pool, first)
                .await?
                .unwrap()
                .best_scores,
            vec![15.0]
        );
        assert_eq!(
            get_global_player_score(&pool, second)
                .await?
                .unwrap()
                .best_scores,
            vec![12.0]
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn active_season_entries_count_like_today(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "active").await?;
        let board_id = board(&pool).await?;
        entry(&pool, user_id, board_id, 20.0, 10).await?;
        entry(&pool, user_id, board_id, 15.0, 10).await?;
        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(score.best_scores, vec![20.0]);
        assert_eq!(score.total_score, 20.0);
        assert_eq!(
            score.enabled_leaderboards,
            active_public_count(&pool).await?
        );
        assert_eq!(score.contributing_leaderboards(), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn gap_and_future_leagues_contribute_nothing(pool: PgPool) -> cja::Result<()> {
        let baseline = active_public_count(&pool).await?;
        assert_eq!(baseline, 4);
        let user_id = user(&pool, 1, "gap").await?;

        let active = league(&pool, "Active").await?;
        entry(&pool, user_id, active, 20.0, 10).await?;

        let gap = league(&pool, "Gap").await?;
        sqlx::query(
            "UPDATE leaderboard_seasons SET starts_at = '2020-01-01', ends_at = '2021-01-01' WHERE leaderboard_id = $1",
        )
        .bind(gap)
        .execute(&pool)
        .await?;
        let gap_snake = snake_for(&pool, user_id).await?;
        entry_in_season(
            &pool,
            gap_snake,
            gap,
            preseason(&pool, gap).await?,
            30.0,
            10,
        )
        .await?;
        register(&pool, gap, gap_snake, false).await?;

        let future: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name, created_at) VALUES ('Future', '2035-01-01') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let future_snake = snake_for(&pool, user_id).await?;
        entry_in_season(
            &pool,
            future_snake,
            future,
            preseason(&pool, future).await?,
            50.0,
            10,
        )
        .await?;
        register(&pool, future, future_snake, false).await?;

        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(score.best_scores, vec![20.0]);
        assert_eq!(score.total_score, 20.0);
        assert_eq!(
            score.enabled_leaderboards,
            active_public_count(&pool).await?
        );
        assert_eq!(score.enabled_leaderboards, baseline + 1);
        // The gap and future leagues are public and enabled, just not in season.
        assert_eq!(public_enabled_count(&pool).await?, baseline + 3);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn past_season_entries_do_not_count(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "past").await?;
        let lb = league(&pool, "Seasoned").await?;
        let past = preseason(&pool, lb).await?;
        sqlx::query(
            "UPDATE leaderboard_seasons SET starts_at = '2020-01-01', ends_at = '2026-01-01' WHERE season_id = $1",
        )
        .bind(past)
        .execute(&pool)
        .await?;
        let current = season(&pool, lb, "Current", "2026-01-01", None).await?;
        let snake = snake_for(&pool, user_id).await?;
        entry_in_season(&pool, snake, lb, past, 40.0, 10).await?;
        entry_in_season(&pool, snake, lb, current, 20.0, 10).await?;
        register(&pool, lb, snake, false).await?;

        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(score.best_scores, vec![20.0]);
        assert_eq!(score.total_score, 20.0);
        assert_eq!(
            score.enabled_leaderboards,
            active_public_count(&pool).await?
        );
        assert_eq!(
            score.enabled_leaderboards,
            public_enabled_count(&pool).await?
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn private_and_disabled_leagues_do_not_count(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "private-disabled").await?;
        let public = league(&pool, "Public").await?;
        entry(&pool, user_id, public, 20.0, 10).await?;
        let private: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name, visibility, invite_code) VALUES ('Private', 'private', 'x') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        entry(&pool, user_id, private, 50.0, 10).await?;
        let disabled = league(&pool, "Disabled").await?;
        entry(&pool, user_id, disabled, 60.0, 10).await?;
        sqlx::query("UPDATE leaderboards SET disabled_at = now() WHERE leaderboard_id = $1")
            .bind(disabled)
            .execute(&pool)
            .await?;

        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(score.best_scores, vec![20.0]);
        assert_eq!(
            score.enabled_leaderboards,
            active_public_count(&pool).await?
        );

        // Flipping both leagues back to public and enabled makes them count.
        sqlx::query(
            "UPDATE leaderboards SET visibility = 'public', invite_code = NULL, disabled_at = NULL
             WHERE leaderboard_id = ANY($1)",
        )
        .bind([private, disabled])
        .execute(&pool)
        .await?;
        let score = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(sorted(score.best_scores), vec![20.0, 50.0, 60.0]);
        assert_eq!(
            score.enabled_leaderboards,
            active_public_count(&pool).await?
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn left_registration_entries_do_not_count(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "left").await?;
        let lb = board(&pool).await?;
        let snake = snake_for(&pool, user_id).await?;
        entry_in_season(&pool, snake, lb, preseason(&pool, lb).await?, 70.0, 10).await?;
        register(&pool, lb, snake, true).await?;
        assert!(get_global_player_score(&pool, user_id).await?.is_none());

        sqlx::query(
            "UPDATE leaderboard_registrations SET disabled_at = NULL
             WHERE leaderboard_id = $1 AND battlesnake_id = $2",
        )
        .bind(lb)
        .bind(snake)
        .execute(&pool)
        .await?;
        assert_eq!(
            get_global_player_score(&pool, user_id)
                .await?
                .unwrap()
                .best_scores,
            vec![70.0]
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rollover_switches_scores_without_a_job(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "rollover").await?;
        let lb = league(&pool, "Rolling").await?;
        let first = preseason(&pool, lb).await?;
        sqlx::query(
            "UPDATE leaderboard_seasons
             SET starts_at = now() - interval '2 days', ends_at = now() + interval '1 day'
             WHERE season_id = $1",
        )
        .bind(first)
        .execute(&pool)
        .await?;
        let s2_start = timestamp_from_now(&pool, "1 day").await?;
        let second = season(&pool, lb, "S2", &s2_start, None).await?;
        let snake = snake_for(&pool, user_id).await?;
        entry_in_season(&pool, snake, lb, first, 11.0, 10).await?;
        entry_in_season(&pool, snake, lb, second, 22.0, 10).await?;
        register(&pool, lb, snake, false).await?;

        let before = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(before.best_scores, vec![11.0]);
        assert_eq!(
            before.enabled_leaderboards,
            active_public_count(&pool).await?
        );

        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE leaderboard_seasons SET ends_at = now() WHERE season_id = $1")
            .bind(first)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE leaderboard_seasons SET starts_at = now() WHERE season_id = $1")
            .bind(second)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let after = get_global_player_score(&pool, user_id).await?.unwrap();
        assert_eq!(after.best_scores, vec![22.0]);
        assert_eq!(after.enabled_leaderboards, before.enabled_leaderboards);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn min_games_filters_within_active_season(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool, 1, "min-games").await?;
        let lb = board(&pool).await?;
        entry(&pool, user_id, lb, 99.0, 9).await?;
        entry(&pool, user_id, lb, 20.0, 10).await?;
        assert_eq!(
            get_global_player_score(&pool, user_id)
                .await?
                .unwrap()
                .best_scores,
            vec![20.0]
        );
        let unfiltered: Vec<f64> = sqlx::query_scalar(
            "SELECT best_scores FROM global_player_scores(0) WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(unfiltered, vec![99.0]);
        Ok(())
    }

    /// The Phase 2 `global_player_scores` body, taken verbatim from its
    /// migration with `min_games` bound as `$1`.
    fn old_body() -> String {
        const MIGRATION: &str = include_str!(
            "../../../migrations/20261121130200_membership_functions_registrations.up.sql"
        );
        let function = MIGRATION
            .split("CREATE OR REPLACE FUNCTION global_player_scores")
            .nth(1)
            .expect("Phase 2 migration defines global_player_scores");
        let body = function
            .split_once("LANGUAGE sql STABLE AS $$")
            .expect("function body start")
            .1
            .split_once("$$;")
            .expect("function body end")
            .0;
        assert_eq!(body.matches("min_games").count(), 1);
        body.replace("min_games", "$1")
    }

    type ScoreRow = (Uuid, Vec<f64>, f64, i64);

    fn assert_total_is_sum(rows: &[ScoreRow]) {
        for (user_id, best_scores, total, _) in rows {
            let fold = best_scores.iter().fold(0.0, |acc, score| acc + score);
            assert!(
                (total - fold).abs() <= 1e-9 * fold.abs().max(1.0),
                "{user_id}: total {total} vs sum {fold}"
            );
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn matches_prechange_function_on_preseason_shape(pool: PgPool) -> cja::Result<()> {
        let boards: Vec<Uuid> = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE disabled_at IS NULL ORDER BY name",
        )
        .fetch_all(&pool)
        .await?;
        let first = user(&pool, 1, "first").await?;
        let second = user(&pool, 2, "second").await?;
        for (i, board_id) in boards.iter().enumerate() {
            entry(&pool, first, *board_id, 10.0 + i as f64, 10).await?;
            entry(&pool, first, *board_id, 5.25 + i as f64, 12).await?;
        }
        entry(&pool, second, boards[0], 31.5, 10).await?;
        entry(&pool, second, boards[1], 44.0, 9).await?;
        let (deleted, _) = entry(&pool, second, boards[2], 70.0, 10).await?;
        sqlx::query("UPDATE battlesnakes SET deleted_at = now() WHERE battlesnake_id = $1")
            .bind(deleted)
            .execute(&pool)
            .await?;
        let (left, _) = entry(&pool, second, boards[3], 80.0, 10).await?;
        sqlx::query(
            "UPDATE leaderboard_registrations SET disabled_at = now()
             WHERE leaderboard_id = $1 AND battlesnake_id = $2",
        )
        .bind(boards[3])
        .bind(left)
        .execute(&pool)
        .await?;
        let private: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name, visibility, invite_code) VALUES ('Private', 'private', 'x') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        entry(&pool, second, private, 90.0, 10).await?;
        let disabled = league(&pool, "Disabled").await?;
        entry(&pool, first, disabled, 95.0, 10).await?;
        sqlx::query("UPDATE leaderboards SET disabled_at = now() WHERE leaderboard_id = $1")
            .bind(disabled)
            .execute(&pool)
            .await?;

        let old_query = format!("{} ORDER BY 1", old_body().trim_end());
        for min_games in [0, 10] {
            let old: Vec<ScoreRow> = // The SQL is the checked-in Phase 2 migration text, not user input.
            sqlx::query_as(sqlx::AssertSqlSafe(old_query.clone()))
                .bind(min_games)
                .fetch_all(&pool)
                .await?;
            let new: Vec<ScoreRow> = sqlx::query_as(
                "SELECT user_id, best_scores, total_score, enabled_leaderboards
                 FROM global_player_scores($1) ORDER BY user_id",
            )
            .bind(min_games)
            .fetch_all(&pool)
            .await?;
            assert_eq!(new.len(), 2);
            let triples = |rows: &[ScoreRow]| {
                rows.iter()
                    .map(|(user_id, scores, _, enabled)| (*user_id, scores.clone(), *enabled))
                    .collect::<Vec<_>>()
            };
            assert_eq!(triples(&old), triples(&new), "min_games = {min_games}");
            assert_total_is_sum(&old);
            assert_total_is_sum(&new);
        }
        Ok(())
    }
}
