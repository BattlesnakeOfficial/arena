//! Social card images (`og:image`): `GET /og/...png`.
//!
//! Cards render on the blocking pool, a couple at a time, and are cached by
//! clients: social platforms fetch a card once per shared link, so there is
//! no server-side cache beyond the static default card.

use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use color_eyre::eyre::{Context as _, eyre};
use std::collections::HashMap;
use tokio::sync::{OnceCell, Semaphore};
use uuid::Uuid;

use arena::design_kit::AssetKind;

use crate::{
    components::page::DEFAULT_DESCRIPTION,
    engine_models::{EngineGameFrame, EngineSnake},
    errors::{ServerError, ServerResult},
    models::{
        game::{GameStatus, get_game_by_id},
        game_battlesnake::{
            GameBattlesnakeWithDetails, get_battlesnakes_by_game_id, join_order_key,
        },
        turn::{get_latest_frame, get_turn_frames_page},
    },
    og::{
        board::{BoardArt, BoardSnake, shape},
        canvas::snake_color,
        cards::{self, GameCard, RosterSnake},
    },
    routes::game::view::ViewGameParams,
    state::AppState,
};

/// Cards rendering at once. Each is ~50 ms of CPU; social scrapers arrive in
/// small bursts when a link is shared, so two keeps a burst off the request
/// workers without queueing anyone for long.
static RENDER_PERMITS: Semaphore = Semaphore::const_new(2);

const DAY: u32 = 24 * 60 * 60;
/// Running and waiting games change turn by turn.
const LIVE_MAX_AGE: u32 = 60;

/// Largest board side drawn. Real boards top out at 25×25; a malformed custom
/// size must not turn into a million grid cells.
const MAX_BOARD_SIDE: i32 = 25;

/// The spoiler-free card shows this turn: by turn 2 every snake has left its
/// starting cell, so the board shows bodies rather than stacked heads.
const UNFURLED_TURN: i64 = 2;

pub const DEFAULT_CARD_PATH: &str = "/og/default.png";

pub fn game_card_path(game_id: Uuid, spoilers: bool) -> String {
    if spoilers {
        format!("/og/games/{game_id}.png?showSpoilers=true")
    } else {
        format!("/og/games/{game_id}.png")
    }
}

async fn render(
    card: impl FnOnce() -> cja::Result<Vec<u8>> + Send + 'static,
) -> cja::Result<Vec<u8>> {
    let _permit = RENDER_PERMITS
        .acquire()
        .await
        .wrap_err("Social card render limiter closed")?;
    tokio::task::spawn_blocking(card)
        .await
        .wrap_err("Social card render task failed")?
}

fn png(bytes: Vec<u8>, max_age: u32) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CACHE_CONTROL, format!("public, max-age={max_age}")),
        ],
        bytes,
    )
        .into_response()
}

fn not_found() -> ServerError<StatusCode> {
    ServerError(eyre!("Social card not found"), StatusCode::NOT_FOUND)
}

/// The id in a `{uuid}.png` path segment.
fn png_id(file: &str) -> Result<Uuid, ServerError<StatusCode>> {
    file.strip_suffix(".png")
        .and_then(|id| id.parse().ok())
        .ok_or_else(not_found)
}

/// GET /og/default.png — the site-wide card. Rendered once per process.
pub async fn default_card() -> ServerResult<Response, StatusCode> {
    static CARD: OnceCell<Vec<u8>> = OnceCell::const_new();
    let bytes = CARD
        .get_or_try_init(|| render(|| cards::default_card(DEFAULT_DESCRIPTION)))
        .await?;
    Ok(png(bytes.clone(), DAY))
}

/// GET /og/games/{id}.png[?showSpoilers]
pub async fn game_card(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(params): Query<ViewGameParams>,
) -> ServerResult<Response, StatusCode> {
    let game_id = png_id(&file)?;
    let card = load_game_card(&state, game_id, params.show_spoilers())
        .await?
        .ok_or_else(not_found)?;
    let max_age = if card.status == cards::GameStatus::Finished {
        DAY
    } else {
        LIVE_MAX_AGE
    };
    let bytes = render(move || cards::game_card(&card)).await?;
    Ok(png(bytes, max_age))
}

/// A game's card, or `None` for an unknown game.
///
/// Spoiler-free unless the page was shared with `?showSpoilers` (the same
/// opt-in as the page's description): join order, an early board, everyone
/// alive. With spoilers on a finished game: placements and the final board.
async fn load_game_card(
    state: &AppState,
    game_id: Uuid,
    show_spoilers: bool,
) -> cja::Result<Option<GameCard>> {
    let Some(game) = get_game_by_id(&state.db, game_id)
        .await
        .wrap_err("Failed to get game for social card")?
    else {
        return Ok(None);
    };
    let mut snakes = get_battlesnakes_by_game_id(&state.db, game_id)
        .await
        .wrap_err("Failed to get game battlesnakes for social card")?;

    let spoilers = game.status == GameStatus::Finished && show_spoilers;
    let frame = if spoilers {
        get_latest_frame(&state.db, game_id)
            .await
            .wrap_err("Failed to get final frame for social card")?
    } else {
        // Up to the unfurled turn, or the last frame before it.
        get_turn_frames_page(&state.db, game_id, 0, UNFURLED_TURN + 1)
            .await
            .wrap_err("Failed to get opening frames for social card")?
            .into_iter()
            .rev()
            .find_map(|turn| turn.frame_data)
    };
    // A frame that doesn't parse (an old import) just means an empty board.
    let frame = frame.and_then(|f| serde_json::from_value::<EngineGameFrame>(f).ok());

    if !spoilers {
        // Placement order would give the result away.
        snakes.sort_by_key(join_order_key);
    }

    let (width, height) = game.board_size.dimensions();
    let side = |n: u32| {
        i32::try_from(n)
            .unwrap_or(MAX_BOARD_SIDE)
            .clamp(1, MAX_BOARD_SIDE)
    };
    Ok(Some(GameCard {
        status: match game.status {
            GameStatus::Waiting => cards::GameStatus::Waiting,
            GameStatus::Running => cards::GameStatus::Live,
            GameStatus::Finished => cards::GameStatus::Finished,
            GameStatus::Failed => cards::GameStatus::Failed,
        },
        mode: game.game_type.as_str().to_string(),
        width: side(width),
        height: side(height),
        roster: roster(&snakes, frame.as_ref(), spoilers),
        board: board(side(width), side(height), frame.as_ref(), spoilers),
        spoilers,
        turns: frame.as_ref().filter(|_| spoilers).map(|f| f.turn),
    }))
}

/// Roster entries in `snakes` order. Colour and head come from the frame
/// when there is one (what the snake wore in this game), else the snake's
/// current settings.
fn roster(
    snakes: &[GameBattlesnakeWithDetails],
    frame: Option<&EngineGameFrame>,
    spoilers: bool,
) -> Vec<RosterSnake> {
    let in_frame: HashMap<&str, &EngineSnake> = frame
        .map(|f| f.snakes.iter().map(|s| (s.id.as_str(), s)).collect())
        .unwrap_or_default();
    snakes
        .iter()
        .map(|snake| {
            let id = snake.game_battlesnake_id.to_string();
            let worn = in_frame.get(id.as_str());
            let color = worn
                .and_then(|s| s.color.as_deref())
                .unwrap_or(&snake.color);
            let head = worn
                .and_then(|s| s.head_type.as_deref())
                .unwrap_or(&snake.head);
            RosterSnake {
                name: snake.name.clone(),
                owner: snake.owner_name.clone(),
                color: snake_color(color),
                head: shape(AssetKind::Head, head),
                placement: snake.placement.filter(|_| spoilers),
            }
        })
        .collect()
}

fn board(width: i32, height: i32, frame: Option<&EngineGameFrame>, spoilers: bool) -> BoardArt {
    let cells = |points: &[crate::engine_models::Point]| -> Vec<(i32, i32)> {
        points.iter().map(|p| (p.x, p.y)).collect()
    };
    let Some(frame) = frame else {
        return BoardArt {
            width,
            height,
            ..BoardArt::default()
        };
    };
    BoardArt {
        width,
        height,
        snakes: frame
            .snakes
            .iter()
            .map(|s| BoardSnake {
                body: cells(&s.body),
                color: snake_color(s.color.as_deref().unwrap_or_default()),
                head: shape(AssetKind::Head, s.head_type.as_deref().unwrap_or_default()),
                tail: shape(AssetKind::Tail, s.tail_type.as_deref().unwrap_or_default()),
                // An early death would give the result away too.
                eliminated: spoilers && s.death.is_some(),
            })
            .collect(),
        food: cells(&frame.food),
        hazards: cells(&frame.hazards),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    struct Reply {
        status: StatusCode,
        content_type: Option<String>,
        cache_control: Option<String>,
        body: Vec<u8>,
    }

    async fn get(state: &AppState, uri: &str) -> Reply {
        let response = crate::routes::routes(state.clone())
            .layer(tower_cookies::CookieManagerLayer::new())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header = |name| {
            response
                .headers()
                .get(name)
                .map(|v: &axum::http::HeaderValue| v.to_str().unwrap().to_string())
        };
        let (content_type, cache_control) =
            (header(header::CONTENT_TYPE), header(header::CACHE_CONTROL));
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            content_type,
            cache_control,
            body,
        }
    }

    fn assert_card(reply: &Reply, max_age: u32) {
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.content_type.as_deref(), Some("image/png"));
        assert_eq!(
            reply.cache_control.as_deref(),
            Some(format!("public, max-age={max_age}").as_str())
        );
        let decoder = png::Decoder::new(std::io::Cursor::new(reply.body.clone()));
        let info = decoder.read_info().unwrap();
        assert_eq!((info.info().width, info.info().height), (1200, 630));
    }

    /// A finished duel: "Early Bird" joined first but placed 2nd and died on
    /// turn 3; "Late Comer" won. Frames for turns 0-4.
    async fn fixture_duel(pool: &PgPool, status: &str) -> (Uuid, [Uuid; 2]) {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ((SELECT COALESCE(MAX(external_github_id), 777000) + 1 FROM users),
                     'og-owner-' || gen_random_uuid(), '')
             RETURNING user_id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap();
        let mut ids = [Uuid::nil(); 2];
        for (i, (name, placement)) in [("Early Bird", 2), ("Late Comer", 1)]
            .into_iter()
            .enumerate()
        {
            let snake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url, color, head, tail)
                 VALUES ($1, $2, 'https://example.com', '#ff3d8a', 'bendr', 'bolt')
                 RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
            ids[i] = sqlx::query_scalar(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement, created_at)
                 VALUES ($1, $2, $3, NOW() + make_interval(secs => $4)) RETURNING game_battlesnake_id",
            )
            .bind(game_id)
            .bind(snake_id)
            .bind((status == "finished").then_some(placement))
            .bind(i as f64)
            .fetch_one(pool)
            .await
            .unwrap();
        }
        for turn in 0..=4 {
            let death =
                (turn >= 3).then(|| serde_json::json!({"Cause": "wall-collision", "Turn": 3}));
            let frame = serde_json::json!({
                "Turn": turn,
                "Snakes": [
                    {"ID": ids[0].to_string(), "Name": "Early Bird",
                     "Body": [{"X": 1, "Y": 1 + turn}, {"X": 1, "Y": turn}],
                     "Color": "#3ddba0", "HeadType": "smile", "TailType": "curled", "Death": death},
                    {"ID": ids[1].to_string(), "Name": "Late Comer",
                     "Body": [{"X": 9, "Y": 9 - turn}, {"X": 9, "Y": 10 - turn}],
                     "Color": "#ff3d8a", "HeadType": "bendr", "TailType": "bolt", "Death": null}
                ],
                "Food": [{"X": 5, "Y": 5}],
                "Hazards": []
            });
            sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
                .bind(game_id)
                .bind(turn)
                .bind(frame)
                .execute(pool)
                .await
                .unwrap();
        }
        (game_id, ids)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn serves_the_default_card(pool: PgPool) {
        let state = AppState::test_from_pool(pool);
        assert_card(&get(&state, "/og/default.png").await, DAY);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn serves_game_cards_and_404s_unknown_ones(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _) = fixture_duel(&pool, "finished").await;

        let plain = get(&state, &format!("/og/games/{game_id}.png")).await;
        assert_card(&plain, DAY);
        let spoiled = get(
            &state,
            &format!("/og/games/{game_id}.png?showSpoilers=true"),
        )
        .await;
        assert_card(&spoiled, DAY);
        assert_ne!(plain.body, spoiled.body, "spoilers must change the card");

        for missing in [
            format!("/og/games/{}.png", Uuid::new_v4()),
            format!("/og/games/{game_id}"),
            "/og/games/not-a-game.png".to_string(),
        ] {
            assert_eq!(
                get(&state, &missing).await.status,
                StatusCode::NOT_FOUND,
                "{missing}"
            );
        }

        let (running, _) = fixture_duel(&pool, "running").await;
        assert_card(
            &get(&state, &format!("/og/games/{running}.png")).await,
            LIVE_MAX_AGE,
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn spoiler_free_game_cards_give_nothing_away(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _) = fixture_duel(&pool, "finished").await;

        let card = load_game_card(&state, game_id, false)
            .await
            .unwrap()
            .unwrap();
        assert!(!card.spoilers);
        // Join order, not placement order; no placements; no turn count.
        let names: Vec<&str> = card.roster.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Early Bird", "Late Comer"]);
        assert!(card.roster.iter().all(|r| r.placement.is_none()));
        assert_eq!(card.turns, None);
        // The turn-2 board, with everyone alive, in what they wore that game.
        assert_eq!(card.board.snakes[0].body, vec![(1, 3), (1, 2)]);
        assert!(card.board.snakes.iter().all(|s| !s.eliminated));
        assert_eq!(card.roster[0].head.map(|h| h.slug), Some("smile"));
        assert_eq!(
            card.roster[0].color,
            crate::og::canvas::rgb(0x3d, 0xdb, 0xa0)
        );

        // A spoiler request on an unfinished game stays spoiler-free.
        sqlx::query("UPDATE games SET status = 'running' WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let running = load_game_card(&state, game_id, true)
            .await
            .unwrap()
            .unwrap();
        assert!(!running.spoilers);
        assert!(running.roster.iter().all(|r| r.placement.is_none()));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_page_titles_list_snakes_in_join_order(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _) = fixture_duel(&pool, "finished").await;
        let page = get(&state, &format!("/games/{game_id}")).await;
        let html = String::from_utf8(page.body).unwrap();
        // The winner ("Late Comer") joined second, so it isn't named first.
        let title = html
            .split("<title>")
            .nth(1)
            .and_then(|rest| rest.split("</title>").next());
        assert_eq!(title, Some("Early Bird vs Late Comer — Battlesnake"));
        assert!(html.contains(r#"<meta property="og:title" content="Early Bird vs Late Comer">"#));
        assert!(html.contains(&format!(
            r#"<meta property="og:image" content="http://localhost:3000/og/games/{game_id}.png">"#
        )));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn spoiler_game_cards_show_the_result(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _) = fixture_duel(&pool, "finished").await;

        let card = load_game_card(&state, game_id, true)
            .await
            .unwrap()
            .unwrap();
        assert!(card.spoilers);
        let roster: Vec<(&str, Option<i32>)> = card
            .roster
            .iter()
            .map(|r| (r.name.as_str(), r.placement))
            .collect();
        assert_eq!(roster, [("Late Comer", Some(1)), ("Early Bird", Some(2))]);
        assert_eq!(card.turns, Some(4));
        // The final board, with the loser faded.
        assert_eq!(card.board.snakes[0].body, vec![(1, 5), (1, 4)]);
        assert!(card.board.snakes[0].eliminated);
        assert!(!card.board.snakes[1].eliminated);

        assert!(
            load_game_card(&state, Uuid::new_v4(), true)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn png_ids_need_the_extension_and_a_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(png_id(&format!("{id}.png")).ok(), Some(id));
        assert!(png_id(&id.to_string()).is_err());
        assert!(png_id(&format!("{id}.jpg")).is_err());
        assert!(png_id("nope.png").is_err());
    }

    #[test]
    fn game_card_paths_carry_the_spoiler_opt_in() {
        let id = Uuid::nil();
        assert_eq!(
            game_card_path(id, false),
            "/og/games/00000000-0000-0000-0000-000000000000.png"
        );
        assert_eq!(
            game_card_path(id, true),
            "/og/games/00000000-0000-0000-0000-000000000000.png?showSpoilers=true"
        );
    }

    fn frame(json: serde_json::Value) -> EngineGameFrame {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn spoiler_free_boards_hide_deaths() {
        let f = frame(serde_json::json!({
            "Turn": 2,
            "Snakes": [
                {"ID": "a", "Name": "A", "Body": [{"X": 1, "Y": 3}, {"X": 1, "Y": 2}],
                 "Color": "#ff0000", "HeadType": "bendr", "TailType": "bolt",
                 "Death": {"Cause": "wall-collision", "Turn": 2}},
                {"ID": "b", "Name": "B", "Body": [{"X": 5, "Y": 5}], "Color": "nope"}
            ],
            "Food": [{"X": 0, "Y": 0}],
            "Hazards": [{"X": 4, "Y": 4}]
        }));
        let hidden = board(11, 11, Some(&f), false);
        assert!(hidden.snakes.iter().all(|s| !s.eliminated));
        let shown = board(11, 11, Some(&f), true);
        assert!(shown.snakes[0].eliminated && !shown.snakes[1].eliminated);

        assert_eq!(shown.snakes[0].body, vec![(1, 3), (1, 2)]);
        assert_eq!(shown.snakes[0].head.map(|h| h.slug), Some("bendr"));
        assert_eq!(shown.snakes[0].tail.map(|t| t.slug), Some("bolt"));
        assert_eq!(shown.snakes[1].head.map(|h| h.slug), Some("default"));
        assert_eq!(shown.food, vec![(0, 0)]);
        assert_eq!(shown.hazards, vec![(4, 4)]);

        let empty = board(7, 7, None, true);
        assert!(empty.snakes.is_empty());
        assert_eq!((empty.width, empty.height), (7, 7));
    }
}
