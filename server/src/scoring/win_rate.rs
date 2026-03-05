use async_trait::async_trait;
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::leaderboard;

use super::{EntryScore, GameResultEvent, ScoringAlgorithm};

/// Win Rate scoring algorithm implementation.
pub struct WinRateScoring;

#[async_trait]
impl ScoringAlgorithm for WinRateScoring {
    fn key(&self) -> &'static str {
        "win_rate"
    }

    fn display_name(&self) -> &'static str {
        "Win Rate"
    }

    fn score_column_name(&self) -> &'static str {
        "Win %"
    }

    async fn initialize_entry(&self, pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<()> {
        sqlx::query(
            "INSERT INTO win_rate_stats (leaderboard_entry_id) \
             VALUES ($1) \
             ON CONFLICT (leaderboard_entry_id) DO NOTHING",
        )
        .bind(leaderboard_entry_id)
        .execute(pool)
        .await
        .wrap_err("Failed to initialize win_rate_stats entry")?;

        Ok(())
    }

    async fn process_game_result(
        &self,
        conn: &mut sqlx::PgConnection,
        event: &GameResultEvent,
    ) -> cja::Result<()> {
        for result in &event.results {
            let is_win = result.placement == 1;

            // Try UPDATE first
            let rows_affected = sqlx::query(
                "UPDATE win_rate_stats SET \
                    games_played = games_played + 1, \
                    wins = wins + CASE WHEN $2 THEN 1 ELSE 0 END, \
                    losses = losses + CASE WHEN $2 THEN 0 ELSE 1 END, \
                    score = CASE WHEN games_played + 1 > 0 \
                        THEN (wins + CASE WHEN $2 THEN 1 ELSE 0 END)::double precision \
                             / (games_played + 1)::double precision * 100.0 \
                        ELSE 0.0 END, \
                    updated_at = NOW() \
                 WHERE leaderboard_entry_id = $1",
            )
            .bind(result.leaderboard_entry_id)
            .bind(is_win)
            .execute(&mut *conn)
            .await
            .wrap_err("Failed to update win_rate_stats")?
            .rows_affected();

            // If no row existed, lazily insert then retry
            if rows_affected == 0 {
                sqlx::query(
                    "INSERT INTO win_rate_stats (leaderboard_entry_id) \
                     VALUES ($1) \
                     ON CONFLICT (leaderboard_entry_id) DO NOTHING",
                )
                .bind(result.leaderboard_entry_id)
                .execute(&mut *conn)
                .await
                .wrap_err("Failed to lazy-insert win_rate_stats")?;

                sqlx::query(
                    "UPDATE win_rate_stats SET \
                        games_played = games_played + 1, \
                        wins = wins + CASE WHEN $2 THEN 1 ELSE 0 END, \
                        losses = losses + CASE WHEN $2 THEN 0 ELSE 1 END, \
                        score = CASE WHEN games_played + 1 > 0 \
                            THEN (wins + CASE WHEN $2 THEN 1 ELSE 0 END)::double precision \
                                 / (games_played + 1)::double precision * 100.0 \
                            ELSE 0.0 END, \
                        updated_at = NOW() \
                     WHERE leaderboard_entry_id = $1",
                )
                .bind(result.leaderboard_entry_id)
                .bind(is_win)
                .execute(&mut *conn)
                .await
                .wrap_err("Failed to retry update win_rate_stats")?;
            }
        }

        Ok(())
    }

    async fn get_scores(
        &self,
        pool: &PgPool,
        leaderboard_id: Uuid,
    ) -> cja::Result<Vec<EntryScore>> {
        let rows = sqlx::query_as::<_, WinRateRow>(
            "SELECT wrs.leaderboard_entry_id, wrs.score, wrs.wins, wrs.losses, wrs.games_played \
             FROM win_rate_stats wrs \
             JOIN leaderboard_entries le ON wrs.leaderboard_entry_id = le.leaderboard_entry_id \
             WHERE le.leaderboard_id = $1 \
               AND le.disabled_at IS NULL \
               AND le.games_played >= $2 \
             ORDER BY wrs.score DESC",
        )
        .bind(leaderboard_id)
        .bind(leaderboard::MIN_GAMES_FOR_RANKING)
        .fetch_all(pool)
        .await
        .wrap_err("Failed to fetch win-rate scores")?;

        Ok(rows
            .into_iter()
            .map(|r| EntryScore {
                leaderboard_entry_id: r.leaderboard_entry_id,
                score: r.score,
                details: vec![
                    ("wins".to_string(), r.wins.to_string()),
                    ("losses".to_string(), r.losses.to_string()),
                    ("games_played".to_string(), r.games_played.to_string()),
                ],
            })
            .collect())
    }

    async fn get_entry_score(
        &self,
        pool: &PgPool,
        leaderboard_entry_id: Uuid,
    ) -> cja::Result<Option<EntryScore>> {
        let row = sqlx::query_as::<_, WinRateRow>(
            "SELECT leaderboard_entry_id, score, wins, losses, games_played \
             FROM win_rate_stats \
             WHERE leaderboard_entry_id = $1",
        )
        .bind(leaderboard_entry_id)
        .fetch_optional(pool)
        .await
        .wrap_err("Failed to fetch win-rate entry score")?;

        Ok(row.map(|r| EntryScore {
            leaderboard_entry_id: r.leaderboard_entry_id,
            score: r.score,
            details: vec![
                ("wins".to_string(), r.wins.to_string()),
                ("losses".to_string(), r.losses.to_string()),
                ("games_played".to_string(), r.games_played.to_string()),
            ],
        }))
    }
}

#[derive(sqlx::FromRow)]
struct WinRateRow {
    leaderboard_entry_id: Uuid,
    score: f64,
    wins: i32,
    losses: i32,
    games_played: i32,
}

/// Win rate = wins / games_played * 100.0
fn compute_win_rate(wins: i32, games_played: i32) -> f64 {
    if games_played > 0 {
        wins as f64 / games_played as f64 * 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::ScoringAlgorithm;
    use uuid::Uuid;

    #[test]
    fn test_win_rate_key() {
        let algo = WinRateScoring;
        assert_eq!(algo.key(), "win_rate");
    }

    #[test]
    fn test_win_rate_display_name() {
        let algo = WinRateScoring;
        assert_eq!(algo.display_name(), "Win Rate");
    }

    #[test]
    fn test_win_rate_score_column_name() {
        let algo = WinRateScoring;
        assert_eq!(algo.score_column_name(), "Win %");
    }

    #[test]
    fn test_win_rate_perfect_record() {
        let rate = compute_win_rate(10, 10);
        assert!(
            (rate - 100.0).abs() < f64::EPSILON,
            "10 wins out of 10 games should be 100%, got {}",
            rate
        );
    }

    #[test]
    fn test_win_rate_no_wins() {
        let rate = compute_win_rate(0, 10);
        assert!(
            rate.abs() < f64::EPSILON,
            "0 wins out of 10 games should be 0%, got {}",
            rate
        );
    }

    #[test]
    fn test_win_rate_half_wins() {
        let rate = compute_win_rate(5, 10);
        assert!(
            (rate - 50.0).abs() < f64::EPSILON,
            "5 wins out of 10 games should be 50%, got {}",
            rate
        );
    }

    #[test]
    fn test_win_rate_division_by_zero() {
        let rate = compute_win_rate(0, 0);
        assert!(
            rate.abs() < f64::EPSILON,
            "0 games played should return 0.0, not NaN or infinity, got {}",
            rate
        );
    }

    #[test]
    fn test_placement_1_is_win() {
        let placement = 1;
        let is_win = placement == 1;
        assert!(is_win, "Placement 1 should count as a win");
    }

    #[test]
    fn test_placement_not_1_is_loss() {
        for placement in [2, 3, 4] {
            let is_win = placement == 1;
            assert!(!is_win, "Placement {} should not count as a win", placement);
        }
    }

    #[test]
    fn test_win_rate_incremental_update() {
        let mut wins = 3;
        let mut games_played = 10;

        let placement = 1;
        games_played += 1;
        if placement == 1 {
            wins += 1;
        }
        let rate = compute_win_rate(wins, games_played);
        assert!(
            (rate - (4.0 / 11.0 * 100.0)).abs() < 1e-10,
            "After winning: 4/11 * 100 = {:.4}, got {:.4}",
            4.0 / 11.0 * 100.0,
            rate
        );

        let placement = 3;
        games_played += 1;
        if placement == 1 {
            wins += 1;
        }
        let rate = compute_win_rate(wins, games_played);
        assert!(
            (rate - (4.0 / 12.0 * 100.0)).abs() < 1e-10,
            "After losing: 4/12 * 100 = {:.4}, got {:.4}",
            4.0 / 12.0 * 100.0,
            rate
        );
    }

    #[test]
    fn test_win_rate_entry_score_details() {
        use crate::scoring::EntryScore;

        let score = EntryScore {
            leaderboard_entry_id: Uuid::new_v4(),
            score: 50.0,
            details: vec![
                ("wins".to_string(), "5".to_string()),
                ("losses".to_string(), "5".to_string()),
                ("games_played".to_string(), "10".to_string()),
            ],
        };

        assert_eq!(
            score.details.len(),
            3,
            "WinRate should provide wins, losses, games_played"
        );
        assert_eq!(score.details[0].0, "wins");
        assert_eq!(score.details[1].0, "losses");
        assert_eq!(score.details[2].0, "games_played");
    }
}
