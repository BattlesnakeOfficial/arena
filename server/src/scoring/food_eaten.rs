use std::collections::HashMap;

use async_trait::async_trait;
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use super::{EntryScore, GameResultEvent, ScoringAlgorithm};

pub struct FoodEatenScoring;

async fn count_food_eaten(
    conn: &mut sqlx::PgConnection,
    game_id: Uuid,
) -> cja::Result<HashMap<String, i32>> {
    let first_turn = sqlx::query_scalar!(
        r#"SELECT frame_data FROM turns WHERE game_id = $1 AND turn_number = 0"#,
        game_id
    )
    .fetch_optional(&mut *conn)
    .await
    .wrap_err("Failed to fetch first turn")?
    .flatten();

    let last_turn = sqlx::query_scalar!(
        r#"SELECT frame_data FROM turns
         WHERE game_id = $1
         ORDER BY turn_number DESC
         LIMIT 1"#,
        game_id
    )
    .fetch_optional(&mut *conn)
    .await
    .wrap_err("Failed to fetch last turn")?
    .flatten();

    let mut food_map: HashMap<String, i32> = HashMap::new();

    let (Some(first_frame), Some(last_frame)) = (first_turn, last_turn) else {
        return Ok(food_map);
    };

    let mut initial_lengths: HashMap<String, i32> = HashMap::new();
    if let Some(snakes) = first_frame.get("Snakes").and_then(|s| s.as_array()) {
        for snake in snakes {
            if let (Some(id), Some(body)) = (
                snake.get("ID").and_then(|v| v.as_str()),
                snake.get("Body").and_then(|v| v.as_array()),
            ) {
                initial_lengths.insert(id.to_string(), body.len() as i32);
            }
        }
    }

    if let Some(snakes) = last_frame.get("Snakes").and_then(|s| s.as_array()) {
        for snake in snakes {
            if let (Some(id), Some(body)) = (
                snake.get("ID").and_then(|v| v.as_str()),
                snake.get("Body").and_then(|v| v.as_array()),
            ) {
                let final_len = body.len() as i32;
                let initial_len = initial_lengths.get(id).copied().unwrap_or(3);
                let eaten = (final_len - initial_len).max(0);
                food_map.insert(id.to_string(), eaten);
            }
        }
    }

    Ok(food_map)
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
                .get(&result.game_battlesnake_id.to_string())
                .copied()
                .unwrap_or(0);

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
    fn test_count_food_from_frame_data() {
        let first_frame = serde_json::json!({
            "Turn": 0,
            "Snakes": [
                {"ID": "snake-aaa", "Body": [{"X":0,"Y":0}, {"X":0,"Y":1}, {"X":0,"Y":2}], "Health": 100},
                {"ID": "snake-bbb", "Body": [{"X":5,"Y":5}, {"X":5,"Y":6}, {"X":5,"Y":7}], "Health": 100},
            ],
            "Food": [], "Hazards": []
        });
        let last_frame = serde_json::json!({
            "Turn": 50,
            "Snakes": [
                {"ID": "snake-aaa", "Body": [{"X":3,"Y":3}, {"X":3,"Y":4}, {"X":3,"Y":5}, {"X":3,"Y":6}, {"X":3,"Y":7}], "Health": 80},
                {"ID": "snake-bbb", "Body": [{"X":8,"Y":8}, {"X":8,"Y":9}, {"X":8,"Y":10}], "Health": 0},
            ],
            "Food": [], "Hazards": []
        });

        let mut initial_lengths: HashMap<String, i32> = HashMap::new();
        if let Some(snakes) = first_frame.get("Snakes").and_then(|s| s.as_array()) {
            for snake in snakes {
                if let (Some(id), Some(body)) = (
                    snake.get("ID").and_then(|v| v.as_str()),
                    snake.get("Body").and_then(|v| v.as_array()),
                ) {
                    initial_lengths.insert(id.to_string(), body.len() as i32);
                }
            }
        }
        let mut food_map: HashMap<String, i32> = HashMap::new();
        if let Some(snakes) = last_frame.get("Snakes").and_then(|s| s.as_array()) {
            for snake in snakes {
                if let (Some(id), Some(body)) = (
                    snake.get("ID").and_then(|v| v.as_str()),
                    snake.get("Body").and_then(|v| v.as_array()),
                ) {
                    let final_len = body.len() as i32;
                    let initial_len = initial_lengths.get(id).copied().unwrap_or(3);
                    food_map.insert(id.to_string(), (final_len - initial_len).max(0));
                }
            }
        }

        assert_eq!(food_map.get("snake-aaa").copied().unwrap_or(0), 2);
        assert_eq!(food_map.get("snake-bbb").copied().unwrap_or(0), 0);
    }

    #[test]
    fn test_negative_length_delta_clamped() {
        let final_len: i32 = 3;
        let initial_len: i32 = 5;
        let eaten = (final_len - initial_len).max(0);
        assert_eq!(eaten, 0);
    }

    #[test]
    fn test_missing_initial_length_defaults_to_3() {
        let initial_lengths: HashMap<String, i32> = HashMap::new();
        let initial_len = initial_lengths.get("unknown-snake").copied().unwrap_or(3);
        assert_eq!(initial_len, 3);
    }
}
