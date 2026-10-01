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

    pub fn combined_rating(&self) -> Option<CombinedRating> {
        combined_rating(
            &self.best_scores,
            self.enabled_leaderboards.try_into().ok()?,
        )
        .and_then(CombinedRating::new)
    }
}

#[derive(Debug, Clone)]
pub struct GlobalRankingEntry {
    pub score: GlobalPlayerScore,
    pub github_login: String,
    pub public_name: String,
    pub github_avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CombinedRating(f64);

impl CombinedRating {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(Self(value))
    }

    pub fn display_value(self) -> i32 {
        (self.0 * 10_000.0) as i32
    }
}

pub fn combined_rating(best_per_board: &[f64], enabled_leaderboards: usize) -> Option<f64> {
    if best_per_board.is_empty() || enabled_leaderboards == 0 {
        return None;
    }
    const MU: f64 = 25.0;
    const SIGMA: f64 = MU / 3.0;
    let n = enabled_leaderboards as f64;
    let z = (best_per_board.iter().sum::<f64>() - MU * n) / (SIGMA * n);
    z.is_finite().then(|| 1.0 / (1.0 + (-z).exp()))
}

struct FlatRankingEntry {
    user_id: Uuid,
    best_scores: Vec<f64>,
    total_score: f64,
    enabled_leaderboards: i64,
    github_login: String,
    public_name: String,
    github_avatar_url: Option<String>,
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
                  u.github_login, u.github_avatar_url,
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
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test') RETURNING user_id",
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
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(Uuid::new_v4().to_string())
        .fetch_one(pool).await?;
        let entry_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, display_score, games_played)
             VALUES ($1, $2, $3, $4) RETURNING leaderboard_entry_id",
        )
        .bind(board_id).bind(snake_id).bind(score).bind(games)
        .fetch_one(pool).await?;
        Ok((snake_id, entry_id))
    }

    #[test]
    fn known_rating_truncates() {
        // (30,5) and (25,5) expose to 15 and 10. z=(25-50)/(50/3)=-1.5.
        let rating = combined_rating(&[15.0, 10.0], 2).unwrap();
        assert!((rating - 0.182_425_523_806_356_35).abs() < 1e-12);
        assert_eq!(CombinedRating::new(rating).unwrap().display_value(), 1824);
    }

    #[test]
    fn empty_or_zero_boards_is_unranked() {
        assert_eq!(combined_rating(&[], 4), None);
        assert_eq!(combined_rating(&[15.0], 0), None);
    }

    #[test]
    fn missing_positive_board_lowers_rating() {
        assert!(combined_rating(&[15.0], 2) < combined_rating(&[15.0, 15.0], 2));
    }

    proptest! {
        #[test]
        fn rating_is_bounded_and_monotone(
            case in (1usize..=8).prop_flat_map(|n| {
                (Just(n), proptest::collection::vec(-100.0f64..100.0, 1..=n))
            }),
            increase in 0.0f64..50.0,
        ) {
            let (n, scores) = case;
            let before = combined_rating(&scores, n).unwrap();
            prop_assert!(before > 0.0 && before < 1.0);
            let mut improved = scores;
            improved[0] += increase;
            prop_assert!(combined_rating(&improved, n).unwrap() >= before);
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
        assert_eq!(before.combined_rating(), after.combined_rating());
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
            assert_eq!(before.combined_rating(), after.combined_rating());
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
            "UPDATE leaderboard_entries SET disabled_at = now() WHERE leaderboard_entry_id = $1",
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
}
