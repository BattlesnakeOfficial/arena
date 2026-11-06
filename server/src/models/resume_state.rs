//! SQL for the live resume state (`game_resume_states` /
//! `game_resume_snakes`). The typed contract lives in
//! [`crate::engine::resume`]; these are the reads and writes.

use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::engine::resume::{ResumeSnakeRecord, ResumeStateData};

/// Upsert the state row and every roster snake row, on the caller's
/// transaction — called from `persist_turn` so the resume state commits (or
/// rolls back) atomically with the turn and its moves. Exactly two
/// statements.
pub async fn upsert_resume_state(
    tx: &mut sqlx::PgConnection,
    game_id: Uuid,
    state: &ResumeStateData,
) -> cja::Result<()> {
    sqlx::query!(
        r#"INSERT INTO game_resume_states (game_id, turn_number, food, hazards, updated_at)
           VALUES ($1, $2, $3, $4, NOW())
           ON CONFLICT (game_id) DO UPDATE SET
               turn_number = EXCLUDED.turn_number,
               food = EXCLUDED.food,
               hazards = EXCLUDED.hazards,
               updated_at = NOW()"#,
        game_id,
        state.turn_number,
        state.food,
        state.hazards,
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to upsert game resume state")?;

    let game_battlesnake_ids: Vec<Uuid> =
        state.snakes.iter().map(|s| s.game_battlesnake_id).collect();
    let healths: Vec<i32> = state.snakes.iter().map(|s| s.health).collect();
    let eliminated_causes: Vec<String> = state
        .snakes
        .iter()
        .map(|s| s.eliminated_cause.clone())
        .collect();
    let eliminated_bys: Vec<String> = state
        .snakes
        .iter()
        .map(|s| s.eliminated_by.clone())
        .collect();
    let eliminated_on_turns: Vec<i32> = state.snakes.iter().map(|s| s.eliminated_on_turn).collect();
    let last_moves: Vec<Option<String>> =
        state.snakes.iter().map(|s| s.last_move.clone()).collect();
    let context_presents: Vec<bool> = state.snakes.iter().map(|s| s.context_present).collect();
    // SnakeContext.latency_ms is i64; the column is INTEGER, narrowed
    // exactly as create_snake_turn narrows snake_turns.latency_ms.
    let latencies: Vec<Option<i32>> = state
        .snakes
        .iter()
        .map(|s| s.latency_ms.map(|ms| ms as i32))
        .collect();
    let shouts: Vec<Option<String>> = state.snakes.iter().map(|s| s.shout.clone()).collect();
    let food_eatens: Vec<i32> = state.snakes.iter().map(|s| s.food_eaten).collect();
    let death_turns: Vec<Option<i32>> = state.snakes.iter().map(|s| s.death_turn).collect();
    let death_causes: Vec<Option<String>> =
        state.snakes.iter().map(|s| s.death_cause.clone()).collect();
    let death_eliminated_bys: Vec<Option<String>> = state
        .snakes
        .iter()
        .map(|s| s.death_eliminated_by.clone())
        .collect();
    let death_orders: Vec<Option<i32>> = state.snakes.iter().map(|s| s.death_order).collect();
    let bodies: Vec<serde_json::Value> = state.snakes.iter().map(|s| s.body.clone()).collect();

    // One batched statement — no per-snake round trips. Runtime-checked
    // (not the query! macro): the nullable array bindings (last_move,
    // latency_ms, shout, the death group) don't fit the macro's `&[T]`
    // inference for `type[]` params; the behavioral tests pin every value
    // this writes.
    sqlx::query(
        r#"INSERT INTO game_resume_snakes (
               game_id, game_battlesnake_id, health, eliminated_cause, eliminated_by,
               eliminated_on_turn, last_move, context_present, latency_ms, shout,
               food_eaten, death_turn, death_cause, death_eliminated_by, death_order, body
           )
           SELECT $1, s.game_battlesnake_id, s.health, s.eliminated_cause, s.eliminated_by,
                  s.eliminated_on_turn, s.last_move, s.context_present, s.latency_ms, s.shout,
                  s.food_eaten, s.death_turn, s.death_cause, s.death_eliminated_by,
                  s.death_order, s.body
           FROM UNNEST($2::uuid[], $3::int[], $4::text[], $5::text[], $6::int[], $7::text[],
                       $8::bool[], $9::int[], $10::text[], $11::int[], $12::int[], $13::text[],
                       $14::text[], $15::int[], $16::jsonb[])
                AS s(game_battlesnake_id, health, eliminated_cause, eliminated_by,
                     eliminated_on_turn, last_move, context_present, latency_ms, shout,
                     food_eaten, death_turn, death_cause, death_eliminated_by, death_order, body)
           ON CONFLICT (game_id, game_battlesnake_id) DO UPDATE SET
               health = EXCLUDED.health,
               eliminated_cause = EXCLUDED.eliminated_cause,
               eliminated_by = EXCLUDED.eliminated_by,
               eliminated_on_turn = EXCLUDED.eliminated_on_turn,
               last_move = EXCLUDED.last_move,
               context_present = EXCLUDED.context_present,
               latency_ms = EXCLUDED.latency_ms,
               shout = EXCLUDED.shout,
               food_eaten = EXCLUDED.food_eaten,
               death_turn = EXCLUDED.death_turn,
               death_cause = EXCLUDED.death_cause,
               death_eliminated_by = EXCLUDED.death_eliminated_by,
               death_order = EXCLUDED.death_order,
               body = EXCLUDED.body"#,
    )
    .bind(game_id)
    .bind(&game_battlesnake_ids)
    .bind(&healths)
    .bind(&eliminated_causes)
    .bind(&eliminated_bys)
    .bind(&eliminated_on_turns)
    .bind(&last_moves)
    .bind(&context_presents)
    .bind(&latencies)
    .bind(&shouts)
    .bind(&food_eatens)
    .bind(&death_turns)
    .bind(&death_causes)
    .bind(&death_eliminated_bys)
    .bind(&death_orders)
    .bind(&bodies)
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to upsert game resume snakes")?;

    Ok(())
}

/// Load the resume state (state row + snake rows), or `None` when the game
/// has no state row. Two SELECTs, once per resume attempt — never per turn.
pub async fn load_resume_state(
    pool: &sqlx::PgPool,
    game_id: Uuid,
) -> cja::Result<Option<ResumeStateData>> {
    let Some(state) = sqlx::query!(
        r#"SELECT turn_number, food, hazards FROM game_resume_states WHERE game_id = $1"#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to load game resume state")?
    else {
        return Ok(None);
    };

    let rows = sqlx::query!(
        r#"SELECT game_battlesnake_id, health, eliminated_cause, eliminated_by,
                  eliminated_on_turn, last_move, context_present, latency_ms, shout,
                  food_eaten, death_turn, death_cause, death_eliminated_by, death_order, body
           FROM game_resume_snakes WHERE game_id = $1"#,
        game_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to load game resume snakes")?;

    let snakes: Vec<ResumeSnakeRecord> = rows
        .into_iter()
        .map(|row| ResumeSnakeRecord {
            game_battlesnake_id: row.game_battlesnake_id,
            health: row.health,
            eliminated_cause: row.eliminated_cause,
            eliminated_by: row.eliminated_by,
            eliminated_on_turn: row.eliminated_on_turn,
            last_move: row.last_move,
            context_present: row.context_present,
            // INTEGER widened back to the i64 SnakeContext carries.
            latency_ms: row.latency_ms.map(i64::from),
            shout: row.shout,
            food_eaten: row.food_eaten,
            death_turn: row.death_turn,
            death_cause: row.death_cause,
            death_eliminated_by: row.death_eliminated_by,
            death_order: row.death_order,
            body: row.body,
        })
        .collect();

    Ok(Some(ResumeStateData {
        turn_number: state.turn_number,
        food: state.food,
        hazards: state.hazards,
        snakes,
    }))
}

/// Delete the resume state (cascades to snake rows). Takes an Executor so it
/// runs on a pool OR inside a transaction (finish, reset).
pub async fn delete_resume_state<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    game_id: Uuid,
) -> cja::Result<()> {
    sqlx::query!("DELETE FROM game_resume_states WHERE game_id = $1", game_id)
        .execute(executor)
        .await
        .wrap_err("Failed to delete game resume state")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    async fn load_returns_none_without_a_state_row(pool: sqlx::PgPool) -> cja::Result<()> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'running') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(load_resume_state(&pool, game_id).await?, None);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn load_round_trips_values_and_delete_cascades(pool: sqlx::PgPool) -> cja::Result<()> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'running') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await?;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (7312, 'resume-state-owner', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'resume-state-snake', 'http://example.com')
             RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let game_battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id)
             VALUES ($1, $2) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(battlesnake_id)
        .fetch_one(&pool)
        .await?;

        let state = ResumeStateData {
            turn_number: 3,
            food: serde_json::json!([{"x": 1, "y": 2}]),
            hazards: serde_json::json!([{"x": 0, "y": 14}]),
            snakes: vec![ResumeSnakeRecord {
                game_battlesnake_id,
                health: 88,
                eliminated_cause: "not_eliminated".to_owned(),
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
                last_move: Some("down".to_owned()),
                context_present: true,
                latency_ms: Some(300_000),
                shout: Some("hi".to_owned()),
                food_eaten: 1,
                death_turn: None,
                death_cause: None,
                death_eliminated_by: None,
                death_order: None,
                body: serde_json::json!([{"x": 1, "y": 1}]),
            }],
        };
        let mut tx = pool.begin().await?;
        upsert_resume_state(&mut tx, game_id, &state).await?;
        tx.commit().await?;

        let loaded = load_resume_state(&pool, game_id)
            .await?
            .expect("state row present");
        assert_eq!(loaded, state, "load must return exactly what was upserted");

        delete_resume_state(&pool, game_id).await?;
        assert_eq!(load_resume_state(&pool, game_id).await?, None);
        let snake_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_snakes WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(snake_rows, 0, "cascade removes snake rows");
        Ok(())
    }

    /// A game with a two-snake roster holding resume state — shared fixture
    /// for the FK-cascade tests.
    async fn game_with_two_resume_snakes(pool: &sqlx::PgPool) -> cja::Result<(Uuid, [Uuid; 2])> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'running') RETURNING game_id",
        )
        .fetch_one(pool)
        .await?;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (7313, 'fk-cascade-owner', 'test-token') RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let mut game_battlesnake_ids = [Uuid::new_v4(); 2];
        for (i, id) in game_battlesnake_ids.iter_mut().enumerate() {
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, 'http://example.com') RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(format!("fk-cascade-snake-{i}"))
            .fetch_one(pool)
            .await?;
            *id = sqlx::query_scalar(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id)
                 VALUES ($1, $2) RETURNING game_battlesnake_id",
            )
            .bind(game_id)
            .bind(battlesnake_id)
            .fetch_one(pool)
            .await?;
        }

        let state = ResumeStateData {
            turn_number: 4,
            food: serde_json::json!([]),
            hazards: serde_json::json!([]),
            snakes: game_battlesnake_ids
                .iter()
                .map(|&id| ResumeSnakeRecord {
                    game_battlesnake_id: id,
                    health: 90,
                    eliminated_cause: "not_eliminated".to_owned(),
                    eliminated_by: String::new(),
                    eliminated_on_turn: 0,
                    last_move: Some("up".to_owned()),
                    context_present: true,
                    latency_ms: Some(120),
                    shout: None,
                    food_eaten: 0,
                    death_turn: None,
                    death_cause: None,
                    death_eliminated_by: None,
                    death_order: None,
                    body: serde_json::json!([{"x": 1, "y": 1}]),
                })
                .collect(),
        };
        let mut tx = pool.begin().await?;
        upsert_resume_state(&mut tx, game_id, &state).await?;
        tx.commit().await?;
        Ok((game_id, game_battlesnake_ids))
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleting_a_roster_row_drops_only_its_resume_snake_row(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (game_id, ids) = game_with_two_resume_snakes(&pool).await?;

        sqlx::query("DELETE FROM game_battlesnakes WHERE game_battlesnake_id = $1")
            .bind(ids[0])
            .execute(&pool)
            .await?;

        let remaining: Vec<Uuid> = sqlx::query_scalar(
            "SELECT game_battlesnake_id FROM game_resume_snakes WHERE game_id = $1",
        )
        .bind(game_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(remaining, vec![ids[1]]);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleting_a_game_removes_all_resume_state(pool: sqlx::PgPool) -> cja::Result<()> {
        let (game_id, _ids) = game_with_two_resume_snakes(&pool).await?;

        sqlx::query("DELETE FROM games WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await?;

        let states: i64 = sqlx::query_scalar("SELECT count(*) FROM game_resume_states")
            .fetch_one(&pool)
            .await?;
        let snakes: i64 = sqlx::query_scalar("SELECT count(*) FROM game_resume_snakes")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            states, 0,
            "deleting a game must remove its resume state row"
        );
        assert_eq!(
            snakes, 0,
            "deleting a game must remove its resume snake rows"
        );
        Ok(())
    }
}
