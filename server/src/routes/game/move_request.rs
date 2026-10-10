//! `GET /api/games/{id}/move-request?turn=&you=`: the `/move` request body a
//! snake was sent on one turn of a game, rebuilt from the persisted frame.
//!
//! Lets snake developers grab any turn from the game viewer and replay it
//! against their snake locally, without a browser extension or CLI.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use color_eyre::eyre::{Context as _, eyre};
use rules::{BoardState, EliminationCause};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    engine::{EngineGame, GameMeta, MOVE_TIMEOUT_MS, frame::SnakeCustomizations},
    engine_models::{EngineGameFrame, EngineSnake},
    errors::{ServerError, ServerResult},
    models::{
        game::{get_game_by_id, get_game_source},
        turn::get_turn_frame,
    },
    state::AppState,
    wire::{self, SnakeContext},
};

#[derive(Debug, Deserialize)]
pub struct MoveRequestQuery {
    /// Turn number, as shown by the board viewer.
    pub turn: i32,
    /// The requesting snake's ID in the game's frames (its
    /// `game_battlesnake_id` for arena games); becomes `you`.
    pub you: String,
}

/// Why a snake has no `/move` request for a turn.
#[derive(Debug, PartialEq, Eq)]
enum NoMoveRequest {
    /// No snake with that ID played in the game.
    UnknownSnake,
    /// The snake was already eliminated, so it wasn't asked for a move.
    Eliminated { name: String, death_turn: i32 },
}

impl NoMoveRequest {
    fn message(&self, turn: i32) -> String {
        match self {
            NoMoveRequest::UnknownSnake => "That snake didn't play in this game.".to_string(),
            NoMoveRequest::Eliminated { name, death_turn } => format!(
                "{name} was eliminated on turn {death_turn}, so it wasn't sent a move request \
                 on turn {turn}. Its last move request was on turn {}.",
                death_turn - 1
            ),
        }
    }
}

/// Rebuild the `/move` request `you` was sent on `frame`'s turn.
///
/// A frame is the board state the engine requested moves on, along with
/// each snake's latency and shout from its previous move (the
/// `SnakeContext` the runner sends). Feeding it back through
/// [`wire::Game::from_engine_game`] keeps the download in step with what
/// snakes actually receive. One known gap: a snake whose `/info` failed was
/// sent empty customizations, but its frame stores the board viewer's
/// fallback color and `"default"` head and tail.
fn move_request_from_frame(
    frame: &EngineGameFrame,
    meta: GameMeta,
    (width, height): (i32, i32),
    you: &str,
) -> Result<wire::Game, NoMoveRequest> {
    let you_snake = frame
        .snakes
        .iter()
        .find(|s| s.id == you)
        .ok_or(NoMoveRequest::UnknownSnake)?;
    if let Some(death) = &you_snake.death {
        return Err(NoMoveRequest::Eliminated {
            name: you_snake.name.clone(),
            death_turn: death.turn,
        });
    }

    // Requests only ever include living snakes, so the dead are left out.
    let alive: Vec<&EngineSnake> = frame.snakes.iter().filter(|s| s.death.is_none()).collect();
    let point = |p: &crate::engine_models::Point| rules::Point::new(p.x, p.y);

    let game = EngineGame {
        board: BoardState {
            turn: frame.turn,
            width,
            height,
            food: frame.food.iter().map(point).collect(),
            snakes: alive
                .iter()
                .map(|s| rules::Snake {
                    id: s.id.clone(),
                    body: s.body.iter().map(point).collect(),
                    health: s.health,
                    eliminated_cause: EliminationCause::NotEliminated,
                    eliminated_by: String::new(),
                    eliminated_on_turn: 0,
                })
                .collect(),
            hazards: frame.hazards.iter().map(point).collect(),
        },
        meta,
        snake_names: alive
            .iter()
            .map(|s| (s.id.clone(), s.name.clone()))
            .collect(),
    };
    let contexts: HashMap<String, SnakeContext> = alive
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                SnakeContext {
                    latency_ms: s.latency.as_deref().and_then(|ms| ms.parse().ok()),
                    shout: s.shout.clone(),
                },
            )
        })
        .collect();
    let customizations: HashMap<String, SnakeCustomizations> = alive
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                SnakeCustomizations {
                    color: s.color.clone().unwrap_or_default(),
                    head: s.head_type.clone().unwrap_or_default(),
                    tail: s.tail_type.clone().unwrap_or_default(),
                    author: String::new(),
                },
            )
        })
        .collect();

    Ok(wire::Game::from_engine_game(
        &game,
        you,
        &contexts,
        &customizations,
    ))
}

/// `{game}-turn-{turn}-{snake}.json`, with the snake name slugged so the
/// filename is safe inside a quoted `Content-Disposition` parameter.
fn download_filename(game_id: Uuid, turn: i32, snake_name: &str) -> String {
    let lowered: String = snake_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = lowered
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "snake" } else { &slug };
    format!("{game_id}-turn-{turn}-{slug}.json")
}

/// GET /api/games/{id}/move-request?turn=&you=
///
/// Public, like the frames it's built from. Served as a pretty-printed JSON
/// download; a turn with no frame, or a snake with no request on that turn,
/// is a 404 with a plain-text reason.
pub async fn get_move_request(
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
    Query(query): Query<MoveRequestQuery>,
) -> ServerResult<Response, StatusCode> {
    let game = get_game_by_id(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch game")?
        .ok_or_else(|| ServerError(eyre!("Game not found"), StatusCode::NOT_FOUND))?;

    let Some(mut frame) = get_turn_frame(&state.db, game_id, query.turn)
        .await
        .wrap_err("Failed to fetch turn frame")?
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            format!("Turn {} of this game hasn't been played.", query.turn),
        )
            .into_response());
    };

    // The same public treatment the frames API gives: moderated shouts stay
    // hidden, and legacy "timeout" latencies become the timeout in ms.
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &game.status).await;
    crate::moderation::shouts::strip_suppressed_shouts(&mut frame, &suppressed);
    crate::engine::frame::normalize_public_frame(&mut frame, MOVE_TIMEOUT_MS);
    let frame: EngineGameFrame =
        serde_json::from_value(frame).wrap_err("Failed to parse turn frame")?;

    let (ruleset_name, settings, royale) = crate::engine::mode_rules(game.game_type, game_id);
    let meta = GameMeta {
        game_id: game_id.to_string(),
        ruleset_name: ruleset_name.to_string(),
        timeout: MOVE_TIMEOUT_MS,
        settings,
        royale,
        source: get_game_source(&state.db, game_id)
            .await
            .wrap_err("Failed to load game source")?,
    };
    let (width, height) = game.board_size.dimensions();

    let request =
        match move_request_from_frame(&frame, meta, (width as i32, height as i32), &query.you) {
            Ok(request) => request,
            Err(reason) => {
                return Ok((StatusCode::NOT_FOUND, reason.message(query.turn)).into_response());
            }
        };

    let body =
        serde_json::to_string_pretty(&request).wrap_err("Failed to serialize move request")?;
    let filename = download_filename(game_id, query.turn, &request.you.name);
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use proptest::prelude::*;
    use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
    use rules::Direction;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    use crate::engine::GameSource;
    use crate::engine::frame::{DeathInfo, game_to_frame, normalize_public_frame};
    use crate::models::game::GameType;
    use crate::snake_client::MoveResult;

    /// Serialize a frame like the runner persists it, then read it back the
    /// way the endpoint does.
    fn persisted(frame: &crate::engine::frame::EngineGameFrame) -> EngineGameFrame {
        let mut json = serde_json::to_value(frame).unwrap();
        normalize_public_frame(&mut json, MOVE_TIMEOUT_MS);
        serde_json::from_value(json).unwrap()
    }

    fn new_game(game_type: GameType, snakes: usize, rng: &mut StdRng) -> EngineGame {
        let game_id = Uuid::from_u128(rng.r#gen());
        let ids: Vec<String> = (0..snakes).map(|i| format!("snake-{i}")).collect();
        let board = rules::board::create_default_board_state(rng, 11, 11, &ids).unwrap();
        let (ruleset_name, settings, royale) = crate::engine::mode_rules(game_type, game_id);
        EngineGame {
            board,
            meta: GameMeta {
                game_id: game_id.to_string(),
                ruleset_name: ruleset_name.to_string(),
                timeout: MOVE_TIMEOUT_MS,
                settings,
                royale,
                source: GameSource::Ladder,
            },
            snake_names: ids
                .iter()
                .map(|id| (id.clone(), format!("Snake {id}")))
                .collect(),
        }
    }

    /// A move that stays on the board and off the neck when one exists, so
    /// games last long enough to reach hazards and food.
    fn pick_move(snake: &rules::Snake, board: &BoardState, rng: &mut StdRng) -> Direction {
        let all = [
            Direction::Up,
            Direction::Down,
            Direction::Left,
            Direction::Right,
        ];
        let head = snake.head();
        let sensible: Vec<Direction> = all
            .into_iter()
            .filter(|d| {
                let (dx, dy) = d.to_delta();
                let next = rules::Point::new(head.x + dx, head.y + dy);
                (0..board.width).contains(&next.x)
                    && (0..board.height).contains(&next.y)
                    && snake.body.get(1) != Some(&next)
            })
            .collect();
        *sensible.choose(rng).or(all.choose(rng)).unwrap()
    }

    /// Play a game the way `game_runner` does and check, on every turn and
    /// for every snake, that the request rebuilt from the persisted frame is
    /// exactly the one the snake was sent.
    fn assert_round_trips(
        game_type: GameType,
        snakes: usize,
        seed: u64,
        max_turns: i32,
    ) -> Result<(), TestCaseError> {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut game = new_game(game_type, snakes, &mut rng);
        let customizations: HashMap<String, SnakeCustomizations> = game
            .board
            .snakes
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    SnakeCustomizations {
                        color: "#ff00aa".to_string(),
                        head: "beluga".to_string(),
                        tail: "curled".to_string(),
                        author: "owner".to_string(),
                    },
                )
            })
            .collect();
        let mut contexts: HashMap<String, SnakeContext> = HashMap::new();
        let mut results: Vec<MoveResult> = Vec::new();
        let mut deaths: Vec<DeathInfo> = Vec::new();

        loop {
            let frame = persisted(&game_to_frame(&game, &deaths, &results, &customizations));
            let size = (game.board.width, game.board.height);
            for snake in &game.board.snakes {
                let rebuilt = move_request_from_frame(&frame, game.meta.clone(), size, &snake.id);
                if snake.eliminated_cause.is_eliminated() {
                    prop_assert!(
                        matches!(rebuilt, Err(NoMoveRequest::Eliminated { .. })),
                        "turn {}: {} is dead but got a request",
                        game.board.turn,
                        snake.id
                    );
                } else {
                    let sent =
                        wire::Game::from_engine_game(&game, &snake.id, &contexts, &customizations);
                    prop_assert_eq!(
                        serde_json::to_value(rebuilt.unwrap()).unwrap(),
                        serde_json::to_value(sent).unwrap(),
                        "turn {} as {}",
                        game.board.turn,
                        snake.id
                    );
                }
            }

            if crate::engine::is_game_over(&game) || game.board.turn >= max_turns {
                return Ok(());
            }

            results = game
                .board
                .snakes
                .iter()
                .filter(|s| !s.eliminated_cause.is_eliminated())
                .map(|s| MoveResult {
                    snake_id: s.id.clone(),
                    direction: pick_move(s, &game.board, &mut rng),
                    latency_ms: rng.gen_bool(0.8).then(|| rng.gen_range(0..700)),
                    timed_out: rng.gen_bool(0.1),
                    shout: rng
                        .gen_bool(0.5)
                        .then(|| format!("turn {}", game.board.turn)),
                    status_code: Some(200),
                    errored: false,
                })
                .collect();
            contexts = results
                .iter()
                .map(|r| {
                    (
                        r.snake_id.clone(),
                        SnakeContext {
                            latency_ms: wire::reported_latency_ms(
                                r.latency_ms,
                                r.timed_out,
                                game.meta.timeout,
                            ),
                            shout: r.shout.clone(),
                        },
                    )
                })
                .collect();
            let moves: Vec<(String, Direction)> = results
                .iter()
                .map(|r| (r.snake_id.clone(), r.direction))
                .collect();
            crate::engine::apply_turn(&mut game, &moves).unwrap();
            game.board.turn += 1;
            crate::engine::spawn_food(&mut game);
            for snake in &game.board.snakes {
                if snake.eliminated_cause.is_eliminated()
                    && !deaths.iter().any(|d| d.snake_id == snake.id)
                {
                    deaths.push(DeathInfo {
                        snake_id: snake.id.clone(),
                        turn: game.board.turn,
                        cause: "test".to_string(),
                        eliminated_by: snake.eliminated_by.clone(),
                    });
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn rebuilt_request_matches_what_the_snake_was_sent(
            game_type in prop::sample::select(vec![
                GameType::Standard,
                GameType::Royale,
                GameType::SnailMode,
                GameType::Constrictor,
                GameType::Solo,
            ]),
            snakes in 1..=4usize,
            seed in any::<u64>(),
        ) {
            assert_round_trips(game_type, snakes, seed, 200)?;
        }
    }

    fn frame_with(snakes: Value) -> EngineGameFrame {
        serde_json::from_value(json!({"Turn": 12, "Snakes": snakes, "Food": [], "Hazards": []}))
            .unwrap()
    }

    fn standard_meta() -> GameMeta {
        let (ruleset_name, settings, royale) =
            crate::engine::mode_rules(GameType::Standard, Uuid::nil());
        GameMeta {
            game_id: Uuid::nil().to_string(),
            ruleset_name: ruleset_name.to_string(),
            timeout: MOVE_TIMEOUT_MS,
            settings,
            royale,
            source: GameSource::Custom,
        }
    }

    #[test]
    fn eliminated_snake_points_at_its_last_request() {
        let frame = frame_with(json!([{
            "ID": "s1",
            "Name": "Dead Snake",
            "Body": [{"X": -1, "Y": 3}],
            "Death": {"Cause": "wall-collision", "Turn": 12}
        }]));

        let err = move_request_from_frame(&frame, standard_meta(), (11, 11), "s1").unwrap_err();

        assert_eq!(
            err,
            NoMoveRequest::Eliminated {
                name: "Dead Snake".to_string(),
                death_turn: 12
            }
        );
        assert_eq!(
            err.message(12),
            "Dead Snake was eliminated on turn 12, so it wasn't sent a move request on \
             turn 12. Its last move request was on turn 11."
        );
    }

    #[test]
    fn unknown_snake_has_no_request() {
        let frame = frame_with(json!([{"ID": "s1", "Name": "Snake", "Body": [{"X": 1, "Y": 1}]}]));

        assert_eq!(
            move_request_from_frame(&frame, standard_meta(), (11, 11), "nope").unwrap_err(),
            NoMoveRequest::UnknownSnake
        );
    }

    #[test]
    fn download_filename_slugs_the_snake_name() {
        let game_id = Uuid::parse_str("6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1").unwrap();
        assert_eq!(
            download_filename(game_id, 42, "Mr. Snek \"2\" 🐍!"),
            "6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1-turn-42-mr-snek-2.json"
        );
        assert_eq!(
            download_filename(game_id, 0, "🐍"),
            "6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1-turn-0-snake.json"
        );
    }

    // --- HTTP ---

    async fn get(pool: &PgPool, path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = crate::routes::routes(AppState::test_from_pool(pool.clone()))
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// A finished Royale game whose turn 3 has a timed-out snake (legacy
    /// `"timeout"` latency), a living rival, and an eliminated snake.
    async fn fixture(pool: &PgPool) -> Uuid {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Royale', 'finished') RETURNING game_id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let frame = json!({
            "Turn": 3,
            "Snakes": [
                {
                    "ID": "s1", "Name": "Slow Snake", "Health": 97,
                    "Body": [{"X": 5, "Y": 5}, {"X": 5, "Y": 4}, {"X": 5, "Y": 3}],
                    "Color": "#123456", "HeadType": "beluga", "TailType": "curled",
                    "Latency": "timeout", "Shout": "bad words", "Death": null
                },
                {
                    "ID": "s2", "Name": "Rival", "Health": 88,
                    "Body": [{"X": 1, "Y": 1}, {"X": 1, "Y": 2}, {"X": 1, "Y": 3}],
                    "Color": "#654321", "HeadType": "default", "TailType": "default",
                    "Latency": "42", "Shout": "hi", "Death": null
                },
                {
                    "ID": "s3", "Name": "Gone", "Health": 0,
                    "Body": [{"X": -1, "Y": 8}, {"X": 0, "Y": 8}, {"X": 1, "Y": 8}],
                    "Latency": "0", "Shout": "",
                    "Death": {"Cause": "wall-collision", "Turn": 2, "EliminatedBy": ""}
                }
            ],
            "Food": [{"X": 9, "Y": 9}],
            "Hazards": [{"X": 0, "Y": 0}]
        });
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 3, $2)")
            .bind(game_id)
            .bind(frame)
            .execute(pool)
            .await
            .unwrap();
        game_id
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn serves_the_turn_as_a_json_download(pool: PgPool) {
        let game_id = fixture(&pool).await;
        crate::models::shout_moderation::insert_suppressed_shout(
            &pool,
            game_id,
            "s1",
            "bad words",
            Some(0.99),
            Some("jev-test"),
        )
        .await
        .unwrap();

        let (status, headers, body) = get(
            &pool,
            &format!("/api/games/{game_id}/move-request?turn=3&you=s1"),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            format!("attachment; filename=\"{game_id}-turn-3-slow-snake.json\"")
        );
        let request: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(request["turn"], 3);
        assert_eq!(request["game"]["id"], game_id.to_string());
        assert_eq!(request["game"]["map"], "royale");
        assert_eq!(request["game"]["ruleset"]["name"], "standard");
        assert_eq!(
            request["game"]["ruleset"]["settings"]["royale"]["shrinkEveryNTurns"],
            25
        );
        assert_eq!(request["game"]["source"], "custom");
        assert_eq!(request["game"]["timeout"], 500);
        assert_eq!(request["board"]["width"], 11);
        assert_eq!(request["board"]["food"], json!([{"x": 9, "y": 9}]));
        assert_eq!(request["board"]["hazards"], json!([{"x": 0, "y": 0}]));
        // The eliminated snake isn't on the board, just like the live request.
        let ids: Vec<&str> = request["board"]["snakes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["s1", "s2"]);
        let you = &request["you"];
        assert_eq!(you["id"], "s1");
        assert_eq!(you["head"], json!({"x": 5, "y": 5}));
        assert_eq!(you["length"], 3);
        assert_eq!(you["health"], 97);
        // A legacy timeout reports the full timeout, like the engine did.
        assert_eq!(you["latency"], "500");
        // Moderated shouts stay hidden here too.
        assert_eq!(you["shout"], "");
        assert_eq!(
            you["customizations"],
            json!({"color": "#123456", "head": "beluga", "tail": "curled"})
        );
        assert_eq!(request["board"]["snakes"][1]["shout"], "hi");
        assert_eq!(request["board"]["snakes"][1]["latency"], "42");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn explains_why_there_is_no_request(pool: PgPool) {
        let game_id = fixture(&pool).await;
        let path = |query: &str| format!("/api/games/{game_id}/move-request?{query}");

        let (status, _, body) = get(&pool, &path("turn=3&you=s3")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body.contains("Gone was eliminated on turn 2") && body.contains("on turn 1."),
            "{body}"
        );

        let (status, _, body) = get(&pool, &path("turn=3&you=nobody")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "That snake didn't play in this game.");

        let (status, _, body) = get(&pool, &path("turn=4&you=s1")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "Turn 4 of this game hasn't been played.");

        let (status, _, _) = get(&pool, &path("you=s1")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _, _) = get(
            &pool,
            &format!("/api/games/{}/move-request?turn=3&you=s1", Uuid::new_v4()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Insert a snake owned by `user_id` into `game_id`, returning its
    /// `game_battlesnake_id`.
    async fn join(pool: &PgPool, game_id: Uuid, user_id: Uuid, name: &str) -> Uuid {
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'http://snake', 'public') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id)
             VALUES ($1, $2) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(snake_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_page_offers_the_download_as_the_viewers_snake(pool: PgPool) {
        use crate::routes::test_support::{
            create_user_session, session_user_id, signed_session_cookie,
        };

        let state = AppState::test_from_pool(pool.clone());
        let other_session = create_user_session(&pool, 9_960_001, false).await;
        let viewer_session = create_user_session(&pool, 9_960_002, false).await;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'running') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let other = join(
            &pool,
            game_id,
            session_user_id(&pool, other_session).await,
            "Their Snake",
        )
        .await;
        let mine = join(
            &pool,
            game_id,
            session_user_id(&pool, viewer_session).await,
            "My Snake",
        )
        .await;

        let page = |path: String| {
            let state = state.clone();
            async move {
                let cookie = format!(
                    "{}={}",
                    crate::models::session::SESSION_COOKIE_NAME,
                    signed_session_cookie(&state, viewer_session)
                );
                let response = crate::routes::routes(state)
                    .layer(tower_cookies::CookieManagerLayer::new())
                    .oneshot(
                        Request::builder()
                            .uri(path)
                            .header(header::COOKIE, cookie)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                String::from_utf8(body.to_vec()).unwrap()
            }
        };

        let body = page(format!("/games/{game_id}?turn=7")).await;
        assert!(
            body.contains(&format!(r#"action="/api/games/{game_id}/move-request""#)),
            "{body}"
        );
        assert!(body.contains(r#"id="move-request-turn""#), "{body}");
        assert!(body.contains(r#"value="7""#), "{body}");
        assert!(
            body.contains(&format!(
                r#"<option value="{mine}" selected>My Snake</option>"#
            )),
            "{body}"
        );
        assert!(
            body.contains(&format!(r#"<option value="{other}">Their Snake</option>"#)),
            "{body}"
        );
        // The board's TURN messages keep the form on the turn being viewed.
        assert!(body.contains("evt.event === 'TURN'"), "{body}");

        // Nothing to download before the game starts.
        sqlx::query("UPDATE games SET status = 'waiting' WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let body = page(format!("/games/{game_id}")).await;
        assert!(
            !body.contains(&format!("/api/games/{game_id}/move-request")),
            "{body}"
        );
    }
}
