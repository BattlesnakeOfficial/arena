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

#[sqlx::test(migrations = "../migrations")]
async fn snake_insert_failure_is_identified_inside_its_turn(database: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&database).await;
    let pool = single_connection(&database).await;
    let turn = create_turn(&pool, game_id, 8, None).await.unwrap();
    let error = create_snake_turn(&pool, &turn, Uuid::new_v4(), "up", Some(42), false, false)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("Failed to create snake turn"));
    assert_eq!(
        &capture.states(game_id)[4..],
        [
            ("acquire_snake_connection".into(), "started".into()),
            ("acquire_snake_connection".into(), "completed".into()),
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
    let _connection = tokio::time::timeout(Duration::from_secs(1), pool.acquire())
        .await
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn successful_snake_insert_keeps_values_and_emits_completion(pool: PgPool) {
    let capture = Capture::shared();
    let game_id = game(&pool).await;
    let user_id = sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (7310, 'storage-test', 'test-token') RETURNING user_id")
        .fetch_one(&pool).await.unwrap();
    let snake_id = sqlx::query_scalar!("INSERT INTO battlesnakes (user_id, name, url, visibility) VALUES ($1, 'Storage Snake', 'http://localhost:8000', 'public') RETURNING battlesnake_id", user_id)
        .fetch_one(&pool).await.unwrap();
    let game_snake_id = sqlx::query_scalar!("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2) RETURNING game_battlesnake_id", game_id, snake_id)
        .fetch_one(&pool).await.unwrap();
    let turn = create_turn(&pool, game_id, 2, None).await.unwrap();
    let saved = create_snake_turn(&pool, &turn, game_snake_id, "left", Some(42), true, true)
        .await
        .unwrap();
    let rows = get_snake_turns_by_turn_id(&pool, turn.turn_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].snake_turn_id, saved.snake_turn_id);
    assert_eq!(rows[0].game_battlesnake_id, game_snake_id);
    assert_eq!(rows[0].direction, "left");
    assert_eq!(rows[0].latency_ms, Some(42));
    assert!(rows[0].timed_out);
    assert!(rows[0].errored);
    assert_eq!(
        &capture.states(game_id)[4..],
        [
            ("acquire_snake_connection".into(), "started".into()),
            ("acquire_snake_connection".into(), "completed".into()),
            ("insert_snake".into(), "started".into()),
            ("insert_snake".into(), "completed".into()),
        ]
    );
}
