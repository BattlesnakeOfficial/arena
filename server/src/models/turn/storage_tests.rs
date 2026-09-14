use super::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tracing::{field::Visit, span::Attributes};
use tracing_subscriber::{Layer, prelude::*};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Record>>>);

type Record = (bool, BTreeMap<String, String>);

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        _: &tracing::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attrs.metadata().name() == "arena.game.phase" {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            self.0.lock().unwrap().push((true, fields.0));
        }
    }
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.get("event_type").map(String::as_str) == Some("game_phase") {
            self.0.lock().unwrap().push((false, fields.0));
        }
    }
}

impl Capture {
    fn shared() -> &'static Self {
        static CAPTURE: OnceLock<Capture> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let capture = Capture::default();
            // Keep callsite interest stable for concurrent database tests. Each
            // assertion selects its own game UUID from the shared capture.
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(capture.clone()),
            )
            .unwrap();
            capture
        })
    }

    fn records(&self, game_id: Uuid, spans: bool) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(span, fields)| {
                *span == spans && fields.get("game_id") == Some(&game_id.to_string())
            })
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    fn states(&self, game_id: Uuid) -> Vec<(String, String)> {
        self.records(game_id, false)
            .into_iter()
            .map(|fields| {
                (
                    fields["phase"]
                        .trim_start_matches("persist_turn.")
                        .to_owned(),
                    fields["state"].clone(),
                )
            })
            .collect()
    }
}

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
    let channels = GameChannels::new();
    let mut receiver = channels.subscribe(game_id).await;
    let frame = serde_json::json!({"test_frame": "retained in storage only"});
    let work = create_turn(&pool, &channels, game_id, 7, Some(frame.clone()));
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
    assert_eq!(receiver.try_recv().unwrap().turn_number, 7);
    let states = capture.states(game_id);
    assert_eq!(
        states,
        [
            ("acquire_frame_connection".into(), "started".into()),
            ("acquire_frame_connection".into(), "completed".into()),
            ("insert_frame".into(), "started".into()),
            ("insert_frame".into(), "completed".into()),
            ("notify".into(), "started".into()),
            ("notify".into(), "completed".into()),
        ]
    );
    let spans = capture.records(game_id, true);
    assert_eq!(spans.len(), 3);
    assert!(
        spans
            .iter()
            .all(|fields| fields["turn"] == "7" && !fields.contains_key("frame_data"))
    );
    let events = capture.records(game_id, false);
    assert!(events[1]["duration_ms"].parse::<u64>().unwrap() >= 20);
}

#[sqlx::test(migrations = "../migrations")]
async fn failed_frame_insert_never_notifies_and_returns_connection(database: PgPool) {
    let capture = Capture::shared();
    let pool = single_connection(&database).await;
    let game_id = Uuid::new_v4(); // No matching game: the real INSERT fails its FK.
    let channels = GameChannels::new();
    let mut receiver = channels.subscribe(game_id).await;
    let error = create_turn(&pool, &channels, game_id, 3, None)
        .await
        .unwrap_err();
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
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
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
    let channels = GameChannels::new();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            create_turn(&pool, &channels, game_id, 4, None)
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
    let turn = create_turn(&pool, &GameChannels::new(), game_id, 8, None)
        .await
        .unwrap();
    let error = create_snake_turn(&pool, &turn, Uuid::new_v4(), "up", Some(42), false)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("Failed to create snake turn"));
    assert_eq!(
        &capture.states(game_id)[6..],
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
    let turn = create_turn(&pool, &GameChannels::new(), game_id, 2, None)
        .await
        .unwrap();
    let saved = create_snake_turn(&pool, &turn, game_snake_id, "left", Some(42), true)
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
    assert_eq!(
        &capture.states(game_id)[6..],
        [
            ("acquire_snake_connection".into(), "started".into()),
            ("acquire_snake_connection".into(), "completed".into()),
            ("insert_snake".into(), "started".into()),
            ("insert_snake".into(), "completed".into()),
        ]
    );
}
