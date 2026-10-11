use std::collections::HashMap;

use async_trait::async_trait;
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use super::{EntryScore, GameResultEvent, ScoringAlgorithm};
use crate::models::game::GameType;

pub struct FoodEatenScoring;

/// Food eaten per snake in a game, keyed by `game_battlesnake_id`. The game
/// runner counts it from the engine's feed stage and writes it alongside the
/// placement, so this is a real count in every mode -- unlike body growth,
/// which Constrictor applies every turn with no food on the board. Snakes with
/// no count (games finished before it was recorded) are absent.
async fn count_food_eaten(
    conn: &mut sqlx::PgConnection,
    game_id: Uuid,
) -> cja::Result<HashMap<Uuid, i32>> {
    let rows = sqlx::query!(
        r#"SELECT game_battlesnake_id, food_eaten AS "food_eaten!"
         FROM game_battlesnakes
         WHERE game_id = $1 AND food_eaten IS NOT NULL"#,
        game_id
    )
    .fetch_all(&mut *conn)
    .await
    .wrap_err("Failed to fetch food eaten per snake")?;

    Ok(rows
        .into_iter()
        .map(|r| (r.game_battlesnake_id, r.food_eaten))
        .collect())
}

#[async_trait]
impl ScoringAlgorithm for FoodEatenScoring {
    fn key(&self) -> &'static str {
        "food_eaten"
    }

    fn display_name(&self) -> &'static str {
        "Food Eaten"
    }

    fn score_column_name(&self) -> &'static str {
        "Food"
    }

    fn applies_to(&self, game_type: &GameType) -> bool {
        game_type.has_food()
    }

    async fn initialize_entry(&self, pool: &PgPool, leaderboard_entry_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            "INSERT INTO food_eaten_stats (leaderboard_entry_id) \
             VALUES ($1) \
             ON CONFLICT (leaderboard_entry_id) DO NOTHING",
            leaderboard_entry_id,
        )
        .execute(pool)
        .await
        .wrap_err("Failed to initialize food_eaten_stats entry")?;

        Ok(())
    }

    async fn process_game_result(
        &self,
        conn: &mut sqlx::PgConnection,
        event: &GameResultEvent,
    ) -> cja::Result<()> {
        let food_eaten_map = count_food_eaten(conn, event.game_id).await?;

        for result in &event.results {
            let food_eaten = food_eaten_map
                .get(&result.game_battlesnake_id)
                .copied()
                .unwrap_or(0);

            // Skip cumulative-score and audit-trail writes when this snake ate nothing.
            // The audit row's `food_eaten` column defaults to 0 from the schema, and the
            // cumulative `food_score` increment would be a no-op. Avoids touching
            // `updated_at` on entries that didn't actually change.
            if food_eaten == 0 {
                continue;
            }

            let rows_affected = sqlx::query!(
                "UPDATE food_eaten_stats SET \
                    food_score = food_score + $2, \
                    updated_at = NOW() \
                 WHERE leaderboard_entry_id = $1",
                result.leaderboard_entry_id,
                food_eaten as i64,
            )
            .execute(&mut *conn)
            .await
            .wrap_err("Failed to update food_eaten_stats")?
            .rows_affected();

            if rows_affected == 0 {
                sqlx::query!(
                    "INSERT INTO food_eaten_stats (leaderboard_entry_id) \
                     VALUES ($1) \
                     ON CONFLICT (leaderboard_entry_id) DO NOTHING",
                    result.leaderboard_entry_id,
                )
                .execute(&mut *conn)
                .await
                .wrap_err("Failed to lazy-insert food_eaten_stats")?;

                sqlx::query!(
                    "UPDATE food_eaten_stats SET \
                        food_score = food_score + $2, \
                        updated_at = NOW() \
                     WHERE leaderboard_entry_id = $1",
                    result.leaderboard_entry_id,
                    food_eaten as i64,
                )
                .execute(&mut *conn)
                .await
                .wrap_err("Failed to retry update food_eaten_stats")?;
            }

            // Update audit trail row created by WengLinScoring; no-op if row doesn't exist
            sqlx::query!(
                "UPDATE leaderboard_game_results \
                 SET food_eaten = $3 \
                 WHERE leaderboard_game_id = $1 AND leaderboard_entry_id = $2",
                event.leaderboard_game_id,
                result.leaderboard_entry_id,
                food_eaten,
            )
            .execute(&mut *conn)
            .await
            .wrap_err("Failed to update food_eaten on game result")?;
        }

        Ok(())
    }

    async fn get_scores(&self, pool: &PgPool, entry_ids: &[Uuid]) -> cja::Result<Vec<EntryScore>> {
        let rows = sqlx::query!(
            "SELECT leaderboard_entry_id, food_score \
             FROM food_eaten_stats \
             WHERE leaderboard_entry_id = ANY($1)",
            entry_ids as &[Uuid],
        )
        .fetch_all(pool)
        .await
        .wrap_err("Failed to fetch food-eaten scores")?;

        Ok(rows
            .into_iter()
            .map(|r| EntryScore {
                leaderboard_entry_id: r.leaderboard_entry_id,
                score: r.food_score as f64,
                details: vec![("food_score".to_string(), r.food_score.to_string())],
            })
            .collect())
    }

    async fn get_entry_score(
        &self,
        pool: &PgPool,
        leaderboard_entry_id: Uuid,
    ) -> cja::Result<Option<EntryScore>> {
        let row = sqlx::query!(
            "SELECT leaderboard_entry_id, food_score \
             FROM food_eaten_stats \
             WHERE leaderboard_entry_id = $1",
            leaderboard_entry_id,
        )
        .fetch_optional(pool)
        .await
        .wrap_err("Failed to fetch food-eaten entry score")?;

        Ok(row.map(|r| EntryScore {
            leaderboard_entry_id: r.leaderboard_entry_id,
            score: r.food_score as f64,
            details: vec![("food_score".to_string(), r.food_score.to_string())],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::ScoringAlgorithm;

    #[test]
    fn test_food_eaten_key() {
        assert_eq!(FoodEatenScoring.key(), "food_eaten");
    }

    #[test]
    fn test_food_eaten_display_name() {
        assert_eq!(FoodEatenScoring.display_name(), "Food Eaten");
    }

    #[test]
    fn test_food_eaten_score_column_name() {
        assert_eq!(FoodEatenScoring.score_column_name(), "Food");
    }

    #[test]
    fn test_food_eaten_applies_only_to_modes_with_food() {
        for game_type in [
            GameType::Standard,
            GameType::Royale,
            GameType::SnailMode,
            GameType::Solo,
        ] {
            assert!(FoodEatenScoring.applies_to(&game_type), "{game_type:?}");
        }
        assert!(!FoodEatenScoring.applies_to(&GameType::Constrictor));
    }
}
