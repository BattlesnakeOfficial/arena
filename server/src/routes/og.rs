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
        battlesnake,
        game::{GameStatus, get_game_by_id},
        game_battlesnake::{
            GameBattlesnakeWithDetails, get_battlesnakes_by_game_id,
            get_game_stats_for_battlesnake, join_order_key,
        },
        global_ranking::get_global_player_score,
        leaderboard::{self, LeaderboardSort},
        tournament::{self, TournamentStatus},
        turn::{get_latest_frame, get_turn_frames_page},
        user::{User, get_user_by_id},
    },
    og::{
        board::{BoardArt, BoardSnake, shape},
        canvas::snake_color,
        cards::{
            self, Entrant, GameCard, LadderRow, LeaderboardCard, ProfileCard, RosterSnake, Stat,
            TournamentCard, lineup_board, portrait_board,
        },
    },
    routes::game::view::{ViewGameParams, comma_separate},
    state::AppState,
};
use tiny_skia::Color;

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

pub fn snake_card_path(battlesnake_id: Uuid) -> String {
    format!("/og/battlesnakes/{battlesnake_id}.png")
}

pub fn entry_card_path(leaderboard_id: Uuid, entry_id: Uuid) -> String {
    format!("/og/leaderboards/{leaderboard_id}/entries/{entry_id}.png")
}

pub fn leaderboard_card_path(leaderboard_id: Uuid) -> String {
    format!("/og/leaderboards/{leaderboard_id}.png")
}

pub fn player_card_path(user_id: Uuid) -> String {
    format!("/og/users/{user_id}.png")
}

pub fn tournament_card_path(tournament_id: Uuid) -> String {
    format!("/og/tournaments/{tournament_id}.png")
}

/// Ratings, records and ladders move every game; an hour is plenty fresh
/// for a link preview.
const HOUR: u32 = 60 * 60;

fn owner_name(owner: Option<&User>) -> String {
    owner.map_or_else(|| "Unknown".to_string(), |o| o.public_name().to_string())
}

fn rating(display_score: f64) -> String {
    format!("{display_score:.1}")
}

/// GET /og/battlesnakes/{id}.png
pub async fn snake_card(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> ServerResult<Response, StatusCode> {
    let battlesnake_id = png_id(&file)?;
    let card = load_snake_card(&state, battlesnake_id)
        .await?
        .ok_or_else(not_found)?;
    let bytes = render(move || cards::profile_card(&card)).await?;
    Ok(png(bytes, HOUR))
}

async fn load_snake_card(
    state: &AppState,
    battlesnake_id: Uuid,
) -> cja::Result<Option<ProfileCard>> {
    let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake for social card")?
    else {
        return Ok(None);
    };
    let owner = get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to get snake owner for social card")?;
    let stats = get_game_stats_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get snake stats for social card")?;

    // The snake's best current rank across the ladders it's on.
    let mut best: Option<(i64, String)> = None;
    let entries = leaderboard::get_entries_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get snake leaderboard entries for social card")?;
    for entry in entries.into_iter().filter(|e| e.disabled_at.is_none()) {
        let rank = leaderboard::get_rank_for_entry(
            &state.db,
            entry.leaderboard_id,
            entry.display_score,
            entry.games_played,
        )
        .await
        .wrap_err("Failed to rank snake for social card")?;
        if let Some(rank) = rank
            && best.as_ref().is_none_or(|(b, _)| rank < *b)
        {
            best = Some((rank, entry.leaderboard_name));
        }
    }

    let color = snake_color(&snake.color);
    let win_rate = if stats.finished_games > 0 {
        format!("{:.1}%", stats.win_rate)
    } else {
        "—".to_string()
    };
    Ok(Some(ProfileCard {
        kicker: "Battlesnake".to_string(),
        title: snake.name,
        subtitle: format!("by {}", owner_name(owner.as_ref())),
        avatar: Some((shape(AssetKind::Head, &snake.head), color)),
        stats: vec![
            Stat::new("Games", comma_separate(stats.total_games)),
            Stat::new("Wins", comma_separate(stats.wins)),
            Stat::new("Win rate", win_rate),
        ],
        highlight: best.map(|(rank, name)| format!("#{rank} on {name}")),
        board: portrait_board(color, &snake.head, &snake.tail),
    }))
}

/// GET /og/leaderboards/{id}/entries/{entry_id}.png
pub async fn entry_card(
    State(state): State<AppState>,
    Path((leaderboard_id, file)): Path<(String, String)>,
) -> ServerResult<Response, StatusCode> {
    let leaderboard_id: Uuid = leaderboard_id.parse().map_err(|_| not_found())?;
    let entry_id = png_id(&file)?;
    let card = load_entry_card(&state, leaderboard_id, entry_id)
        .await?
        .ok_or_else(not_found)?;
    let bytes = render(move || cards::profile_card(&card)).await?;
    Ok(png(bytes, HOUR))
}

async fn load_entry_card(
    state: &AppState,
    leaderboard_id: Uuid,
    entry_id: Uuid,
) -> cja::Result<Option<ProfileCard>> {
    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to get leaderboard for social card")?
    else {
        return Ok(None);
    };
    let Some(entry) = leaderboard::get_entry_by_id(&state.db, entry_id)
        .await
        .wrap_err("Failed to get leaderboard entry for social card")?
        .filter(|e| e.leaderboard_id == leaderboard_id)
    else {
        return Ok(None);
    };
    let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, entry.battlesnake_id)
        .await
        .wrap_err("Failed to get entry snake for social card")?
    else {
        return Ok(None);
    };
    let owner = get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to get entry owner for social card")?;
    let rank = leaderboard::get_rank_for_entry(
        &state.db,
        leaderboard_id,
        entry.display_score,
        entry.games_played,
    )
    .await
    .wrap_err("Failed to rank entry for social card")?;

    let color = snake_color(&snake.color);
    Ok(Some(ProfileCard {
        kicker: format!("{} leaderboard", lb.name),
        title: snake.name,
        subtitle: format!("by {}", owner_name(owner.as_ref())),
        avatar: Some((shape(AssetKind::Head, &snake.head), color)),
        stats: vec![
            Stat::new(
                "Rank",
                rank.map_or_else(|| "—".to_string(), |r| format!("#{r}")),
            ),
            Stat::new("Rating", rating(entry.display_score)),
            Stat::new("Games", comma_separate(entry.games_played)),
        ],
        highlight: None,
        board: portrait_board(color, &snake.head, &snake.tail),
    }))
}

/// GET /og/users/{user_id}.png
pub async fn player_card(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> ServerResult<Response, StatusCode> {
    let user_id = png_id(&file)?;
    let card = load_player_card(&state, user_id)
        .await?
        .ok_or_else(not_found)?;
    let bytes = render(move || cards::profile_card(&card)).await?;
    Ok(png(bytes, HOUR))
}

async fn load_player_card(state: &AppState, user_id: Uuid) -> cja::Result<Option<ProfileCard>> {
    let Some(user) = get_user_by_id(&state.db, user_id)
        .await
        .wrap_err("Failed to get player for social card")?
    else {
        return Ok(None);
    };
    let snakes = battlesnake::get_battlesnakes_by_user_id(&state.db, user_id)
        .await
        .wrap_err("Failed to get player snakes for social card")?;
    let score = get_global_player_score(&state.db, user_id)
        .await
        .wrap_err("Failed to get player rating for social card")?;

    let lineup: Vec<(Color, &str, &str)> = snakes
        .iter()
        .map(|s| (snake_color(&s.color), s.head.as_str(), s.tail.as_str()))
        .collect();
    Ok(Some(ProfileCard {
        kicker: "Player".to_string(),
        title: user.public_name().to_string(),
        subtitle: format!("@{}", user.github_login),
        avatar: None,
        stats: vec![
            Stat::new("Snakes", comma_separate(snakes.len() as i64)),
            Stat::new(
                "Rating",
                score
                    .as_ref()
                    .map_or_else(|| "Unranked".to_string(), |s| s.display_total()),
            ),
            Stat::new(
                "Ladders",
                score
                    .map_or(0, |s| s.contributing_leaderboards())
                    .to_string(),
            ),
        ],
        highlight: None,
        board: lineup_board(&lineup),
    }))
}

/// GET /og/leaderboards/{id}.png
pub async fn leaderboard_card(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> ServerResult<Response, StatusCode> {
    let leaderboard_id = png_id(&file)?;
    let card = load_leaderboard_card(&state, leaderboard_id)
        .await?
        .ok_or_else(not_found)?;
    let bytes = render(move || cards::leaderboard_card(&card)).await?;
    Ok(png(bytes, HOUR))
}

/// Rows on the leaderboard card's ladder.
const LADDER_ROWS: i64 = 5;

async fn load_leaderboard_card(
    state: &AppState,
    leaderboard_id: Uuid,
) -> cja::Result<Option<LeaderboardCard>> {
    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to get leaderboard for social card")?
    else {
        return Ok(None);
    };
    let ranked = leaderboard::count_ranked_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to count ranked entries for social card")?;
    let status = leaderboard::get_leaderboard_status(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to get leaderboard status for social card")?;
    let top = leaderboard::get_ranked_entries_paginated(
        &state.db,
        leaderboard_id,
        0,
        LADDER_ROWS,
        LeaderboardSort::Rating,
    )
    .await
    .wrap_err("Failed to get top entries for social card")?;
    let ids: Vec<Uuid> = top.iter().map(|e| e.battlesnake_id).collect();
    let looks = battlesnake::get_cosmetics_by_ids(&state.db, &ids)
        .await
        .wrap_err("Failed to get top snakes' cosmetics for social card")?;

    Ok(Some(LeaderboardCard {
        kicker: format!("Leaderboard · {} · {}", lb.game_type, lb.board_size),
        title: lb.name,
        stats: vec![
            Stat::new("Ranked snakes", comma_separate(ranked)),
            Stat::new("Games", comma_separate(status.total_games)),
            Stat::new("Live now", comma_separate(status.games_in_progress)),
        ],
        rows: top
            .into_iter()
            .zip(1..)
            .map(|(entry, rank)| LadderRow {
                rank,
                rating: rating(entry.display_score),
                snake: RosterSnake {
                    color: snake_color(&entry.snake_color),
                    head: shape(
                        AssetKind::Head,
                        looks
                            .get(&entry.battlesnake_id)
                            .map_or("", |l| l.head.as_str()),
                    ),
                    name: entry.snake_name,
                    owner: entry.owner_name,
                    placement: None,
                },
            })
            .collect(),
    }))
}

/// GET /og/tournaments/{id}.png — 404 for participants-only tournaments,
/// which only their participants can see.
pub async fn tournament_card(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> ServerResult<Response, StatusCode> {
    let tournament_id = png_id(&file)?;
    let card = load_tournament_card(&state, tournament_id)
        .await?
        .ok_or_else(not_found)?;
    let bytes = render(move || cards::tournament_card(&card)).await?;
    Ok(png(bytes, HOUR))
}

async fn load_tournament_card(
    state: &AppState,
    tournament_id: Uuid,
) -> cja::Result<Option<TournamentCard>> {
    let Some(t) = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to get tournament for social card")?
    else {
        return Ok(None);
    };
    let registrations = tournament::get_registrations_with_details(&state.db, tournament_id)
        .await
        .wrap_err("Failed to get tournament registrations for social card")?;
    // Scrapers are anonymous: show exactly what a logged-out visitor sees.
    let participants: Vec<Uuid> = registrations.iter().map(|r| r.user_id).collect();
    if !crate::routes::tournament::can_view(&t, None, &participants) {
        return Ok(None);
    }

    let matches = tournament::get_matches_for_tournament(&state.db, tournament_id)
        .await
        .wrap_err("Failed to get tournament matches for social card")?;
    let total_rounds = matches.iter().map(|m| m.round).max().unwrap_or(0);
    // The champion: the winner of the final, once the tournament is over.
    let champion_id = (t.status == TournamentStatus::Completed)
        .then(|| matches.iter().find(|m| m.round == total_rounds)?.winner_id)
        .flatten();

    let ids: Vec<Uuid> = registrations.iter().map(|r| r.battlesnake_id).collect();
    let looks = battlesnake::get_cosmetics_by_ids(&state.db, &ids)
        .await
        .wrap_err("Failed to get entrants' cosmetics for social card")?;
    let head = |id: &Uuid| {
        shape(
            AssetKind::Head,
            looks.get(id).map_or("", |l| l.head.as_str()),
        )
    };

    let status = match t.status {
        TournamentStatus::Created => ("COMING SOON".to_string(), false),
        TournamentStatus::Registration => ("REGISTRATION OPEN".to_string(), false),
        TournamentStatus::InProgress if total_rounds > 0 => (
            format!("ROUND {} OF {total_rounds}", t.current_round.max(1)),
            true,
        ),
        TournamentStatus::InProgress => ("LIVE".to_string(), true),
        TournamentStatus::Completed => ("COMPLETE".to_string(), false),
        TournamentStatus::Canceled => ("CANCELED".to_string(), false),
    };
    let (width, height) = t.board_size.dimensions();
    let snakes = registrations.len();
    Ok(Some(TournamentCard {
        status,
        kicker: format!("Tournament · {} · {width}×{height}", t.game_type.as_str()),
        subtitle: format!(
            "{} {} · {}",
            comma_separate(snakes as i64),
            if snakes == 1 { "snake" } else { "snakes" },
            t.match_style.label()
        ),
        title: t.name,
        champion: registrations
            .iter()
            .find(|r| Some(r.battlesnake_id) == champion_id)
            .map(|r| RosterSnake {
                name: r.snake_name.clone(),
                owner: r.owner_name.clone(),
                color: snake_color(&r.snake_color),
                head: head(&r.battlesnake_id),
                placement: Some(1),
            }),
        entrants: registrations
            .iter()
            .map(|r| Entrant {
                color: snake_color(&r.snake_color),
                head: head(&r.battlesnake_id),
                champion: Some(r.battlesnake_id) == champion_id,
            })
            .collect(),
    }))
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

    /// A player ("OG Player") with one ranked snake ("Card Shark": 12 games on
    /// "OG Ladder") that won a one-match tournament ("OG Cup").
    struct World {
        user_id: Uuid,
        snake_id: Uuid,
        leaderboard_id: Uuid,
        entry_id: Uuid,
        tournament_id: Uuid,
    }

    async fn fixture_world(pool: &PgPool) -> World {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
             VALUES (888001, 'og-player', '', 'OG Player') RETURNING user_id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, color, head, tail)
             VALUES ($1, 'Card Shark', 'https://example.com', '#3ddba0', 'bendr', 'bolt')
             RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('OG Ladder') RETURNING leaderboard_id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let entry_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played, display_score)
             VALUES ($1, $2, 12, 33.3) RETURNING leaderboard_entry_id",
        )
        .bind(leaderboard_id)
        .bind(snake_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let tournament_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournaments (name, user_id, status) VALUES ('OG Cup', $1, 'completed')
             RETURNING tournament_id",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tournament_registrations (tournament_id, battlesnake_id, user_id, seed)
             VALUES ($1, $2, $3, 1)",
        )
        .bind(tournament_id)
        .bind(snake_id)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tournament_matches
                 (tournament_id, round, position, visual_column, visual_row, winner_id)
             VALUES ($1, 1, 0, 0, 0, $2)",
        )
        .bind(tournament_id)
        .bind(snake_id)
        .execute(pool)
        .await
        .unwrap();
        World {
            user_id,
            snake_id,
            leaderboard_id,
            entry_id,
            tournament_id,
        }
    }

    fn stats(card: &ProfileCard) -> Vec<(&str, &str)> {
        card.stats
            .iter()
            .map(|s| (s.label.as_str(), s.value.as_str()))
            .collect()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn serves_entity_cards_and_404s_unknown_ones(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let w = fixture_world(&pool).await;
        for path in [
            snake_card_path(w.snake_id),
            entry_card_path(w.leaderboard_id, w.entry_id),
            leaderboard_card_path(w.leaderboard_id),
            player_card_path(w.user_id),
            tournament_card_path(w.tournament_id),
        ] {
            assert_card(&get(&state, &path).await, HOUR);
        }

        let nobody = Uuid::new_v4();
        for path in [
            snake_card_path(nobody),
            entry_card_path(w.leaderboard_id, nobody),
            // A real entry under the wrong leaderboard.
            entry_card_path(nobody, w.entry_id),
            format!("/og/leaderboards/not-a-uuid/entries/{}.png", w.entry_id),
            leaderboard_card_path(nobody),
            player_card_path(nobody),
            tournament_card_path(nobody),
            format!("/og/battlesnakes/{}", w.snake_id),
        ] {
            assert_eq!(
                get(&state, &path).await.status,
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn profile_cards_carry_the_record(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let w = fixture_world(&pool).await;

        let snake = load_snake_card(&state, w.snake_id).await.unwrap().unwrap();
        assert_eq!(snake.title, "Card Shark");
        assert_eq!(snake.subtitle, "by OG Player");
        assert_eq!(snake.highlight.as_deref(), Some("#1 on OG Ladder"));
        assert_eq!(
            stats(&snake),
            [("Games", "0"), ("Wins", "0"), ("Win rate", "—")]
        );
        assert_eq!(
            snake.avatar.and_then(|(h, _)| h).map(|h| h.slug),
            Some("bendr")
        );

        let entry = load_entry_card(&state, w.leaderboard_id, w.entry_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.kicker, "OG Ladder leaderboard");
        assert_eq!(
            stats(&entry),
            [("Rank", "#1"), ("Rating", "33.3"), ("Games", "12")]
        );

        let player = load_player_card(&state, w.user_id).await.unwrap().unwrap();
        assert_eq!(
            (player.title.as_str(), player.subtitle.as_str()),
            ("OG Player", "@og-player")
        );
        assert_eq!(player.stats[0].value, "1");
        assert_eq!(player.board.snakes.len(), 1);
        assert_eq!(player.board.snakes[0].head.map(|h| h.slug), Some("bendr"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_and_tournament_cards_list_their_snakes(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let w = fixture_world(&pool).await;

        let ladder = load_leaderboard_card(&state, w.leaderboard_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ladder.title, "OG Ladder");
        assert_eq!(ladder.stats[0].value, "1");
        let row = &ladder.rows[0];
        assert_eq!((row.rank, row.rating.as_str()), (1, "33.3"));
        assert_eq!(row.snake.name, "Card Shark");
        assert_eq!(row.snake.owner, "OG Player");
        assert_eq!(row.snake.head.map(|h| h.slug), Some("bendr"));

        let cup = load_tournament_card(&state, w.tournament_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cup.status, ("COMPLETE".to_string(), false));
        assert_eq!(cup.subtitle, "1 snake · Single game");
        assert_eq!(
            cup.champion.as_ref().map(|c| c.name.as_str()),
            Some("Card Shark")
        );
        assert_eq!(cup.entrants.len(), 1);
        assert!(cup.entrants[0].champion);

        // Participants-only tournaments are invisible to anonymous scrapers.
        sqlx::query(
            "UPDATE tournaments SET visibility = 'participants_only' WHERE tournament_id = $1",
        )
        .bind(w.tournament_id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            load_tournament_card(&state, w.tournament_id)
                .await
                .unwrap()
                .is_none()
        );
        let reply = get(&state, &tournament_card_path(w.tournament_id)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn entity_pages_point_at_their_cards(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let w = fixture_world(&pool).await;
        for (page, card) in [
            (
                format!("/battlesnakes/{}/profile", w.snake_id),
                snake_card_path(w.snake_id),
            ),
            (
                format!("/leaderboards/{}/entries/{}", w.leaderboard_id, w.entry_id),
                entry_card_path(w.leaderboard_id, w.entry_id),
            ),
            (
                format!("/leaderboards/{}", w.leaderboard_id),
                leaderboard_card_path(w.leaderboard_id),
            ),
            ("/users/og-player".to_string(), player_card_path(w.user_id)),
            (
                format!("/tournaments/{}", w.tournament_id),
                tournament_card_path(w.tournament_id),
            ),
        ] {
            let reply = get(&state, &page).await;
            assert_eq!(reply.status, StatusCode::OK, "{page}");
            let html = String::from_utf8(reply.body).unwrap();
            let tag =
                format!(r#"<meta property="og:image" content="http://localhost:3000{card}">"#);
            assert!(html.contains(&tag), "{page} should point at {card}");
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn private_tournament_pages_keep_the_default_card(pool: PgPool) {
        use crate::routes::test_support::{
            create_user_session, session_user_id, signed_session_cookie,
        };
        let state = AppState::test_from_pool(pool.clone());
        let w = fixture_world(&pool).await;
        // The owner can see their participants-only tournament, but a scraper
        // can't, so the page must not link a card that 404s.
        let session_id = create_user_session(&pool, 888002, false).await;
        sqlx::query(
            "UPDATE tournaments SET visibility = 'participants_only', user_id = $2
             WHERE tournament_id = $1",
        )
        .bind(w.tournament_id)
        .bind(session_user_id(&pool, session_id).await)
        .execute(&pool)
        .await
        .unwrap();
        let cookie = format!(
            "{}={}",
            crate::models::session::SESSION_COOKIE_NAME,
            signed_session_cookie(&state, session_id)
        );
        let response = crate::routes::routes(state.clone())
            .layer(tower_cookies::CookieManagerLayer::new())
            .oneshot(
                Request::get(format!("/tournaments/{}", w.tournament_id))
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains(
            r#"<meta property="og:image" content="http://localhost:3000/og/default.png">"#
        ));
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
