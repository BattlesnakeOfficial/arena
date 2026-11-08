use super::*;
use std::time::Duration;

// The shared capture and its single global subscriber live in
// `crate::test_capture` so the auth span test can share them.
use crate::test_capture::Capture;

async fn game(pool: &PgPool) -> Uuid {
    sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
        .fetch_one(pool).await.unwrap()
}

async fn single_connection(database: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(database.connect_options().as_ref().clone())
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn pool_wait_precedes_insert_and_keeps_creation_identity(database: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&database).await;
    let pool = single_connection(&database).await;
    let held = pool.acquire().await.unwrap();
    let frame = serde_json::json!({"test_frame": "retained in storage only"});
    let work = create_turn(&pool, game_id, 7, Some(frame.clone()));
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => panic!("must wait for held connection: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(30)) => {}
    }
    assert_eq!(
        capture.states(game_id),
        [("acquire_frame_connection".into(), "started".into())]
    );
    drop(held);
    let turn = tokio::time::timeout(Duration::from_secs(5), &mut work)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(turn.frame_data, Some(frame));
    let states = capture.states(game_id);
    assert_eq!(
        states,
        [
            ("acquire_frame_connection".into(), "started".into()),
            ("acquire_frame_connection".into(), "completed".into()),
            ("insert_frame".into(), "started".into()),
            ("insert_frame".into(), "completed".into()),
        ]
    );
    let spans = capture.records(game_id, true);
    assert_eq!(spans.len(), 2);
    assert!(
        spans
            .iter()
            .all(|fields| fields["turn"] == "7" && !fields.contains_key("frame_data"))
    );
    let events = capture.records(game_id, false);
    assert!(events.iter().all(|fields| fields["state"] == "completed"));
    assert!(events[0]["duration_ms"].parse::<u64>().unwrap() >= 20);
}

#[sqlx::test(migrations = "../migrations")]
async fn failed_frame_insert_never_notifies_and_returns_connection(database: PgPool) {
    let capture = Capture::shared();
    let pool = single_connection(&database).await;
    let game_id = Uuid::new_v4(); // No matching game: the real INSERT fails its FK.
    let error = create_turn(&pool, game_id, 3, None).await.unwrap_err();
    assert!(format!("{error:#}").contains("Failed to create turn"));
    assert_eq!(
        capture.states(game_id),
        [
            ("acquire_frame_connection".into(), "started".into()),
            ("acquire_frame_connection".into(), "completed".into()),
            ("insert_frame".into(), "started".into()),
            ("insert_frame".into(), "failed".into()),
        ]
    );
    let _connection = tokio::time::timeout(Duration::from_secs(1), pool.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn cancelling_pool_wait_emits_no_insert_or_completion(database: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&database).await;
    let pool = single_connection(&database).await;
    let _held = pool.acquire().await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            create_turn(&pool, game_id, 4, None)
        )
        .await
        .is_err()
    );
    assert_eq!(
        capture.states(game_id),
        [
            ("acquire_frame_connection".into(), "started".into()),
            ("acquire_frame_connection".into(), "cancelled".into()),
        ]
    );
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM turns WHERE game_id=$1", game_id)
            .fetch_one(&database)
            .await
            .unwrap(),
        Some(0)
    );
}

/// A minimal resume snapshot for persist_turn fixtures: a state row keyed to
/// the game, no snake rows (persist_turn's snake upserts are exercised by
/// the atomicity test with a full capture).
fn bare_resume(turn_number: i32) -> crate::engine::resume::ResumeStateData {
    crate::engine::resume::ResumeStateData {
        turn_number,
        food: serde_json::json!([]),
        hazards: serde_json::json!([]),
        snakes: vec![],
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn snake_insert_failure_is_identified_inside_its_turn(database: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&database).await;
    let pool = single_connection(&database).await;
    let error = persist_turn(
        &pool,
        game_id,
        8,
        serde_json::json!({"frame": 8}),
        &bare_resume(8),
        &[SnakeMoveRecord {
            game_battlesnake_id: Uuid::new_v4(), // No matching roster row: FK violation.
            direction: "up".to_owned(),
            latency_ms: Some(42),
            timed_out: false,
            errored: false,
        }],
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("Failed to create snake turn"));
    assert_eq!(
        capture.states(game_id),
        [
            ("acquire_frame_connection".into(), "started".into()),
            ("acquire_frame_connection".into(), "completed".into()),
            ("insert_frame".into(), "started".into()),
            ("insert_frame".into(), "completed".into()),
            ("insert_snake".into(), "started".into()),
            ("insert_snake".into(), "failed".into()),
        ]
    );
    assert!(
        capture
            .records(game_id, true)
            .iter()
            .all(|fields| fields["turn"] == "8")
    );
    // The transaction rolls the frame insert back with the failed move —
    // the new property persist_turn adds.
    assert_eq!(
        sqlx::query_scalar!("SELECT count(*) FROM turns WHERE game_id=$1", game_id)
            .fetch_one(&database)
            .await
            .unwrap(),
        Some(0)
    );
    // The resume upserts roll back with everything else.
    assert_eq!(
        sqlx::query_scalar!(
            "SELECT count(*) FROM game_resume_states WHERE game_id=$1",
            game_id
        )
        .fetch_one(&database)
        .await
        .unwrap(),
        Some(0)
    );
    let _connection = tokio::time::timeout(Duration::from_secs(1), pool.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn successful_snake_insert_keeps_values_and_emits_completion(pool: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&pool).await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login) VALUES (7310, 'storage-test') RETURNING user_id")
        .fetch_one(&pool).await.unwrap();
    let snake_id = sqlx::query_scalar!("INSERT INTO battlesnakes (user_id, name, url, visibility) VALUES ($1, 'Storage Snake', 'http://localhost:8000', 'public') RETURNING battlesnake_id", user_id)
        .fetch_one(&pool).await.unwrap();
    let game_snake_id = sqlx::query_scalar!("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2) RETURNING game_battlesnake_id", game_id, snake_id)
        .fetch_one(&pool).await.unwrap();
    persist_turn(
        &pool,
        game_id,
        2,
        serde_json::json!({"frame": 2}),
        &bare_resume(2),
        &[SnakeMoveRecord {
            game_battlesnake_id: game_snake_id,
            direction: "left".to_owned(),
            latency_ms: Some(42),
            timed_out: true,
            errored: true,
        }],
    )
    .await
    .unwrap();
    let turn_id: Uuid = sqlx::query_scalar!(
        "SELECT turn_id FROM turns WHERE game_id = $1 AND turn_number = 2",
        game_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let rows = get_snake_turns_by_turn_id(&pool, turn_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].game_battlesnake_id, game_snake_id);
    assert_eq!(rows[0].direction, "left");
    assert_eq!(rows[0].latency_ms, Some(42));
    assert!(rows[0].timed_out);
    assert!(rows[0].errored);
    assert_eq!(
        &capture.states(game_id)[4..],
        [
            ("insert_snake".into(), "started".into()),
            ("insert_snake".into(), "completed".into()),
        ]
    );
}

// --- persist_turn + resume-state atomicity (DEV-1569) ---

/// A full two-snake resume snapshot with every field shaped: one snake with
/// last_move/context (latency + shout)/food_eaten, one with the death group
/// and an off-board body point (an OutOfBounds head), plus raw hazards
/// holding a Snail-Mode-style off-board pending tail.
fn rich_resume(snake_a: Uuid, snake_b: Uuid, turn: i32) -> crate::engine::resume::ResumeStateData {
    crate::engine::resume::ResumeStateData {
        turn_number: turn,
        food: serde_json::json!([{"x": 3, "y": 4}]),
        hazards: serde_json::json!([{"x": 2, "y": 5}, {"x": 2, "y": 22}]),
        snakes: vec![
            crate::engine::resume::ResumeSnakeRecord {
                game_battlesnake_id: snake_a,
                health: 97,
                eliminated_cause: "not_eliminated".to_owned(),
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
                last_move: Some("right".to_owned()),
                context_present: true,
                latency_ms: Some(123),
                shout: Some("hello".to_owned()),
                food_eaten: 2,
                body: serde_json::json!([{"x": 5, "y": 5}, {"x": 4, "y": 5}, {"x": 3, "y": 5}]),
            },
            crate::engine::resume::ResumeSnakeRecord {
                game_battlesnake_id: snake_b,
                health: 0,
                eliminated_cause: "out_of_bounds".to_owned(),
                eliminated_by: String::new(),
                eliminated_on_turn: turn,
                last_move: None,
                context_present: false,
                latency_ms: None,
                shout: None,
                food_eaten: 0,
                body: serde_json::json!([{"x": -1, "y": 5}, {"x": 0, "y": 5}, {"x": 1, "y": 5}]),
            },
        ],
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn persist_turn_writes_turn_moves_and_resume_state_atomically(pool: PgPool) {
    let game_id = game(&pool).await;
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (external_github_id, github_login)
         VALUES (7311, 'persist-turn-owner') RETURNING user_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut game_snake_ids = vec![];
    for name in ["alpha", "bravo"] {
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        let game_snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id)
             VALUES ($1, $2) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(snake_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        game_snake_ids.push(game_snake_id);
    }
    let [snake_a, snake_b] = [game_snake_ids[0], game_snake_ids[1]];

    let resume = rich_resume(snake_a, snake_b, 4);
    persist_turn(
        &pool,
        game_id,
        4,
        serde_json::json!({"Turn": 4}),
        &resume,
        &[
            SnakeMoveRecord {
                game_battlesnake_id: snake_a,
                direction: "right".to_owned(),
                latency_ms: Some(123),
                timed_out: false,
                errored: false,
            },
            SnakeMoveRecord {
                game_battlesnake_id: snake_b,
                direction: "left".to_owned(),
                latency_ms: None,
                timed_out: true,
                errored: true,
            },
        ],
    )
    .await
    .unwrap();

    // The turns row and both snake moves exist.
    let turn_id: Uuid =
        sqlx::query_scalar("SELECT turn_id FROM turns WHERE game_id = $1 AND turn_number = 4")
            .bind(game_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let moves = get_snake_turns_by_turn_id(&pool, turn_id).await.unwrap();
    assert_eq!(moves.len(), 2);

    // The state row exists with raw hazards (off-board point included).
    let (state_turn, food, hazards): (i32, serde_json::Value, serde_json::Value) = sqlx::query_as(
        "SELECT turn_number, food, hazards FROM game_resume_states WHERE game_id = $1",
    )
    .bind(game_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state_turn, 4);
    assert_eq!(food, serde_json::json!([{"x": 3, "y": 4}]));
    assert_eq!(
        hazards,
        serde_json::json!([{"x": 2, "y": 5}, {"x": 2, "y": 22}])
    );

    // Every snake row carries the captured values.
    let loaded = crate::models::resume_state::load_resume_state(&pool, game_id)
        .await
        .unwrap()
        .expect("state row present");
    assert_eq!(loaded.turn_number, 4);
    let by_id = |id: Uuid| {
        loaded
            .snakes
            .iter()
            .find(|s| s.game_battlesnake_id == id)
            .unwrap_or_else(|| panic!("snake {id} present"))
    };
    let a = by_id(snake_a);
    assert_eq!(a.health, 97);
    assert_eq!(a.eliminated_cause, "not_eliminated");
    assert_eq!(a.last_move.as_deref(), Some("right"));
    assert!(a.context_present);
    assert_eq!(a.latency_ms, Some(123));
    assert_eq!(a.shout.as_deref(), Some("hello"));
    assert_eq!(a.food_eaten, 2);
    let b = by_id(snake_b);
    assert_eq!(b.eliminated_cause, "out_of_bounds");
    assert_eq!(b.last_move, None);
    assert!(!b.context_present);

    // A second persist_turn at T+1 UPDATES the same single state row.
    let mut next = rich_resume(snake_a, snake_b, 5);
    next.snakes[0].health = 96;
    next.snakes[0].food_eaten = 3;
    persist_turn(
        &pool,
        game_id,
        5,
        serde_json::json!({"Turn": 5}),
        &next,
        &[],
    )
    .await
    .unwrap();
    let state_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM game_resume_states WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state_rows, 1);
    let snake_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM game_resume_snakes WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(snake_rows, 2);
    let updated = crate::models::resume_state::load_resume_state(&pool, game_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.turn_number, 5);
    let a = updated
        .snakes
        .iter()
        .find(|s| s.game_battlesnake_id == snake_a)
        .unwrap();
    assert_eq!((a.health, a.food_eaten), (96, 3));

    // A duplicate persist_turn for the same (game_id, turn_number) errors:
    // the unique constraint is the last-resort double-writer guard.
    let duplicate = persist_turn(
        &pool,
        game_id,
        5,
        serde_json::json!({"Turn": 5}),
        &rich_resume(snake_a, snake_b, 5),
        &[],
    )
    .await;
    assert!(duplicate.is_err(), "duplicate turn insert must fail loudly");
}

#[sqlx::test(migrations = "../migrations")]
async fn game_has_turns_and_turn_row_exists_track_rows(pool: PgPool) {
    let game_id = game(&pool).await;
    assert!(!game_has_turns(&pool, game_id).await.unwrap());
    // A legacy-shaped row (frame NULL) still counts as "has turns".
    sqlx::query("INSERT INTO turns (game_id, turn_number) VALUES ($1, 0)")
        .bind(game_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(game_has_turns(&pool, game_id).await.unwrap());
    assert!(turn_row_exists(&pool, game_id, 0).await.unwrap());
    assert!(!turn_row_exists(&pool, game_id, 1).await.unwrap());
}

#[sqlx::test(migrations = "../migrations")]
async fn delete_turns_above_removes_exactly_the_tail(pool: PgPool) {
    let game_id = game(&pool).await;
    for turn in 0..=5 {
        sqlx::query("INSERT INTO turns (game_id, turn_number) VALUES ($1, $2)")
            .bind(game_id)
            .bind(turn)
            .execute(&pool)
            .await
            .unwrap();
    }
    delete_turns_above(&pool, game_id, 3).await.unwrap();
    let turns: Vec<i32> =
        sqlx::query_scalar("SELECT turn_number FROM turns WHERE game_id = $1 ORDER BY turn_number")
            .bind(game_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(turns, vec![0, 1, 2, 3]);
}
