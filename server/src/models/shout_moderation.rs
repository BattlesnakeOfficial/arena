//! Persistence for post-game shout screening (DEV-1297).
//!
//! [`shout_screenings`](ShoutScreening) is the one-row-per-game idempotency
//! marker plus metrics record: written LAST on every terminal path
//! (screened / no_shouts / jev_error), which is what makes the screening
//! job fail-open without retry storms. [`DistinctShout`] rows are the
//! distinct per-snake shout strings extracted from persisted frames in
//! SQL, and the `suppressed_shouts` table is the serve-time suppression
//! set consulted by every frame-serving path.

use color_eyre::eyre::Context as _;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// The one-row-per-game screening marker. Presence = this game has been
/// through shout screening (whatever the outcome).
#[derive(Debug, sqlx::FromRow)]
pub struct ShoutScreening {
    pub game_id: Uuid,
    /// `screened` | `no_shouts` | `jev_error`
    pub outcome: String,
    pub distinct_shout_count: i32,
    pub judged_count: i32,
    pub suppressed_count: i32,
    pub over_cap_count: i32,
    pub model: Option<String>,
    pub latency_ms: Option<f64>,
    pub input_tokens: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Idempotency check: has this game been through shout screening?
pub async fn get_shout_screening(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<ShoutScreening>> {
    sqlx::query_as!(
        ShoutScreening,
        r#"
        SELECT game_id, outcome, distinct_shout_count, judged_count,
               suppressed_count, over_cap_count, model,
               latency_ms, input_tokens, created_at
        FROM shout_screenings
        WHERE game_id = $1
        "#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch shout screening")
}

/// Insert the one-row-per-game marker. Written on EVERY terminal path
/// (screened / no_shouts / jev_error). `ON CONFLICT (game_id) DO NOTHING` —
/// a concurrent duplicate job that loses the race must no-op silently
/// (first row wins; callers never need to know).
#[allow(clippy::too_many_arguments)]
pub async fn insert_shout_screening(
    pool: &PgPool,
    game_id: Uuid,
    outcome: &str,
    distinct: i32,
    judged: i32,
    suppressed: i32,
    over_cap: i32,
    model: Option<&str>,
    latency_ms: Option<f64>,
    input_tokens: Option<i64>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO shout_screenings (
            game_id, outcome, distinct_shout_count, judged_count,
            suppressed_count, over_cap_count, model, latency_ms, input_tokens
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (game_id) DO NOTHING
        "#,
        game_id,
        outcome,
        distinct,
        judged,
        suppressed,
        over_cap,
        model,
        latency_ms,
        input_tokens,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to insert shout screening")?;
    Ok(())
}

/// One distinct (snake, text) pair from a game's persisted frames.
/// `frequency` = number of turns the pair appears on. `snake_name` is MIN
/// over turns — a rename mid-game collapses to one row (the GROUP BY is on
/// id+text, never name).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DistinctShout {
    pub snake_id: String,
    pub snake_name: String,
    pub text: String,
    pub frequency: i64,
}

/// Distinct (snake_id, snake_name, shout, frequency) from a game's
/// persisted frames, deterministic order: snake_id ASC, frequency DESC,
/// shout ASC. Extracted in SQL so a 5000-turn game never loads all frames
/// into memory. Frames whose `Snakes` is missing or not an array are
/// tolerated (the `CASE` guard yields an empty array); empty-string
/// shouts are excluded — `FrameSnake.shout` serializes `""` for "no
/// shout".
pub async fn distinct_game_shouts(pool: &PgPool, game_id: Uuid) -> cja::Result<Vec<DistinctShout>> {
    sqlx::query_as!(
        DistinctShout,
        r#"
        SELECT
            s->>'ID'    AS "snake_id!",
            MIN(COALESCE(s->>'Name', '')) AS "snake_name!",
            s->>'Shout' AS "text!",
            COUNT(*)::BIGINT AS "frequency!"
        FROM turns t
        CROSS JOIN LATERAL jsonb_array_elements(
            CASE WHEN jsonb_typeof(t.frame_data->'Snakes') = 'array'
                 THEN t.frame_data->'Snakes' ELSE '[]'::jsonb END
        ) AS s
        WHERE t.game_id = $1
          AND t.frame_data IS NOT NULL
          AND s->>'ID' IS NOT NULL
          AND s->>'Shout' IS NOT NULL
          AND s->>'Shout' <> ''
        GROUP BY s->>'ID', s->>'Shout'
        ORDER BY s->>'ID' ASC, COUNT(*) DESC, s->>'Shout' ASC
        "#,
        game_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to extract distinct game shouts")
}

/// sha256 hex of a shout text — the dedup key for suppressions.
pub(crate) fn shout_text_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Record one judged suppression. Returns `true` when this call actually
/// inserted (`rows_affected() == 1`); `false` when `ON CONFLICT (game_id,
/// snake_id, text_hash) DO NOTHING` skipped an existing row — callers gate
/// the `moderation_flags` insert on this.
pub async fn insert_suppressed_shout(
    pool: &PgPool,
    game_id: Uuid,
    snake_id: &str,
    text: &str,
    probability: Option<f64>,
    model: Option<&str>,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        INSERT INTO suppressed_shouts (
            game_id, snake_id, text_hash, text, probability, model
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (game_id, snake_id, text_hash) DO NOTHING
        "#,
        game_id,
        snake_id,
        shout_text_hash(text),
        text,
        probability,
        model,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to insert suppressed shout")?;
    Ok(result.rows_affected() == 1)
}

/// Bulk-insert over-cap (unjudged) suppressions: one round-trip, literal
/// NULL probability and model `'over-cap'`. Idempotent via
/// `ON CONFLICT DO NOTHING`. `entries` must be non-empty.
pub async fn insert_over_cap_suppressions(
    pool: &PgPool,
    game_id: Uuid,
    entries: &[DistinctShout],
) -> cja::Result<()> {
    debug_assert!(!entries.is_empty(), "over-cap insert with no entries");
    let snake_ids: Vec<String> = entries.iter().map(|e| e.snake_id.clone()).collect();
    let hashes: Vec<String> = entries.iter().map(|e| shout_text_hash(&e.text)).collect();
    let texts: Vec<String> = entries.iter().map(|e| e.text.clone()).collect();
    sqlx::query!(
        r#"
        INSERT INTO suppressed_shouts (game_id, snake_id, text_hash, text, probability, model)
        SELECT $1, u.sid, u.h, u.t, NULL::DOUBLE PRECISION, 'over-cap'
        FROM UNNEST($2::text[], $3::text[], $4::text[]) AS u(sid, h, t)
        ON CONFLICT (game_id, snake_id, text_hash) DO NOTHING
        "#,
        game_id,
        &snake_ids,
        &hashes,
        &texts,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to bulk-insert over-cap suppressions")?;
    Ok(())
}

/// Serve-time suppression set source: (snake_id, text) pairs to blank.
pub async fn suppressed_shout_pairs(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Vec<(String, String)>> {
    let rows = sqlx::query!(
        r#"
        SELECT snake_id, text
        FROM suppressed_shouts
        WHERE game_id = $1
        "#,
        game_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch suppressed shout pairs")?;
    Ok(rows.into_iter().map(|r| (r.snake_id, r.text)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn fixture_game(pool: &PgPool, status: &str) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    fn frame(turn: i32, snake_id: &str, name: &str, shout: &str) -> serde_json::Value {
        json!({
            "Turn": turn,
            "Snakes": [{"ID": snake_id, "Name": name, "Shout": shout}],
            "Food": [],
            "Hazards": [],
        })
    }

    async fn fixture_turn(
        pool: &PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: serde_json::Value,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn_number)
            .bind(frame_data)
            .execute(pool)
            .await?;
        Ok(())
    }

    fn shout(id: &str, name: &str, text: &str, frequency: i64) -> DistinctShout {
        DistinctShout {
            snake_id: id.to_string(),
            snake_name: name.to_string(),
            text: text.to_string(),
            frequency,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn distinct_game_shouts_collapses_repeats_and_name_changes(
        pool: PgPool,
    ) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let sid = Uuid::new_v4().to_string();

        // Same (id, text) pair on three turns with a mid-game rename, plus
        // one different shout: exactly two distinct rows, frequency 3 and 1.
        fixture_turn(&pool, game_id, 0, frame(0, &sid, "Snek", "get rekt")).await?;
        fixture_turn(&pool, game_id, 1, frame(1, &sid, "Snek2", "get rekt")).await?;
        fixture_turn(&pool, game_id, 2, frame(2, &sid, "Snek2", "hello")).await?;
        fixture_turn(&pool, game_id, 3, frame(3, &sid, "Snek2", "get rekt")).await?;

        let shouts = distinct_game_shouts(&pool, game_id).await?;
        assert_eq!(shouts.len(), 2, "name variation must not split the pair");
        // Ordering: same snake → frequency DESC.
        assert_eq!(shouts[0].text, "get rekt");
        assert_eq!(shouts[0].frequency, 3);
        assert_eq!(shouts[1].text, "hello");
        assert_eq!(shouts[1].frequency, 1);
        // snake_name is MIN over turns.
        assert_eq!(shouts[0].snake_name, "Snek");

        // No leakage between games.
        let other = fixture_game(&pool, "finished").await?;
        assert!(distinct_game_shouts(&pool, other).await?.is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn distinct_game_shouts_excludes_empty_and_tolerates_odd_frames(
        pool: PgPool,
    ) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let sid = Uuid::new_v4().to_string();

        // Empty-string shout (the serialized "no shout") is excluded.
        fixture_turn(&pool, game_id, 0, frame(0, &sid, "Snek", "")).await?;
        // Frame without Snakes at all.
        fixture_turn(
            &pool,
            game_id,
            1,
            json!({"Turn": 1, "Food": [], "Hazards": []}),
        )
        .await?;
        // Snakes not an array.
        fixture_turn(&pool, game_id, 2, json!({"Turn": 2, "Snakes": "nope"})).await?;
        // Snake with no Shout key.
        fixture_turn(
            &pool,
            game_id,
            3,
            json!({"Turn": 3, "Snakes": [{"ID": sid, "Name": "Snek"}]}),
        )
        .await?;
        // NULL frame data.
        sqlx::query("INSERT INTO turns (game_id, turn_number) VALUES ($1, 4)")
            .bind(game_id)
            .execute(&pool)
            .await?;

        assert!(distinct_game_shouts(&pool, game_id).await?.is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn distinct_game_shouts_orders_deterministically(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let a = "11111111-1111-1111-1111-111111111111";
        let b = "22222222-2222-2222-2222-222222222222";

        fixture_turn(&pool, game_id, 0, frame(0, a, "A", "a-low")).await?;
        fixture_turn(&pool, game_id, 1, frame(1, a, "A", "a-high")).await?;
        fixture_turn(&pool, game_id, 2, frame(2, a, "A", "a-high")).await?;
        fixture_turn(&pool, game_id, 3, frame(3, b, "B", "b-only")).await?;

        let shouts = distinct_game_shouts(&pool, game_id).await?;
        // snake_id ASC, then frequency DESC within a snake.
        assert_eq!(shouts[0].snake_id, a);
        assert_eq!(shouts[0].text, "a-high");
        assert_eq!(shouts[1].text, "a-low");
        assert_eq!(shouts[2].snake_id, b);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn insert_suppressed_shout_is_idempotent_and_reports_freshness(
        pool: PgPool,
    ) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;

        assert!(
            insert_suppressed_shout(
                &pool,
                game_id,
                "snake-1",
                "bad text",
                Some(0.97),
                Some("jev-test"),
            )
            .await?
        );
        // Second insert with the same (game, snake, text): no-op, false.
        assert!(
            !insert_suppressed_shout(
                &pool,
                game_id,
                "snake-1",
                "bad text",
                Some(0.99),
                Some("jev-test"),
            )
            .await?
        );

        let pairs = suppressed_shout_pairs(&pool, game_id).await?;
        assert_eq!(pairs, vec![("snake-1".to_string(), "bad text".to_string())]);

        let row = sqlx::query!(
            "SELECT probability, model FROM suppressed_shouts WHERE game_id = $1",
            game_id
        )
        .fetch_one(&pool)
        .await?;
        // First row wins.
        assert!((row.probability.unwrap() - 0.97).abs() < 1e-9);
        assert_eq!(row.model.as_deref(), Some("jev-test"));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn over_cap_bulk_insert_lands_all_rows_idempotently(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;
        let entries = vec![
            shout("snake-1", "A", "t1", 5),
            shout("snake-1", "A", "t2", 4),
            shout("snake-2", "B", "t3", 2),
        ];

        insert_over_cap_suppressions(&pool, game_id, &entries).await?;
        // Re-run: idempotent.
        insert_over_cap_suppressions(&pool, game_id, &entries).await?;

        let rows = sqlx::query!(
            "SELECT snake_id, text, probability, model FROM suppressed_shouts
             WHERE game_id = $1 ORDER BY text",
            game_id
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert!(row.probability.is_none(), "over-cap rows are unjudged");
            assert_eq!(row.model.as_deref(), Some("over-cap"));
        }
        assert_eq!(rows[0].text, "t1");
        assert_eq!(rows[2].text, "t3");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn screening_marker_round_trips_and_first_row_wins(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "finished").await?;

        assert!(get_shout_screening(&pool, game_id).await?.is_none());

        insert_shout_screening(
            &pool,
            game_id,
            "screened",
            3,
            3,
            1,
            0,
            Some("jev-test"),
            Some(123.4),
            Some(777),
        )
        .await?;
        // A second (racing) insert with different values: silent no-op.
        insert_shout_screening(&pool, game_id, "jev_error", 3, 0, 0, 0, None, None, None).await?;

        let marker = get_shout_screening(&pool, game_id)
            .await?
            .expect("marker exists");
        assert_eq!(marker.outcome, "screened");
        assert_eq!(marker.distinct_shout_count, 3);
        assert_eq!(marker.judged_count, 3);
        assert_eq!(marker.suppressed_count, 1);
        assert_eq!(marker.over_cap_count, 0);
        assert_eq!(marker.model.as_deref(), Some("jev-test"));
        assert!((marker.latency_ms.unwrap() - 123.4).abs() < 1e-9);
        assert_eq!(marker.input_tokens, Some(777));
        Ok(())
    }
}
