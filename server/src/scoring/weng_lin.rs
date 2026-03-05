use async_trait::async_trait;
use color_eyre::eyre::Context as _;
use skillratings::MultiTeamOutcome;
use skillratings::weng_lin::{WengLinConfig, WengLinRating, weng_lin_multi_team};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::leaderboard::{self, LeaderboardEntry};

use super::{EntryScore, GameResultEvent, ScoringAlgorithm};

/// Computed rating update for a single snake in a game.
/// Separated from DB logic for testability.
#[derive(Debug)]
pub struct RatingUpdate {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    pub placement: i32,
    pub old_mu: f64,
    pub old_sigma: f64,
    pub new_mu: f64,
    pub new_sigma: f64,
    pub new_display_score: f64,
    pub display_score_change: f64,
    pub is_first_place: bool,
}

/// Pure computation: calculate new ratings from entries and placements.
/// No DB access — fully testable.
pub fn calculate_rating_updates(
    entries_with_placements: &[(LeaderboardEntry, i32)],
) -> Vec<RatingUpdate> {
    let config = WengLinConfig::new();

    let ratings: Vec<Vec<WengLinRating>> = entries_with_placements
        .iter()
        .map(|(entry, _)| {
            vec![WengLinRating {
                rating: entry.mu,
                uncertainty: entry.sigma,
            }]
        })
        .collect();

    let teams_and_ranks: Vec<(&[WengLinRating], MultiTeamOutcome)> = ratings
        .iter()
        .zip(entries_with_placements.iter())
        .map(|(team, (_, placement))| (team.as_slice(), MultiTeamOutcome::new(*placement as usize)))
        .collect();

    let new_ratings = weng_lin_multi_team(&teams_and_ranks, &config);

    entries_with_placements
        .iter()
        .enumerate()
        .map(|(i, (entry, placement))| {
            let new_rating = &new_ratings[i][0];
            let new_mu = new_rating.rating;
            let new_sigma = new_rating.uncertainty;
            let new_display_score = new_mu - 3.0 * new_sigma;
            let old_display_score = entry.mu - 3.0 * entry.sigma;

            RatingUpdate {
                leaderboard_entry_id: entry.leaderboard_entry_id,
                battlesnake_id: entry.battlesnake_id,
                placement: *placement,
                old_mu: entry.mu,
                old_sigma: entry.sigma,
                new_mu,
                new_sigma,
                new_display_score,
                display_score_change: new_display_score - old_display_score,
                is_first_place: *placement == 1,
            }
        })
        .collect()
}

/// Weng-Lin scoring algorithm implementation.
pub struct WengLinScoring;

#[async_trait]
impl ScoringAlgorithm for WengLinScoring {
    fn key(&self) -> &'static str {
        "weng_lin"
    }

    fn display_name(&self) -> &'static str {
        "Weng-Lin"
    }

    fn score_column_name(&self) -> &'static str {
        "Rating"
    }

    async fn initialize_entry(&self, pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            "INSERT INTO weng_lin_ratings (leaderboard_entry_id) \
             VALUES ($1) \
             ON CONFLICT (leaderboard_entry_id) DO NOTHING",
            leaderboard_entry_id,
        )
        .execute(pool)
        .await
        .wrap_err("Failed to initialize weng_lin_ratings entry")?;

        Ok(())
    }

    async fn process_game_result(
        &self,
        conn: &mut sqlx::PgConnection,
        event: &GameResultEvent,
    ) -> cja::Result<()> {
        // Lock weng_lin_ratings rows and read current mu/sigma
        let entry_ids: Vec<Uuid> = event
            .results
            .iter()
            .map(|r| r.leaderboard_entry_id)
            .collect();

        // Fetch existing weng_lin_ratings rows with FOR UPDATE
        let wl_rows = sqlx::query!(
            "SELECT leaderboard_entry_id, mu, sigma FROM weng_lin_ratings \
             WHERE leaderboard_entry_id = ANY($1) FOR UPDATE",
            &entry_ids,
        )
        .fetch_all(&mut *conn)
        .await
        .wrap_err("Failed to lock weng_lin_ratings rows")?;

        let wl_map: std::collections::HashMap<Uuid, (f64, f64)> = wl_rows
            .into_iter()
            .map(|r| (r.leaderboard_entry_id, (r.mu, r.sigma)))
            .collect();

        // Also fetch leaderboard_entries for fallback mu/sigma and for write-through
        let le_rows = sqlx::query_as!(
            LeaderboardEntry,
            "SELECT leaderboard_entry_id, leaderboard_id, battlesnake_id, \
             mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes, \
             disabled_at, created_at, updated_at \
             FROM leaderboard_entries WHERE leaderboard_entry_id = ANY($1) FOR UPDATE",
            &entry_ids,
        )
        .fetch_all(&mut *conn)
        .await
        .wrap_err("Failed to fetch leaderboard entries for weng-lin")?;

        let le_map: std::collections::HashMap<Uuid, LeaderboardEntry> = le_rows
            .into_iter()
            .map(|e| (e.leaderboard_entry_id, e))
            .collect();

        // Build entries_with_placements using weng_lin_ratings mu/sigma (fallback to leaderboard_entries)
        let mut entries_with_placements: Vec<(LeaderboardEntry, i32)> = Vec::new();
        for result in &event.results {
            if let Some(le) = le_map.get(&result.leaderboard_entry_id) {
                let mut entry = le.clone();
                // Override mu/sigma from weng_lin_ratings if available
                if let Some((mu, sigma)) = wl_map.get(&result.leaderboard_entry_id) {
                    entry.mu = *mu;
                    entry.sigma = *sigma;
                }
                entries_with_placements.push((entry, result.placement));
            }
        }

        if entries_with_placements.len() < 2 {
            return Ok(());
        }

        let updates = calculate_rating_updates(&entries_with_placements);

        for update in &updates {
            // Upsert weng_lin_ratings
            sqlx::query!(
                "INSERT INTO weng_lin_ratings (leaderboard_entry_id, mu, sigma, display_score) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (leaderboard_entry_id) DO UPDATE SET \
                   mu = $2, sigma = $3, display_score = $4, updated_at = NOW()",
                update.leaderboard_entry_id,
                update.new_mu,
                update.new_sigma,
                update.new_display_score,
            )
            .execute(&mut *conn)
            .await
            .wrap_err("Failed to update weng_lin_ratings")?;

            // Write-through to leaderboard_entries (backward compatibility)
            leaderboard::update_rating(
                &mut *conn,
                update.leaderboard_entry_id,
                update.new_mu,
                update.new_sigma,
                update.new_display_score,
                update.is_first_place,
            )
            .await
            .wrap_err("Failed to write-through to leaderboard_entries")?;

            // Record audit trail
            leaderboard::create_game_result(
                &mut *conn,
                leaderboard::CreateGameResult {
                    leaderboard_game_id: event.leaderboard_game_id,
                    leaderboard_entry_id: update.leaderboard_entry_id,
                    placement: update.placement,
                    mu_before: update.old_mu,
                    mu_after: update.new_mu,
                    sigma_before: update.old_sigma,
                    sigma_after: update.new_sigma,
                    display_score_change: update.display_score_change,
                },
            )
            .await
            .wrap_err("Failed to create game result record")?;
        }

        Ok(())
    }

    async fn get_scores(
        &self,
        pool: &PgPool,
        leaderboard_id: Uuid,
    ) -> cja::Result<Vec<EntryScore>> {
        let rows = sqlx::query!(
            "SELECT wlr.leaderboard_entry_id, wlr.display_score, wlr.mu, wlr.sigma \
             FROM weng_lin_ratings wlr \
             JOIN leaderboard_entries le ON wlr.leaderboard_entry_id = le.leaderboard_entry_id \
             WHERE le.leaderboard_id = $1 \
               AND le.disabled_at IS NULL \
               AND le.games_played >= $2 \
             ORDER BY wlr.display_score DESC",
            leaderboard_id,
            leaderboard::MIN_GAMES_FOR_RANKING,
        )
        .fetch_all(pool)
        .await
        .wrap_err("Failed to fetch weng-lin scores")?;

        Ok(rows
            .into_iter()
            .map(|r| EntryScore {
                leaderboard_entry_id: r.leaderboard_entry_id,
                score: r.display_score,
                details: vec![
                    ("mu".to_string(), format!("{:.2}", r.mu)),
                    ("sigma".to_string(), format!("{:.2}", r.sigma)),
                ],
            })
            .collect())
    }

    async fn get_entry_score(
        &self,
        pool: &PgPool,
        leaderboard_entry_id: Uuid,
    ) -> cja::Result<Option<EntryScore>> {
        let row = sqlx::query!(
            "SELECT leaderboard_entry_id, display_score, mu, sigma \
             FROM weng_lin_ratings \
             WHERE leaderboard_entry_id = $1",
            leaderboard_entry_id,
        )
        .fetch_optional(pool)
        .await
        .wrap_err("Failed to fetch weng-lin entry score")?;

        Ok(row.map(|r| EntryScore {
            leaderboard_entry_id: r.leaderboard_entry_id,
            score: r.display_score,
            details: vec![
                ("mu".to_string(), format!("{:.2}", r.mu)),
                ("sigma".to_string(), format!("{:.2}", r.sigma)),
            ],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::ScoringAlgorithm;
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
    fn test_weng_lin_key() {
        let algo = WengLinScoring;
        assert_eq!(algo.key(), "weng_lin");
    }

    #[test]
    fn test_weng_lin_display_name() {
        let algo = WengLinScoring;
        assert_eq!(algo.display_name(), "Weng-Lin");
    }

    #[test]
    fn test_weng_lin_score_column_name() {
        let algo = WengLinScoring;
        assert_eq!(algo.score_column_name(), "Rating");
    }

    #[test]
    fn test_weng_lin_display_score_formula() {
        let mu = 30.0_f64;
        let sigma = 6.0_f64;
        let expected_display_score = mu - 3.0 * sigma;

        assert!(
            (expected_display_score - 12.0).abs() < f64::EPSILON,
            "Display score should be mu - 3*sigma = 12.0, got {}",
            expected_display_score
        );
    }

    #[test]
    fn test_weng_lin_default_rating_values() {
        let default_mu = 25.0_f64;
        let default_sigma = 8.333_f64;
        let default_display_score = default_mu - 3.0 * default_sigma;

        assert!(
            default_display_score < 0.01,
            "Default display score should be near 0, got {}",
            default_display_score
        );
        assert!(
            default_display_score > -0.01,
            "Default display score should be near 0, got {}",
            default_display_score
        );
    }

    #[test]
    fn test_weng_lin_entry_score_details_include_mu_and_sigma() {
        use crate::scoring::EntryScore;

        let score = EntryScore {
            leaderboard_entry_id: Uuid::new_v4(),
            score: 12.0,
            details: vec![
                ("mu".to_string(), "30.00".to_string()),
                ("sigma".to_string(), "6.00".to_string()),
            ],
        };

        assert_eq!(
            score.details.len(),
            2,
            "WengLin should provide mu and sigma details"
        );
        assert_eq!(score.details[0].0, "mu");
        assert_eq!(score.details[1].0, "sigma");
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
            (make_entry(25.0, 8.333), 1),
            (make_entry(25.0, 8.333), 2),
            (make_entry(25.0, 8.333), 3),
            (make_entry(25.0, 8.333), 4),
        ];

        let updates = calculate_rating_updates(&entries);

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
            (make_entry(25.0, 8.333), 4),
        ];

        let updates = calculate_rating_updates(&entries);

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
        let strong = make_entry(35.0, 5.0);
        let weak = make_entry(15.0, 5.0);

        let expected_win = vec![(strong.clone(), 1), (weak.clone(), 2)];
        let updates = calculate_rating_updates(&expected_win);
        let strong_gain = updates[0].new_mu - updates[0].old_mu;

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
