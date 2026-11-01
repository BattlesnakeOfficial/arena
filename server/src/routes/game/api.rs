use std::collections::BTreeMap;

use crate::archive::{ArchiveReader, ArchiveRowKind, ArchivedGame, LoadArchiveError};
use crate::watched_games::{Readiness, WatchedGameUpdate};
use axum::{
    Json,
    extract::{
        Path, Query, RawQuery, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::IntoResponse,
};
use color_eyre::eyre::Context as _;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    errors::ServerResult,
    models::game::{
        GameRow, GameStatus, GameType, get_game_by_id, get_game_row_by_any_id, get_game_source,
    },
    models::game_battlesnake::get_battlesnakes_by_game_id,
    models::turn::{get_latest_frame, get_turn_frames_page, get_turns_by_game_id, has_frames},
    state::AppState,
};

/// Snake-ID → owner public name map for filling in frame `Author` fields.
///
/// Frames persisted before authors were threaded through the game runner have
/// `Author: ""`, which the board viewer's scoreboard renders as a dangling
/// "by ". Frame snake IDs are `game_battlesnake_id` strings, so this joins
/// back to the owner at serve time. Games without `game_battlesnakes` rows
/// (archived imports) yield an empty map and enrichment is a no-op.
async fn frame_author_map(
    db: &sqlx::PgPool,
    game_id: Uuid,
) -> std::collections::HashMap<String, String> {
    match get_battlesnakes_by_game_id(db, game_id).await {
        Ok(snakes) => snakes
            .into_iter()
            .map(|bs| (bs.game_battlesnake_id.to_string(), bs.owner_name))
            .collect(),
        Err(e) => {
            // Author enrichment is cosmetic; never fail a frames request over it.
            tracing::warn!(error = ?e, %game_id, "Failed to load authors for frame enrichment");
            std::collections::HashMap::new()
        }
    }
}

/// Fill in missing/empty `Author` fields on a persisted frame's `Snakes`.
fn fill_frame_authors(
    frame: &mut serde_json::Value,
    authors: &std::collections::HashMap<String, String>,
) {
    if authors.is_empty() {
        return;
    }
    let Some(snakes) = frame.get_mut("Snakes").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for snake in snakes {
        let has_author = snake
            .get("Author")
            .and_then(|a| a.as_str())
            .is_some_and(|a| !a.is_empty());
        if has_author {
            continue;
        }
        let Some(author) = snake
            .get("ID")
            .and_then(|id| id.as_str())
            .and_then(|id| authors.get(id))
        else {
            continue;
        };
        snake["Author"] = serde_json::Value::String(author.clone());
    }
}

/// Everything a persisted frame needs before it is served publicly: owner
/// names filled in, moderated shouts stripped, and the legacy engine's public
/// frame shape (see `normalize_public_frame`).
pub(crate) fn prepare_public_frame(
    frame: &mut serde_json::Value,
    authors: &std::collections::HashMap<String, String>,
    suppressed: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) {
    fill_frame_authors(frame, authors);
    crate::moderation::shouts::strip_suppressed_shouts(frame, suppressed);
    crate::engine::frame::normalize_public_frame(frame, crate::engine::MOVE_TIMEOUT_MS);
}

/// `GET /api/games/{id}`: the legacy engine's `GET /games/{id}` shape,
/// `{Game, LastFrame}` (sampled live from engine.battlesnake.com). Read by
/// the board viewer (Width/Height), the GIF exporter, and third-party tools
/// written against the engine.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardViewerGameResponse {
    pub game: BoardViewerGame,
    /// Latest persisted frame; omitted before turn 0 exists, like the engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_frame: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardViewerGame {
    /// Game ID — required by the GIF exporter, which echoes it back into
    /// the frames requests it makes.
    #[serde(rename = "ID")]
    pub id: String,
    /// Legacy-engine status string ("stopped" | "running" | "complete" |
    /// "error" — the engine's four values; see [`engine_status`]).
    pub status: String,
    /// Lossless Arena status, additive to the legacy engine contract.
    pub arena_status: String,
    pub width: u32,
    pub height: u32,
    /// Rules settings as strings keyed by the Go rules param names
    /// (`name`, `foodSpawnChance`, `minimumFood`, `damagePerTurn`,
    /// `shrinkEveryNTurns`), like the engine's `Game.Ruleset` map.
    pub ruleset: BTreeMap<String, String>,
    pub ruleset_name: String,
    pub rules_stages: Vec<String>,
    pub map: String,
    pub source: String,
    pub snake_timeout: i64,
    pub max_turns: i32,
    /// Pre-placed spawns; arena games never have any.
    pub food_spawns: Vec<serde_json::Value>,
    pub hazard_spawns: Vec<serde_json::Value>,
    /// Unix microseconds as a string, like the engine.
    pub created: String,
}

/// The legacy engine's `Game.Ruleset`, `RulesetName`, `Map`, and
/// `RulesStages` for a game. Settings come from `engine::mode_rules` and the
/// public ruleset/map pair from `wire::wire_ruleset_and_map`, the same
/// sources as the snake request payload, so the API and what snakes are told
/// can't drift apart.
fn engine_rules(
    game_type: GameType,
    game_id: Uuid,
) -> (BTreeMap<String, String>, String, String, Vec<String>) {
    let (internal, settings, royale) = crate::engine::mode_rules(game_type, game_id);
    let (ruleset_name, map) = crate::wire::wire_ruleset_and_map(internal);

    let mut ruleset = BTreeMap::from([
        ("name".to_string(), ruleset_name.to_string()),
        (
            "foodSpawnChance".to_string(),
            settings.food_spawn_chance.to_string(),
        ),
        ("minimumFood".to_string(), settings.minimum_food.to_string()),
        (
            "damagePerTurn".to_string(),
            settings.hazard_damage_per_turn.to_string(),
        ),
    ]);
    if let Some(royale) = royale {
        ruleset.insert(
            "shrinkEveryNTurns".to_string(),
            royale.shrink_every_n_turns.to_string(),
        );
    }

    // Stage pipelines from play `core/game_stages.py`: the game-over stage
    // depends on the snake count (Solo is the single-snake mode), and only
    // Constrictor adds a stage to the standard pipeline; Royale's shrinking
    // and Snail Mode's trails came from their maps.
    let game_over = if internal == "solo" {
        "game_over.solo_snake"
    } else {
        "game_over.standard"
    };
    let mut stages = vec![
        game_over,
        "movement.standard",
        "starvation.standard",
        "hazard_damage.standard",
        "feed_snakes.standard",
    ];
    if ruleset_name == "constrictor" {
        stages.push("modify_snakes.always_grow");
    }
    stages.push("elimination.standard");

    (
        ruleset,
        ruleset_name.to_string(),
        map.to_string(),
        stages.into_iter().map(String::from).collect(),
    )
}

/// Map arena's game status to the legacy engine's status strings. The
/// engine has exactly four values: `stopped` (created, not running),
/// `running`, `complete`, and `error` (failed). These strings are
/// serve-time only — never written to SQL or stored anywhere.
fn engine_status(status: GameStatus) -> &'static str {
    match status {
        GameStatus::Waiting => "stopped",
        GameStatus::Running => "running",
        GameStatus::Finished => "complete",
        GameStatus::Failed => "error",
    }
}

/// The engine's `game_end` websocket payload: the terminal game object
/// (live capture 2026-10-31, `engine_ws_summary.json`). Field-for-field
/// the same values as `GET /games/{id}`'s `Game` — hence built from
/// [`BoardViewerGame`], never independently. No `ArenaStatus` here:
/// the engine's `Data` carries exactly these 10 keys.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GameEndGame {
    #[serde(rename = "ID")]
    id: String,
    status: String,
    width: u32,
    height: u32,
    ruleset: BTreeMap<String, String>,
    snake_timeout: i64,
    source: String,
    ruleset_name: String,
    rules_stages: Vec<String>,
    map: String,
}

impl From<&BoardViewerGame> for GameEndGame {
    fn from(game: &BoardViewerGame) -> Self {
        GameEndGame {
            id: game.id.clone(),
            status: game.status.clone(),
            width: game.width,
            height: game.height,
            ruleset: game.ruleset.clone(),
            snake_timeout: game.snake_timeout,
            source: game.source.clone(),
            ruleset_name: game.ruleset_name.clone(),
            rules_stages: game.rules_stages.clone(),
            map: game.map.clone(),
        }
    }
}

fn archive_row_kind(record: &GameRow) -> ArchiveRowKind {
    if record.engine_game_id.is_some() {
        ArchiveRowKind::Play
    } else {
        ArchiveRowKind::Native
    }
}

/// True when this game's frames must come from the archive: the row has an
/// archive pointer, is terminal, and has no renderable frame in `turns`
/// (imported Play stubs, or pruned native games — DEV-1981). The status
/// guard keeps live games (waiting/running) on the DB path and the live
/// websocket even if they carry a stale `gcs_path`. The `turns` probe runs
/// only for archived-looking rows, so live games pay nothing.
async fn serves_from_archive(db: &sqlx::PgPool, record: &GameRow) -> cja::Result<bool> {
    if record.gcs_path.is_none()
        || !matches!(
            record.game.status,
            GameStatus::Finished | GameStatus::Failed
        )
    {
        return Ok(false);
    }
    Ok(!has_frames(db, record.game.game_id).await?)
}

/// Load the archived game for an HTTP endpoint: missing object → 404
/// (ERROR log happens inside the reader), storage/decode failure → 500.
async fn load_archived_for_http(
    state: &AppState,
    record: &GameRow,
) -> ServerResult<std::sync::Arc<ArchivedGame>, StatusCode> {
    let path = record.gcs_path.clone().unwrap_or_default();
    match state
        .archive
        .load(record.game.game_id, &path, archive_row_kind(record))
        .await
    {
        Ok(archived) => Ok(archived),
        Err(LoadArchiveError::ObjectMissing { .. }) => Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!(
                "Archive object missing for game {}: {path}",
                record.game.game_id
            ),
            StatusCode::NOT_FOUND,
        )),
        Err(LoadArchiveError::Failed(e)) => Err(e.into()), // → 500
    }
}

/// Engine-API game info built from the arena `games` row — the DB path's
/// construction (extracted verbatim from `get_game_info`). Both the DB
/// path and the native archive path (DEV-1980 objects + DEV-1981 pruning)
/// call this, so a native game's `Game` cannot drift across the prune.
/// `source` is fetched by the caller (it is a DB round trip).
fn board_viewer_game_from_record(
    game: &crate::models::game::Game,
    source: &crate::engine::GameSource,
) -> BoardViewerGame {
    let (width, height) = game.board_size.dimensions();
    let (ruleset, ruleset_name, map, rules_stages) =
        engine_rules(game.game_type.clone(), game.game_id);
    BoardViewerGame {
        id: game.game_id.to_string(),
        status: engine_status(game.status).to_string(),
        arena_status: game.status.as_str().to_string(),
        width,
        height,
        ruleset,
        ruleset_name,
        rules_stages,
        map,
        source: source.as_str().to_string(),
        snake_timeout: crate::engine::MOVE_TIMEOUT_MS,
        max_turns: crate::engine::MAX_TURNS,
        food_spawns: Vec::new(),
        hazard_spawns: Vec::new(),
        created: game.created_at.timestamp_micros().to_string(),
    }
}

/// Engine-API game info built from the archived engine game — Play rows
/// only (`engine_game_id IS NOT NULL`): its real
/// Width/Height/Ruleset/RulesetName/Map/Source/Created/SnakeTimeout/
/// MaxTurns, NOT arena's mode-rule defaults. Native rows use
/// [`board_viewer_game_from_record`] instead and ignore this header.
/// Status comes from the arena row (never the archived engine's own status
/// string — a stray archived `running` would put the board into live mode).
fn board_viewer_game_from_archived(
    record_game_id: Uuid,
    arena_status: GameStatus,
    archived: &crate::engine_models::EngineGame,
) -> BoardViewerGame {
    BoardViewerGame {
        id: record_game_id.to_string(),
        status: engine_status(arena_status).to_string(),
        arena_status: arena_status.as_str().to_string(),
        width: archived.width.max(0) as u32,
        height: archived.height.max(0) as u32,
        ruleset: archived
            .ruleset
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ruleset_name: archived.ruleset_name.clone().unwrap_or_default(),
        rules_stages: archived.rules_stages.clone().unwrap_or_default(),
        map: archived.map.clone().unwrap_or_default(),
        source: archived.source.clone().unwrap_or_default(),
        snake_timeout: archived.snake_timeout as i64,
        max_turns: archived.max_turns,
        food_spawns: archived
            .food_spawns
            .iter()
            .map(|p| serde_json::to_value(p).unwrap_or_default())
            .collect(),
        hazard_spawns: archived
            .hazard_spawns
            .iter()
            .map(|p| serde_json::to_value(p).unwrap_or_default())
            .collect(),
        // Unix microseconds as a string, like the engine (and like the DB
        // path's created_at.timestamp_micros()).
        created: archived.created.to_string(),
    }
}

/// The engine-API `Game` object for a resolved row: the archived engine
/// header for archived Play rows, the arena row (plus its source)
/// otherwise — native pruned games included. Single construction site
/// shared by `get_game_info` and the `game_end` websocket payload, so the
/// two can never drift apart.
async fn board_viewer_game_for_record(
    state: &AppState,
    record: &GameRow,
) -> ServerResult<BoardViewerGame, StatusCode> {
    if serves_from_archive(&state.db, record).await? && record.engine_game_id.is_some() {
        let archived = load_archived_for_http(state, record).await?;
        let engine_game = archived
            .parse_game()
            .wrap_err("Failed to parse archived game header")?;
        Ok(board_viewer_game_from_archived(
            record.game.game_id,
            record.game.status,
            &engine_game,
        ))
    } else {
        let source = get_game_source(&state.db, record.game.game_id)
            .await
            .wrap_err("Failed to load game source")?;
        Ok(board_viewer_game_from_record(&record.game, &source))
    }
}

/// `game_end` `Data`: the engine game object. On failure, log and send
/// `{}` — the terminal close sequence must never wedge on the payload.
async fn game_end_payload(state: &AppState, record: &GameRow) -> serde_json::Value {
    match board_viewer_game_for_record(state, record).await {
        Ok(game) => {
            serde_json::to_value(GameEndGame::from(&game)).unwrap_or_else(|_| serde_json::json!({}))
        }
        Err(error) => {
            tracing::error!(game_id = %record.game.game_id,
                error = %format_args!("{error:#}"), "Failed to build game_end payload");
            serde_json::json!({})
        }
    }
}

/// GET /api/games/{id}
/// Returns game info for the Battlesnake board viewer and the GIF exporter
pub async fn get_game_info(
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let record = get_game_row_by_any_id(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch game")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Game not found"),
                StatusCode::NOT_FOUND,
            )
        })?;
    let game_id = record.game.game_id;

    if serves_from_archive(&state.db, &record).await? {
        let archived = load_archived_for_http(&state, &record).await?;
        let authors = frame_author_map(&state.db, game_id).await;
        let suppressed =
            crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &record.game.status)
                .await;
        // One frame parsed + prepped inline: µs-scale.
        let last_frame = archived
            .last_frame()
            .map(|raw| ArchiveReader::prepare_frame(raw, &authors, &suppressed))
            .transpose()
            .wrap_err("Failed to prepare archived last frame")?;
        // Game's source is the ROW, not the object: Play rows serve the
        // archived engine game; native rows serve the arena row through
        // the same constructor the DB path uses. (The second archive load
        // inside is an `ArchiveCache` hit.)
        let game = board_viewer_game_for_record(&state, &record).await?;
        return Ok(Json(BoardViewerGameResponse { game, last_frame }));
    }

    let source = get_game_source(&state.db, game_id)
        .await
        .wrap_err("Failed to load game source")?;

    let last_frame = match get_latest_frame(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch latest frame")?
    {
        Some(mut frame) => {
            let authors = frame_author_map(&state.db, game_id).await;
            let suppressed = crate::moderation::shouts::load_suppressed_set(
                &state.db,
                game_id,
                &record.game.status,
            )
            .await;
            prepare_public_frame(&mut frame, &authors, &suppressed);
            Some(frame)
        }
        None => None,
    };

    Ok(Json(BoardViewerGameResponse {
        game: board_viewer_game_from_record(&record.game, &source),
        last_frame,
    }))
}

/// Default and maximum page size for the frames endpoint.
///
/// The GIF exporter (github.com/BattlesnakeOfficial/exporter) fetches frames
/// in batches of exactly 100 and treats a short page as "no more frames", so
/// the cap must be >= its batch size. Games can have up to ~5000 turns —
/// this cap (applied in SQL) is what keeps the endpoint bounded.
const MAX_FRAMES_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct FramesQuery {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

/// Clamp pagination params to sane, non-negative, bounded values.
/// Matches the legacy engine's semantics: missing limit defaults to the max.
fn clamp_frames_pagination(offset: Option<i64>, limit: Option<i64>) -> (i64, i64) {
    let offset = offset.unwrap_or(0).max(0);
    let limit = limit.unwrap_or(MAX_FRAMES_LIMIT).clamp(0, MAX_FRAMES_LIMIT);
    (offset, limit)
}

/// Engine-compatible frames list envelope: `{"Count", "Frames"}`, PascalCase
/// like the rest of the legacy engine API (verified live against
/// engine.battlesnake.com). `Count` is the number of frames in this page. The
/// GIF exporter's lowercase `count`/`frames` tags still match because Go's
/// JSON decoding is case-insensitive; JS/Python clients are not.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameFramesResponse {
    pub count: usize,
    pub frames: Vec<serde_json::Value>,
}

/// GET /api/games/{id}/frames?offset=&limit=
///
/// Engine-compatible paginated frame history, served from the `turns` table.
/// Each frame is the same PascalCase JSON blob the websocket path streams
/// (`turns.frame_data`, produced by `engine::frame::game_to_frame`).
/// Public: game data is public, matching the legacy engine.
pub async fn get_game_frames(
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
    Query(query): Query<FramesQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // 404 for unknown games, like the legacy engine (the exporter maps this
    // through to its own 404).
    let record = get_game_row_by_any_id(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch game")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Game not found"),
                StatusCode::NOT_FOUND,
            )
        })?;
    let game_id = record.game.game_id;

    if serves_from_archive(&state.db, &record).await? {
        let archived = load_archived_for_http(&state, &record).await?;
        let (offset, limit) = clamp_frames_pagination(query.offset, query.limit);
        let authors = frame_author_map(&state.db, game_id).await;
        let suppressed =
            crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &record.game.status)
                .await;
        // `clamp_frames_pagination` returns non-negative `(i64, i64)`, so
        // the `as usize` casts cannot wrap.
        let frames = state
            .archive
            .prepare_frame_page(
                archived.clone(),
                offset as usize,
                limit as usize,
                authors,
                suppressed,
            )
            .await
            .wrap_err("Failed to prepare archived frames page")?;
        let count = frames.len();
        return Ok(Json(GameFramesResponse { count, frames }));
    }

    let (offset, limit) = clamp_frames_pagination(query.offset, query.limit);

    let turns = get_turn_frames_page(&state.db, game_id, offset, limit)
        .await
        .wrap_err("Failed to fetch turn frames")?;

    let authors = frame_author_map(&state.db, game_id).await;
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &record.game.status)
            .await;
    let mut frames: Vec<serde_json::Value> =
        turns.into_iter().filter_map(|t| t.frame_data).collect();
    for frame in &mut frames {
        prepare_public_frame(frame, &authors, &suppressed);
    }

    Ok(Json(GameFramesResponse {
        count: frames.len(),
        frames,
    }))
}

/// WebSocket message types for the board viewer
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct WebSocketMessage {
    #[serde(rename = "Type")]
    pub message_type: String,
    #[serde(rename = "Data")]
    pub data: serde_json::Value,
}

/// `?startFrame=N`: replay starts at turn N. Parsed from the raw query
/// string — first occurrence wins, like Go's `r.URL.Query().Get`;
/// `strconv.Atoi` semantics: missing/unparseable/empty → 0. Not a
/// `Query` extractor: `serde_urlencoded` rejects duplicate fields with
/// a 400, which the engine never does.
fn start_frame_from_query(query: Option<&str>) -> i32 {
    query
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "startFrame")
                .map(|(_, v)| v)
        })
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0)
}

/// GET /api/games/{id}/events
/// WebSocket endpoint for streaming game frames
pub async fn game_events_websocket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
    RawQuery(query): RawQuery,
) -> impl IntoResponse {
    let start_frame = start_frame_from_query(query.as_deref());
    ws.on_upgrade(move |socket| handle_game_websocket(socket, state, game_id, start_frame))
}

/// Send a WebSocket close frame and wait for the client to acknowledge.
///
/// The board viewer uses ReconnectingWebSocket which auto-reconnects on any
/// server-initiated close. If we just drop the socket (by returning), the TCP
/// connection resets before the client processes buffered messages like game_end.
/// By sending a proper Close frame and waiting for the client's response, we give
/// the client time to process all messages and close its side first.
async fn graceful_close(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
) {
    let _ = sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000, // NORMAL, like the engine (close frame absent → clients see 1005)
            reason: "".into(),
        })))
        .await;
    // Drain until the client sends Close back or the connection drops
    while let Some(msg) = receiver.next().await {
        if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
            break;
        }
    }
}

type WsSender = futures::stream::SplitSink<WebSocket, Message>;
type WsReceiver = futures::stream::SplitStream<WebSocket>;

struct FrameContext<'a> {
    authors: &'a std::collections::HashMap<String, String>,
    suppressed: &'a std::collections::HashMap<String, std::collections::HashSet<String>>,
}

async fn send_message(sender: &mut WsSender, kind: &str, data: serde_json::Value) -> bool {
    let message = WebSocketMessage {
        message_type: kind.to_string(),
        data,
    };
    sender
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .is_ok()
}

async fn send_error_and_close(sender: &mut WsSender, receiver: &mut WsReceiver, message: &str) {
    let _ = send_message(sender, "error", serde_json::json!({"message": message})).await;
    graceful_close(sender, receiver).await;
}

async fn send_frames(
    sender: &mut WsSender,
    turns: &[crate::models::turn::Turn],
    last_sent_turn: &mut i32,
    context: &FrameContext<'_>,
) -> bool {
    for turn in turns {
        if turn.turn_number <= *last_sent_turn {
            continue;
        }
        if let Some(mut frame) = turn.frame_data.clone() {
            prepare_public_frame(&mut frame, context.authors, context.suppressed);
            if !send_message(sender, "frame", frame).await {
                return false;
            }
            *last_sent_turn = turn.turn_number;
        }
    }
    true
}

async fn send_terminal(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    state: &AppState,
    game_id: Uuid,
    status: GameStatus,
    last_sent_turn: &mut i32,
    context: &FrameContext<'_>,
) {
    let turns = match crate::models::turn::get_turns_from(
        &state.db,
        game_id,
        last_sent_turn.saturating_add(1),
    )
    .await
    {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed terminal frame drain");
            send_error_and_close(sender, receiver, "Failed to fetch game frames").await;
            return;
        }
    };
    if !send_frames(sender, &turns, last_sent_turn, context).await {
        return;
    }
    match status {
        GameStatus::Finished | GameStatus::Failed => {
            if matches!(status, GameStatus::Failed) {
                let _ = send_message(
                    sender,
                    "error",
                    serde_json::json!({"message":"Game failed"}),
                )
                .await;
            }
            // The engine's game_end carries the game object. One row read
            // at terminal only; any failure sends `{}` (logged inside) so
            // the close sequence can never wedge on the payload.
            let payload = match get_game_row_by_any_id(&state.db, game_id).await {
                Ok(Some(record)) => game_end_payload(state, &record).await,
                Ok(None) => {
                    tracing::error!(%game_id,
                        "Failed to build game_end payload: game row vanished");
                    serde_json::json!({})
                }
                Err(error) => {
                    tracing::error!(%game_id, error = %format_args!("{error:#}"),
                        "Failed to build game_end payload");
                    serde_json::json!({})
                }
            };
            let _ = send_message(sender, "game_end", payload).await;
        }
        _ => {
            send_error_and_close(sender, receiver, "Game changed, please reconnect").await;
            return;
        }
    }
    graceful_close(sender, receiver).await;
}

/// Replay an archived game over the websocket: every frame as a `frame`
/// event, then `game_end`, then a graceful close — the same terminal
/// sequence a finished DB game produces. Storage failures use the same
/// error-event-then-close behavior the DB path has today. Frames-only: no
/// Game is served here, so Play and native objects need no branching
/// beyond passing the row kind into `load`.
async fn handle_archived_websocket(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    state: &AppState,
    record: &GameRow,
    start_frame: i32,
) {
    let game_id = record.game.game_id;
    let path = record.gcs_path.clone().unwrap_or_default();
    let archived = match state
        .archive
        .load(game_id, &path, archive_row_kind(record))
        .await
    {
        Ok(archived) => archived,
        Err(LoadArchiveError::ObjectMissing { .. }) => {
            send_error_and_close(sender, receiver, "Game not found").await;
            return;
        }
        Err(LoadArchiveError::Failed(error)) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"),
                "Failed to load archived game for WebSocket");
            send_error_and_close(sender, receiver, "Internal server error").await;
            return;
        }
    };
    let authors = frame_author_map(&state.db, game_id).await;
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &record.game.status)
            .await;
    // Full-accessor iteration (&Box<RawValue> deref-coerces to &RawValue).
    for raw in archived.frames() {
        // Single-frame parse, µs-scale, inline.
        let frame = match ArchiveReader::prepare_frame(raw, &authors, &suppressed) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::error!(%game_id, error = %format_args!("{error:#}"),
                    "Failed to prepare archived frame for WebSocket");
                send_error_and_close(sender, receiver, "Internal server error").await;
                return;
            }
        };
        if frame
            .get("Turn")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
            < i64::from(start_frame)
        {
            continue;
        }
        if !send_message(sender, "frame", frame).await {
            return;
        }
    }
    if matches!(record.game.status, GameStatus::Failed) {
        let _ = send_message(
            sender,
            "error",
            serde_json::json!({"message": "Game failed"}),
        )
        .await;
    }
    let _ = send_message(sender, "game_end", game_end_payload(state, record).await).await;
    graceful_close(sender, receiver).await;
}

async fn handle_game_websocket(
    socket: WebSocket,
    state: AppState,
    game_id: Uuid,
    start_frame: i32,
) {
    let (mut sender, mut receiver) = socket.split();
    let record = match get_game_row_by_any_id(&state.db, game_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
            return;
        }
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to fetch game for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Internal server error").await;
            return;
        }
    };
    // Canonical arena id from here on: subscribe, turns, and terminal
    // rereads all key on it, whichever id kind the client connected with.
    let game_id = record.game.game_id;
    match serves_from_archive(&state.db, &record).await {
        Ok(true) => {
            handle_archived_websocket(&mut sender, &mut receiver, &state, &record, start_frame)
                .await;
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"),
                "Failed archive check for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Internal server error").await;
            return;
        }
    }
    let mut subscription = state.watched_games.subscribe(game_id).await;
    let mut retired_before_history = false;
    if !subscription.created {
        loop {
            let readiness = *subscription.readiness.borrow_and_update();
            match readiness {
                Readiness::Active { .. } => break,
                Readiness::Retired => {
                    // A terminal update may already be queued. Read history and
                    // authoritative status before deciding how to close.
                    retired_before_history = true;
                    break;
                }
                Readiness::Pending => {}
            }
            // Subscription owns the entry and therefore its watch sender.
            let _ = subscription.readiness.changed().await;
        }
    }
    let existing_turns = match get_turns_by_game_id(&state.db, game_id).await {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to fetch turns for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Failed to fetch game frames").await;
            return;
        }
    };
    if subscription.created {
        let last = existing_turns
            .iter()
            .rev()
            .find(|turn| turn.frame_data.is_some());
        state
            .watched_games
            .seed_with_turn_id(
                game_id,
                &mut subscription,
                last.map_or(-1, |turn| turn.turn_number),
                last.map(|turn| turn.turn_id),
            )
            .await;
    }
    let authors = frame_author_map(&state.db, game_id).await;
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &record.game.status)
            .await;
    let context = FrameContext {
        authors: &authors,
        suppressed: &suppressed,
    };
    // `?startFrame=N`: 0/missing → -1 (replay from turn 0, today's
    // behavior); negative N → sends everything, like the engine. The
    // subscription cursor (`seed_with_turn_id`) is independent of this —
    // `send_frames`' `turn <= last_sent_turn` skip does the filtering.
    let mut last_sent_turn = start_frame.saturating_sub(1);
    if !send_frames(&mut sender, &existing_turns, &mut last_sent_turn, &context).await {
        return;
    }
    match get_game_by_id(&state.db, game_id).await {
        Ok(Some(game)) if matches!(game.status, GameStatus::Finished | GameStatus::Failed) => {
            send_terminal(
                &mut sender,
                &mut receiver,
                &state,
                game_id,
                game.status,
                &mut last_sent_turn,
                &context,
            )
            .await;
            return;
        }
        Ok(Some(_)) if retired_before_history => {
            send_error_and_close(&mut sender, &mut receiver, "Game changed, please reconnect")
                .await;
            return;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
            return;
        }
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to reread game for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Internal server error").await;
            return;
        }
    }
    loop {
        tokio::select! {
            msg = receiver.next() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Ping(data))) => { if sender.send(Message::Pong(data)).await.is_err() { return; } }
                Some(Ok(_)) => {}
            },
            update = subscription.updates.recv() => match update {
                Ok(WatchedGameUpdate::Frames(turns)) => {
                    if !send_frames(&mut sender, &turns, &mut last_sent_turn, &context).await { return; }
                }
                Ok(WatchedGameUpdate::Terminal(status)) => {
                    send_terminal(&mut sender, &mut receiver, &state, game_id, status, &mut last_sent_turn, &context).await;
                    return;
                }
                Ok(WatchedGameUpdate::Missing) => {
                    send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
                    return;
                }
                Ok(WatchedGameUpdate::Reset) => {
                    send_error_and_close(&mut sender, &mut receiver, "Game restarted, please reconnect").await;
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(%game_id, lagged = count, "WebSocket lagged");
                    send_error_and_close(&mut sender, &mut receiver, "Connection lagged, please reconnect").await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Subscription owns the broadcast sender, so this is unreachable
                    // during normal operation. Close once if that invariant changes.
                    send_error_and_close(&mut sender, &mut receiver, "Game changed, please reconnect").await;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use sqlx::PgPool;

    pub(crate) fn zstd_json(value: &serde_json::Value, level: i32) -> Vec<u8> {
        zstd::stream::encode_all(serde_json::to_vec(value).unwrap().as_slice(), level).unwrap()
    }

    /// Engine game JSON with deliberately NON-arena-default values, so the
    /// asserts prove the archive won over arena's mode rules.
    pub(crate) fn archived_engine_game_json(play_id: &str) -> serde_json::Value {
        serde_json::json!({
            "ID": play_id,
            "Status": "complete",
            "Width": 15,
            "Height": 9,
            "Ruleset": {"name": "royale", "damagePerTurn": "14"},
            "SnakeTimeout": 600,
            "MaxTurns": 2500,
            "FoodSpawns": [],
            "HazardSpawns": [],
            "Source": "ladder",
            "RulesetName": "royale",
            "RulesStages": null,
            "Map": "royale",
            "Created": 1735689600000000i64,
        })
    }

    /// One snake frame in the archived-engine shape. The typed-Option keys
    /// (`StatusCode`, `Error`, `IsBot`, `IsEnvironment`, `TimingMicros`)
    /// carry JSON null exactly as v1/v2 objects serialize them, so the
    /// null-tolerant fill in `normalize_public_frame` gets exercised.
    pub(crate) fn frame_json(turn: i32, with_private_fields: bool) -> serde_json::Value {
        let mut snake = serde_json::json!({
            "ID": "s1",
            "Name": "Snek",
            "URL": "https://snake.example.com/move?token=sekrit",
            "ProxyURL": "https://proxy.example.com/s1",
            "APIVersion": "1",
            "Body": [{"X": 5, "Y": 5}],
            "Health": 100,
            "Latency": "12",
            "Author": "playuser",
            "Death": null,
            "StatusCode": null,
            "Error": null,
            "IsBot": null,
            "IsEnvironment": null,
        });
        if !with_private_fields {
            let obj = snake.as_object_mut().unwrap();
            obj.remove("URL");
            obj.remove("ProxyURL");
            obj.remove("APIVersion");
        }
        serde_json::json!({
            "Turn": turn,
            "Snakes": [snake],
            "Food": [],
            "Hazards": [],
        })
    }

    /// v1 object: hand-built (no v1 writer exists any more) — keeps its
    /// non-empty URL/ProxyURL/APIVersion and zstd-3, like every existing
    /// Play object in the bucket.
    pub(crate) fn v1_archive(play_id: &str) -> Vec<u8> {
        let object = serde_json::json!({
            "game": archived_engine_game_json(play_id),
            "frames": [frame_json(0, true), frame_json(1, true)],
            "exported_at": "2026-01-01T00:00:00Z",
        });
        zstd_json(&object, 3)
    }

    /// v2 object: built with the REAL writer — `crate::backup::
    /// build_compressed_export(engine_game, frames)` — so drift between
    /// the v2 writer's member names/version/stripping and the reader fails
    /// the tests using this fixture. Frames are passed WITH private fields:
    /// the writer strips URL/ProxyURL/APIVersion, which the served-blank
    /// asserts then pin end-to-end.
    pub(crate) fn v2_archive(play_id: &str) -> Vec<u8> {
        let game: crate::engine_models::EngineGame =
            serde_json::from_value(archived_engine_game_json(play_id)).unwrap();
        crate::backup::build_compressed_export(game, vec![frame_json(0, true), frame_json(1, true)])
            .unwrap()
    }

    /// A native frame (raw `turns.frame_data` shape): engine::frame
    /// PascalCase keys, URL already blank (native frames persist blanks),
    /// EMPTY Author whose snake ID is the `game_battlesnake_id` — the key
    /// `fill_frame_authors` matches.
    fn native_frame_json(turn: i32, snake_id: &str) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": snake_id,
                "Name": "Native Snake",
                "URL": "",
                "ProxyURL": "",
                "APIVersion": "",
                "Author": "",
                "Body": [{"X": 1, "Y": 1}],
                "Health": 100,
                "Latency": "10",
                "Death": null,
            }],
            "Food": [],
            "Hazards": [],
        })
    }

    /// DEV-1980-shaped native object (`NativeGameExport`). The `game`
    /// member is deliberately NOT EngineGame-shaped (no ID/Status — a
    /// `NativeGameHeader`), so this fixture fails Play-kind header
    /// validation and pins that native rows never run it.
    pub(crate) fn native_archive(game_id: &str, snake_id: &str) -> Vec<u8> {
        let object = serde_json::json!({
            "archive_version": 2,
            "game_id": game_id,
            "game": {
                "game_type": "Standard",
                "board_size": "11x11",
                "status": "finished",
            },
            "frames": [native_frame_json(0, snake_id), native_frame_json(1, snake_id)],
            "exported_at": "2026-01-01T00:00:00Z",
        });
        zstd_json(&object, 9)
    }

    pub(crate) fn native_frame_value(turn: i32, snake_id: &str) -> serde_json::Value {
        native_frame_json(turn, snake_id)
    }

    /// Imported Play stub row: `engine_game_id` + `gcs_path` + terminal
    /// status, no `turns`. Returns the arena game id.
    pub(crate) async fn fixture_archived_game(
        pool: &PgPool,
        engine_id: &str,
        gcs_path: &str,
    ) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO games
                (board_size, game_type, status, engine_game_id, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', $1, NOW(), $2, 1)
            RETURNING game_id
            "#,
        )
        .bind(engine_id)
        .bind(gcs_path)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    /// Native row: `engine_game_id` NULL, terminal status, `gcs_path`
    /// `native/YYYY/MM/DD/<id>.json.zst`, archive_version 2, archived_at —
    /// plus one battlesnakes + game_battlesnakes pair so author fill has a
    /// row to read. Returns `(game_id, game_battlesnake_id)`.
    pub(crate) async fn fixture_native_game(pool: &PgPool) -> cja::Result<(Uuid, Uuid)> {
        let game_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO games (board_size, game_type, status)
            VALUES ('11x11', 'Standard', 'finished')
            RETURNING game_id
            "#,
        )
        .fetch_one(pool)
        .await?;
        let gcs_path = format!("native/2026/10/31/{game_id}.json.zst");
        sqlx::query(
            "UPDATE games SET archived_at = NOW(), gcs_path = $1, archive_version = 2 WHERE game_id = $2",
        )
        .bind(&gcs_path)
        .bind(game_id)
        .execute(pool)
        .await?;
        let user_id = crate::routes::test_support::create_user(pool, 424242, false).await;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO battlesnakes (user_id, name, url, visibility)
            VALUES ($1, 'Native Snake', 'http://snake', 'public')
            RETURNING battlesnake_id
            "#,
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?;
        let game_battlesnake_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO game_battlesnakes (game_id, battlesnake_id)
            VALUES ($1, $2)
            RETURNING game_battlesnake_id
            "#,
        )
        .bind(game_id)
        .bind(battlesnake_id)
        .fetch_one(pool)
        .await?;
        Ok((game_id, game_battlesnake_id))
    }

    /// A row with an explicit status (and optional archive pointers) — used
    /// by the guard test that pins non-terminal rows to the DB path.
    pub(crate) async fn fixture_game_with_status(
        pool: &PgPool,
        status: &str,
        engine_id: Option<&str>,
        gcs_path: Option<&str>,
    ) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO games
                (board_size, game_type, status, engine_game_id, gcs_path, archived_at, archive_version)
            VALUES ('11x11', 'Standard', $1, $2, $3,
                    CASE WHEN $3::text IS NULL THEN NULL ELSE NOW() END,
                    CASE WHEN $3::text IS NULL THEN NULL ELSE 2 END)
            RETURNING game_id
            "#,
        )
        .bind(status)
        .bind(engine_id)
        .bind(gcs_path)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    /// Test state whose archive reader and object store are the SAME
    /// in-memory store, so tests can seed objects and count fetches.
    pub(crate) fn archived_state(
        pool: sqlx::PgPool,
        store: std::sync::Arc<crate::object_store::InMemoryObjectStore>,
    ) -> AppState {
        let mut state = AppState::test_from_pool(pool);
        state.object_store = store.clone();
        state.archive = crate::archive::ArchiveReader::new(
            store,
            crate::archive::DEFAULT_ARCHIVE_CACHE_MAX_BYTES,
        );
        state
    }

    // --- Live engine capture: Play game addedb91 (DEV-1988) ---

    /// Play game addedb91-93ef-4889-a936-d0caef728a8c, captured live from
    /// engine.battlesnake.com 2026-10-30/31 (Go engine v1.0.139):
    /// `server/tests/fixtures/engine-capture-addedb91/`.
    pub(crate) const CAPTURE_ADDEDB91: &str = "addedb91-93ef-4889-a936-d0caef728a8c";

    pub(crate) fn capture_game() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/engine-capture-addedb91/engine_game.json"
        ))
        .unwrap()
    }

    pub(crate) fn capture_frames() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/engine-capture-addedb91/engine_frames_offset0_limit100.json"
        ))
        .unwrap()
    }

    pub(crate) fn capture_ws_summary() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/engine-capture-addedb91/engine_ws_summary.json"
        ))
        .unwrap()
    }

    /// Emulate the engine-DB→backup shape that produced real v1 objects:
    /// every proto3-omittable empty (""/0/false/{}, and "" inside Death)
    /// was written as an explicit JSON null. This is exactly the null
    /// pattern `normalize_public_frame` must invert at serve time. Takes
    /// the frames ARRAY (`capture_frames()["Frames"]`).
    fn null_proto3_empties(frames: &mut serde_json::Value) {
        for frame in frames.as_array_mut().expect("capture frames is an array") {
            for snake in frame["Snakes"].as_array_mut().unwrap() {
                let obj = snake.as_object_mut().unwrap();
                let null_if = |obj: &mut serde_json::Map<String, serde_json::Value>,
                               key: &str,
                               empty: serde_json::Value| {
                    if obj.get(key) == Some(&empty) {
                        obj.insert(key.to_string(), serde_json::Value::Null);
                    }
                };
                for key in ["Shout", "Squad", "Latency", "Author", "Error"] {
                    null_if(obj, key, serde_json::json!(""));
                }
                null_if(obj, "StatusCode", serde_json::json!(0));
                for key in ["IsBot", "IsEnvironment"] {
                    null_if(obj, key, serde_json::json!(false));
                }
                null_if(obj, "TimingMicros", serde_json::json!({}));
                if let Some(death) = obj.get_mut("Death").and_then(|d| d.as_object_mut()) {
                    for key in ["EliminatedBy", "Cause"] {
                        null_if(death, key, serde_json::json!(""));
                    }
                }
            }
        }
    }

    /// Seed the addedb91 capture as a v1 archive object (nulled engine-DB
    /// shape) + archived Play row. Returns (state, arena_id).
    pub(crate) async fn capture_fixture(pool: &PgPool) -> cja::Result<(AppState, Uuid)> {
        // Sanity: the raw capture frames are well-formed engine frames.
        let frames = capture_frames()["Frames"].clone();
        for frame in frames.as_array().unwrap() {
            serde_json::from_value::<crate::engine_models::EngineGameFrame>(frame.clone())
                .expect("capture frame must deserialize as EngineGameFrame");
        }

        let store = std::sync::Arc::new(crate::object_store::InMemoryObjectStore::default());
        let gcs_path = format!("games/2026/01/01/{CAPTURE_ADDEDB91}.json.zst");
        let mut nulled_frames = frames.clone();
        null_proto3_empties(&mut nulled_frames);
        // `Created` rides as a NUMBER in v1 objects (the engine's int64 DB
        // column); the engine's API jsonpb layer is what stringifies it.
        let mut game = capture_game()["Game"].clone();
        game["Created"] = serde_json::json!(
            capture_game()["Game"]["Created"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap()
        );
        let object = serde_json::json!({
            "game": game,
            "frames": nulled_frames,
            "exported_at": "2026-01-01T00:00:00Z",
        });
        use crate::object_store::ObjectStore as _;
        store.put(&gcs_path, zstd_json(&object, 3)).await.unwrap();
        let game_id = fixture_archived_game(pool, CAPTURE_ADDEDB91, &gcs_path).await?;
        let state = archived_state(pool.clone(), store);
        Ok((state, game_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_game(status: GameStatus, game_type: GameType) -> BoardViewerGame {
        let game_id = Uuid::nil();
        let (ruleset, ruleset_name, map, rules_stages) = engine_rules(game_type, game_id);
        BoardViewerGame {
            id: "abc-123".to_string(),
            status: engine_status(status).to_string(),
            arena_status: status.as_str().to_string(),
            width: 11,
            height: 11,
            ruleset,
            ruleset_name,
            rules_stages,
            map,
            source: "arena".to_string(),
            snake_timeout: crate::engine::MOVE_TIMEOUT_MS,
            max_turns: crate::engine::MAX_TURNS,
            food_spawns: Vec::new(),
            hazard_spawns: Vec::new(),
            created: "1790859473028997".to_string(),
        }
    }

    /// `Game` carries every key of the legacy engine's `GET /games/{id}`
    /// (sampled live from engine.battlesnake.com) plus arena's additive
    /// `ArenaStatus`; `LastFrame` is omitted until a frame exists.
    #[test]
    fn test_board_viewer_response_matches_legacy_engine_shape() {
        let response = BoardViewerGameResponse {
            game: sample_game(GameStatus::Finished, GameType::Standard),
            last_frame: None,
        };

        let json = serde_json::to_value(&response).unwrap();
        let mut top: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top.sort();
        assert_eq!(top, ["Game"]);
        let mut game: Vec<&str> = json["Game"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        game.sort();
        assert_eq!(
            game,
            [
                "ArenaStatus",
                "Created",
                "FoodSpawns",
                "HazardSpawns",
                "Height",
                "ID",
                "Map",
                "MaxTurns",
                "RulesStages",
                "Ruleset",
                "RulesetName",
                "SnakeTimeout",
                "Source",
                "Status",
                "Width",
            ]
        );
        assert_eq!(json["Game"]["Created"], "1790859473028997");
        assert_eq!(json["Game"]["SnakeTimeout"], 500);

        let with_frame = BoardViewerGameResponse {
            game: sample_game(GameStatus::Running, GameType::Standard),
            last_frame: Some(serde_json::json!({"Turn": 7})),
        };
        let json = serde_json::to_value(&with_frame).unwrap();
        assert_eq!(json["LastFrame"]["Turn"], 7);
    }

    /// Ruleset/map/stages match what play.battlesnake.com created for each
    /// mode (play `ui/maps.py`, `leaderboards_setup.py`, `game_stages.py`),
    /// with arena's real settings values.
    #[test]
    fn test_engine_rules_match_play_per_mode() {
        let standard_pipeline = [
            "movement.standard",
            "starvation.standard",
            "hazard_damage.standard",
            "feed_snakes.standard",
            "elimination.standard",
        ];
        let with_game_over = |game_over: &str| {
            std::iter::once(game_over.to_string())
                .chain(standard_pipeline.iter().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        };

        let (ruleset, name, map, stages) = engine_rules(GameType::Standard, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "standard"));
        assert_eq!(ruleset["name"], "standard");
        assert_eq!(ruleset["foodSpawnChance"], "15");
        assert_eq!(ruleset["minimumFood"], "1");
        assert!(!ruleset.contains_key("shrinkEveryNTurns"));
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (ruleset, name, map, stages) = engine_rules(GameType::Royale, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "royale"));
        assert_eq!(ruleset["name"], "standard");
        assert_eq!(ruleset["damagePerTurn"], "14");
        assert_eq!(ruleset["shrinkEveryNTurns"], "25");
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (ruleset, name, map, stages) = engine_rules(GameType::Constrictor, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("constrictor", "empty"));
        assert_eq!(ruleset["name"], "constrictor");
        assert_eq!(
            stages,
            [
                "game_over.standard",
                "movement.standard",
                "starvation.standard",
                "hazard_damage.standard",
                "feed_snakes.standard",
                "modify_snakes.always_grow",
                "elimination.standard",
            ]
        );

        let (_, name, map, stages) = engine_rules(GameType::SnailMode, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "snail_mode"));
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (_, name, map, stages) = engine_rules(GameType::Solo, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "standard"));
        assert_eq!(stages, with_game_over("game_over.solo_snake"));
    }

    #[test]
    fn test_engine_status_mapping() {
        assert_eq!(engine_status(GameStatus::Waiting), "stopped");
        assert_eq!(engine_status(GameStatus::Running), "running");
        assert_eq!(engine_status(GameStatus::Finished), "complete");
        assert_eq!(engine_status(GameStatus::Failed), "error");
    }

    #[test]
    fn arena_status_serializes_all_lossless_states() {
        for (status, expected) in [
            (GameStatus::Waiting, "waiting"),
            (GameStatus::Running, "running"),
            (GameStatus::Finished, "finished"),
            (GameStatus::Failed, "failed"),
        ] {
            let response = BoardViewerGameResponse {
                game: sample_game(status, GameType::Standard),
                last_frame: None,
            };
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["Game"]["ArenaStatus"], expected);
            assert_eq!(value["Game"]["Status"], engine_status(status));
        }
    }

    #[test]
    fn test_frames_response_serialization() {
        // Engine envelope: PascalCase Count/Frames wrapping the frame blobs,
        // exactly like engine.battlesnake.com.
        let response = GameFramesResponse {
            count: 1,
            frames: vec![serde_json::json!({
                "Turn": 0,
                "Snakes": [],
                "Food": [{"X": 1, "Y": 2}],
                "Hazards": [],
            })],
        };

        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"Count":1,"Frames":[{"Food":[{"X":1,"Y":2}],"Hazards":[],"Snakes":[],"Turn":0}]}"#
        );
    }

    #[test]
    fn test_clamp_frames_pagination_defaults() {
        assert_eq!(clamp_frames_pagination(None, None), (0, MAX_FRAMES_LIMIT));
    }

    #[test]
    fn test_clamp_frames_pagination_clamps_limit_to_max() {
        assert_eq!(
            clamp_frames_pagination(Some(200), Some(5000)),
            (200, MAX_FRAMES_LIMIT)
        );
    }

    #[test]
    fn test_clamp_frames_pagination_rejects_negatives() {
        assert_eq!(clamp_frames_pagination(Some(-5), Some(-10)), (0, 0));
    }

    #[test]
    fn test_clamp_frames_pagination_passes_through_valid_values() {
        assert_eq!(clamp_frames_pagination(Some(300), Some(50)), (300, 50));
    }

    #[test]
    fn test_websocket_message_serialization() {
        let msg = WebSocketMessage {
            message_type: "frame".to_string(),
            data: serde_json::json!({"Turn": 5, "Snakes": []}),
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"Type\":\"frame\""));
        assert!(json.contains("\"Data\""));
    }

    use sqlx::PgPool;

    /// Insert a bare game row (no snakes) with the given status.
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

    async fn fixture_turn(
        pool: &PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: Option<serde_json::Value>,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn_number)
            .bind(frame_data)
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// `GET /api/games/{id}` serves the legacy engine's `{Game, LastFrame}`:
    /// a ladder game reports `Source: "arena"`, `LastFrame` is the latest
    /// turn, and a frame persisted before DEV-1502 (Latency "timeout", no
    /// engine fields) is served in the engine's shape.
    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_serves_engine_game_and_last_frame(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('info') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
            .bind(leaderboard_id)
            .bind(game_id)
            .execute(&pool)
            .await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            1,
            Some(serde_json::json!({
                "Turn": 1,
                "Snakes": [{"ID": "s1", "Latency": "timeout", "APIVersion": "1"}],
                "Food": [],
                "Hazards": []
            })),
        )
        .await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        let game = &json["Game"];
        assert_eq!(game["Status"], "complete");
        assert_eq!(game["Source"], "arena");
        assert_eq!(game["RulesetName"], "standard");
        assert_eq!(game["Map"], "standard");
        assert_eq!(game["Ruleset"]["name"], "standard");
        assert_eq!(game["SnakeTimeout"], 500);
        assert!(
            game["Created"]
                .as_str()
                .is_some_and(|c| c.parse::<i64>().is_ok())
        );

        let last = &json["LastFrame"];
        assert_eq!(last["Turn"], 1);
        assert_eq!(last["PointState"], serde_json::json!([]));
        let snake = &last["Snakes"][0];
        assert_eq!(snake["Latency"], "500");
        assert_eq!(snake["Error"], crate::engine::frame::TIMEOUT_ERROR);
        assert_eq!(snake["APIVersion"], "");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_omits_last_frame_before_turn_zero(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "waiting").await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();
        let json = response_json(response).await;

        assert_eq!(json["Game"]["Status"], "stopped");
        assert_eq!(json["Game"]["Source"], "custom");
        assert!(json.get("LastFrame").is_none());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_returns_engine_envelope(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        // Insert out of order to prove ordering comes from SQL, plus a
        // NULL-frame turn that must be filtered out.
        fixture_turn(
            &pool,
            game_id,
            1,
            Some(serde_json::json!({"Turn": 1, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(&pool, game_id, 2, None).await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await
        .unwrap()
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        assert_eq!(json["Count"], 2);
        let frames = json["Frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["Turn"], 0);
        assert_eq!(frames[1]["Turn"], 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_paginates_with_offset_and_limit(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        for turn in 0..5 {
            fixture_turn(
                &pool,
                game_id,
                turn,
                Some(serde_json::json!({"Turn": turn, "Snakes": [], "Food": [], "Hazards": []})),
            )
            .await?;
        }

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(2),
                limit: Some(2),
            }),
        )
        .await
        .unwrap()
        .into_response();

        let json = response_json(response).await;
        assert_eq!(json["Count"], 2);
        assert_eq!(json["Frames"][0]["Turn"], 2);
        assert_eq!(json["Frames"][1]["Turn"], 3);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_offset_past_end_returns_empty(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(100),
                limit: Some(100),
            }),
        )
        .await
        .unwrap()
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["Count"], 0);
        assert_eq!(json["Frames"].as_array().unwrap().len(), 0);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_unknown_game_is_404(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool);

        let result = get_game_frames(
            State(state),
            Path(Uuid::new_v4()),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await;

        let response = result.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    /// A finished Solo game's frames flow through the unchanged endpoint
    /// with the full progression: ordered from turn 0 through the final
    /// death frame, with the death cause present on the last frame only.
    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_serves_full_solo_progression(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Solo', 'finished') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await?;

        let solo_frame = |turn: i32, death: Option<&str>| {
            serde_json::json!({
                "Turn": turn,
                "Snakes": [{
                    "ID": "snake-1",
                    "Death": death.map(|c| serde_json::json!({"Cause": c, "Turn": turn})),
                }],
                "Food": [],
                "Hazards": [],
            })
        };
        for turn in 0..3 {
            fixture_turn(&pool, game_id, turn, Some(solo_frame(turn, None))).await?;
        }
        fixture_turn(
            &pool,
            game_id,
            3,
            Some(solo_frame(3, Some("out-of-health"))),
        )
        .await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(0),
                limit: Some(100),
            }),
        )
        .await
        .unwrap()
        .into_response();

        let json = response_json(response).await;
        let frames = json["Frames"].as_array().unwrap();
        assert_eq!(json["Count"], 4);
        assert_eq!(frames[0]["Turn"], 0, "progression starts at turn 0");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame["Turn"], i, "frames stay ordered");
        }
        let last = frames.last().unwrap();
        assert_eq!(last["Snakes"][0]["Death"]["Cause"], "out-of-health");
        for frame in &frames[..frames.len() - 1] {
            assert!(
                frame["Snakes"][0]["Death"].is_null(),
                "no death before the end"
            );
        }

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_includes_id_and_status(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["Game"]["ID"], game_id.to_string());
        assert_eq!(json["Game"]["Status"], "complete");
        assert_eq!(json["Game"]["Width"], 11);
        assert_eq!(json["Game"]["Height"], 11);

        Ok(())
    }

    #[test]
    fn fill_frame_authors_fills_empty_and_missing_only() {
        let mut authors = std::collections::HashMap::new();
        authors.insert("gb-1".to_string(), "corey".to_string());
        authors.insert("gb-2".to_string(), "brandi".to_string());

        let mut frame = serde_json::json!({
            "Turn": 3,
            "Snakes": [
                {"ID": "gb-1", "Author": ""},          // legacy empty → filled
                {"ID": "gb-2"},                          // missing → filled
                {"ID": "gb-3", "Author": ""},          // unknown ID → untouched
                {"ID": "gb-1", "Author": "already"},   // present → preserved
            ]
        });
        fill_frame_authors(&mut frame, &authors);

        assert_eq!(frame["Snakes"][0]["Author"], "corey");
        assert_eq!(frame["Snakes"][1]["Author"], "brandi");
        assert_eq!(frame["Snakes"][2]["Author"], "");
        assert_eq!(frame["Snakes"][3]["Author"], "already");
    }

    #[test]
    fn fill_frame_authors_tolerates_shapeless_frames() {
        let mut authors = std::collections::HashMap::new();
        authors.insert("gb-1".to_string(), "corey".to_string());

        // No Snakes key, wrong type, empty map: all must be silent no-ops.
        let mut no_snakes = serde_json::json!({"Turn": 0});
        fill_frame_authors(&mut no_snakes, &authors);
        assert_eq!(no_snakes, serde_json::json!({"Turn": 0}));

        let mut wrong_type = serde_json::json!({"Snakes": "nope"});
        fill_frame_authors(&mut wrong_type, &authors);
        assert_eq!(wrong_type, serde_json::json!({"Snakes": "nope"}));

        let mut frame = serde_json::json!({"Snakes": [{"ID": "gb-1"}]});
        fill_frame_authors(&mut frame, &std::collections::HashMap::new());
        assert_eq!(frame, serde_json::json!({"Snakes": [{"ID": "gb-1"}]}));
    }

    // --- Archived-game serving (DEV-1979) ---

    use crate::object_store::InMemoryObjectStore;
    use crate::object_store::ObjectStore as _;
    use fixtures as fx;
    use std::sync::Arc;

    /// Seed an archived-Play row + store and return (state, arena_id,
    /// play_id, store). The object is built by `build_object` from the
    /// engine id, so its contents carry the same play id as the row.
    async fn archived_play_fixture(
        pool: &PgPool,
        build_object: impl FnOnce(&str) -> Vec<u8>,
    ) -> cja::Result<(AppState, Uuid, Uuid, Arc<InMemoryObjectStore>)> {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, build_object(&engine_id))
            .await
            .unwrap();
        let game_id = fx::fixture_archived_game(pool, &engine_id, &gcs_path).await?;
        let state = fx::archived_state(pool.clone(), store.clone());
        Ok((state, game_id, Uuid::parse_str(&engine_id).unwrap(), store))
    }

    fn assert_archived_game_shape(game: &serde_json::Value, arena_id: Uuid) {
        // The archive won over arena's mode-rule defaults.
        assert_eq!(game["Width"], 15);
        assert_eq!(game["Height"], 9);
        assert_eq!(game["Map"], "royale");
        assert_eq!(game["RulesetName"], "royale");
        assert_eq!(game["Ruleset"]["damagePerTurn"], "14");
        assert_eq!(game["Source"], "ladder");
        assert_eq!(game["SnakeTimeout"], 600);
        assert_eq!(game["MaxTurns"], 2500);
        assert_eq!(game["Created"], "1735689600000000");
        // The arena id is the served id; status comes from the ROW.
        assert_eq!(game["ID"], arena_id.to_string());
        assert_eq!(game["Status"], "complete");
        assert_eq!(game["ArenaStatus"], "finished");
    }

    fn assert_snake_public_shape(snake: &serde_json::Value) {
        assert_eq!(snake["URL"], "");
        assert_eq!(snake["ProxyURL"], "");
        assert_eq!(snake["APIVersion"], "");
        // The null-tolerance pins: typed-Option nulls filled to the DB path's
        // public shape.
        assert_eq!(snake["StatusCode"], 0);
        assert_eq!(snake["Error"], "");
        assert_eq!(snake["IsBot"], false);
        assert_eq!(snake["IsEnvironment"], false);
        assert_eq!(snake["TimingMicros"], serde_json::json!({}));
        // The engine jsonpb EmitDefaults pins: string fields always present,
        // `""` when empty (the fixture omits these keys, so they're filled).
        assert_eq!(snake["Shout"], "");
        assert_eq!(snake["Squad"], "");
    }

    /// v1 archive: both id kinds serve the archived engine game and its
    /// last frame through the DB path's exact response shape.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_info_serves_engine_game_and_last_frame(pool: PgPool) -> cja::Result<()> {
        let (state, game_id, play_id, _store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;

        for id in [play_id, game_id] {
            let response = get_game_info(State(state.clone()), Path(id))
                .await
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            let json = response_json(response).await;
            assert_archived_game_shape(&json["Game"], game_id);
            assert_eq!(json["LastFrame"]["Turn"], 1);
            let snake = &json["LastFrame"]["Snakes"][0];
            assert_snake_public_shape(snake);
            // The engine's own author survives; the empty author map is a no-op.
            assert_eq!(snake["Author"], "playuser");
        }
        Ok(())
    }

    /// Every endpoint blanks the archived snake's private fields — the v1
    /// object's real URL never leaks, and the typed-Option nulls are filled.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_snake_private_fields_blank_in_every_endpoint(
        pool: PgPool,
    ) -> cja::Result<()> {
        let (state, _game_id, play_id, _store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;

        let info = response_json(
            get_game_info(State(state.clone()), Path(play_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert_snake_public_shape(&info["LastFrame"]["Snakes"][0]);

        for (offset, limit) in [(0, 1), (1, 100)] {
            let page = response_json(
                get_game_frames(
                    State(state.clone()),
                    Path(play_id),
                    Query(FramesQuery {
                        offset: Some(offset),
                        limit: Some(limit),
                    }),
                )
                .await
                .unwrap()
                .into_response(),
            )
            .await;
            for frame in page["Frames"].as_array().unwrap() {
                assert_snake_public_shape(&frame["Snakes"][0]);
            }
        }
        Ok(())
    }

    /// The frames endpoint keeps the engine envelope over archived frames,
    /// including the short-page "no more frames" signal past the end.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_frames_page_serves_engine_envelope(pool: PgPool) -> cja::Result<()> {
        let (state, _game_id, play_id, _store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;

        let frames_page = |offset: Option<i64>, limit: Option<i64>| {
            get_game_frames(
                State(state.clone()),
                Path(play_id),
                Query(FramesQuery { offset, limit }),
            )
        };

        let page1 = response_json(
            frames_page(Some(0), Some(100))
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert_eq!(page1["Count"], 2);
        assert_eq!(page1["Frames"][0]["Turn"], 0);
        assert_eq!(page1["Frames"][1]["Turn"], 1);

        let page2 = response_json(
            frames_page(Some(1), Some(100))
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert_eq!(page2["Count"], 1);
        assert_eq!(page2["Frames"][0]["Turn"], 1);

        let past_end = response_json(
            frames_page(Some(2), Some(100))
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert_eq!(past_end["Count"], 0);
        assert_eq!(past_end["Frames"].as_array().unwrap().len(), 0);
        Ok(())
    }

    /// A v2 object built by the REAL writer serves exactly like v1 — pins
    /// the writer↔reader contract end-to-end, including URL keys that the
    /// writer omits and `normalize_public_frame` inserts as "".
    #[sqlx::test(migrations = "../migrations")]
    async fn v2_archive_object_serves_like_v1(pool: PgPool) -> cja::Result<()> {
        let (state, game_id, play_id, _store) =
            archived_play_fixture(&pool, fx::v2_archive).await?;
        sqlx::query("UPDATE games SET archive_version = 2 WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await?;

        let response = get_game_info(State(state), Path(play_id))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_archived_game_shape(&json["Game"], game_id);
        assert_eq!(json["LastFrame"]["Turn"], 1);
        assert_snake_public_shape(&json["LastFrame"]["Snakes"][0]);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_object_missing_is_404(pool: PgPool) -> cja::Result<()> {
        let (state, _game_id, play_id, _store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;
        // Overwrite the row's path with one that was never seeded.
        sqlx::query("UPDATE games SET gcs_path = 'games/2026/01/01/missing.json.zst' WHERE engine_game_id = $1")
            .bind(play_id.to_string())
            .execute(&pool)
            .await?;

        let info = get_game_info(State(state.clone()), Path(play_id))
            .await
            .err()
            .expect("expected error")
            .into_response();
        assert_eq!(info.status(), StatusCode::NOT_FOUND);

        let frames = get_game_frames(
            State(state),
            Path(play_id),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await
        .err()
        .expect("expected error")
        .into_response();
        assert_eq!(frames.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_lookup_unknown_id_is_404(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool);
        let info = get_game_info(State(state.clone()), Path(Uuid::new_v4()))
            .await
            .err()
            .expect("expected error")
            .into_response();
        assert_eq!(info.status(), StatusCode::NOT_FOUND);

        let frames = get_game_frames(
            State(state),
            Path(Uuid::new_v4()),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await
        .err()
        .expect("expected error")
        .into_response();
        assert_eq!(frames.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_object_corrupt_is_500_then_recovers(pool: PgPool) -> cja::Result<()> {
        let (state, _game_id, play_id, store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;
        let path = format!("games/2026/01/01/{play_id}.json.zst");
        store
            .put(&path, vec![0xde, 0xad, 0xbe, 0xef])
            .await
            .unwrap();

        let corrupt = get_game_info(State(state.clone()), Path(play_id))
            .await
            .err()
            .expect("expected error")
            .into_response();
        assert_eq!(corrupt.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Nothing partial was cached: the good object serves after repair.
        store
            .put(&path, fx::v1_archive(&play_id.to_string()))
            .await
            .unwrap();
        let ok = get_game_info(State(state), Path(play_id))
            .await
            .unwrap()
            .into_response();
        assert_eq!(ok.status(), StatusCode::OK);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_storage_error_is_500_then_recovers(pool: PgPool) -> cja::Result<()> {
        let (state, _game_id, play_id, store) =
            archived_play_fixture(&pool, fx::v1_archive).await?;

        store.fail_next_get();
        let failed = get_game_info(State(state.clone()), Path(play_id))
            .await
            .err()
            .expect("expected error")
            .into_response();
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // The knob is one-shot: the next request succeeds.
        let ok = get_game_info(State(state), Path(play_id))
            .await
            .unwrap()
            .into_response();
        assert_eq!(ok.status(), StatusCode::OK);
        Ok(())
    }

    /// The status guard: a live row with a stale `gcs_path` never touches
    /// storage and serves the DB path's arena defaults.
    #[sqlx::test(migrations = "../migrations")]
    async fn nonterminal_row_with_gcs_path_takes_db_path(pool: PgPool) -> cja::Result<()> {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, fx::v1_archive(&engine_id))
            .await
            .unwrap();
        let game_id =
            fx::fixture_game_with_status(&pool, "running", Some(&engine_id), Some(&gcs_path))
                .await?;
        let state = fx::archived_state(pool, store.clone());

        let response = get_game_info(State(state), Path(Uuid::parse_str(&engine_id).unwrap()))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["Game"]["Status"], "running");
        assert!(json.get("LastFrame").is_none());
        assert_eq!(
            json["Game"]["Width"], 11,
            "arena defaults, not the archive's"
        );
        assert_eq!(json["Game"]["Height"], 11);
        assert_eq!(json["Game"]["ID"], game_id.to_string());
        assert_eq!(
            store.get_count(),
            0,
            "the guard short-circuits before any storage touch"
        );
        Ok(())
    }

    // --- Live engine-capture parity (DEV-1988) ---

    /// Delete the fields the capture comparison must exempt: the
    /// privacy-blanked URL fields (arena blanks them unconditionally, the
    /// capture carries "" — equal in practice, but the exemption keeps the
    /// comparison about content) and arena's additive snake-level
    /// `EliminatedBy`/`EliminatedCause` if present. `Death.EliminatedBy`
    /// STAYS — it is one of the null fixes under test.
    fn strip_comparison_exemptions(frame: &mut serde_json::Value) {
        for snake in frame["Snakes"].as_array_mut().unwrap() {
            let obj = snake.as_object_mut().unwrap();
            for key in [
                "URL",
                "ProxyURL",
                "APIVersion",
                "EliminatedBy",
                "EliminatedCause",
            ] {
                obj.remove(key);
            }
        }
    }

    /// The archived Play game's `/frames?offset=0&limit=100` has ZERO JSON
    /// differences from the saved engine capture: the v1 object's explicit
    /// nulls (Shout/Squad/Latency/Death.EliminatedBy/…) come back as the
    /// engine's `""` (jsonpb EmitDefaults). This fails before the null
    /// fills and passes after.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_play_frames_match_engine_capture(pool: PgPool) -> cja::Result<()> {
        let (state, _arena_id) = fx::capture_fixture(&pool).await?;
        let play_id = Uuid::parse_str(fx::CAPTURE_ADDEDB91).unwrap();

        let served = response_json(
            get_game_frames(
                State(state),
                Path(play_id),
                Query(FramesQuery {
                    offset: Some(0),
                    limit: Some(100),
                }),
            )
            .await
            .unwrap()
            .into_response(),
        )
        .await;

        let capture = fx::capture_frames();
        assert_eq!(served["Count"], 78);
        let served_frames = served["Frames"].as_array().unwrap();
        let capture_frames = capture["Frames"].as_array().unwrap();
        assert_eq!(served_frames.len(), capture_frames.len());
        for (i, capture_frame) in capture_frames.iter().enumerate() {
            let mut expected = capture_frame.clone();
            let mut actual = served_frames[i].clone();
            strip_comparison_exemptions(&mut expected);
            strip_comparison_exemptions(&mut actual);
            assert_eq!(actual, expected, "frame {i} must match the engine capture");
        }
        Ok(())
    }

    /// `GET /games/{id}` by the PLAY id serves the capture's `Game`
    /// field-for-field (ID excepted — DEV-1979 canonicalizes to the arena
    /// id), with `ArenaStatus: "finished"` and the capture's `LastFrame`.
    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_matches_engine_capture(pool: PgPool) -> cja::Result<()> {
        let (state, arena_id) = fx::capture_fixture(&pool).await?;
        let play_id = Uuid::parse_str(fx::CAPTURE_ADDEDB91).unwrap();

        let served = response_json(
            get_game_info(State(state), Path(play_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await;

        let capture_game = &fx::capture_game()["Game"];
        let game = &served["Game"];
        assert_eq!(game["ID"], arena_id.to_string());
        assert_eq!(game["ArenaStatus"], "finished");
        for key in [
            "Status",
            "Width",
            "Height",
            "Ruleset",
            "SnakeTimeout",
            "MaxTurns",
            "FoodSpawns",
            "HazardSpawns",
            "Source",
            "RulesetName",
            "RulesStages",
            "Map",
            "Created",
        ] {
            assert_eq!(
                game[key], capture_game[key],
                "Game[{key}] must match the capture"
            );
        }

        let mut expected_last = fx::capture_game()["LastFrame"].clone();
        let mut actual_last = served["LastFrame"].clone();
        strip_comparison_exemptions(&mut expected_last);
        strip_comparison_exemptions(&mut actual_last);
        assert_eq!(actual_last, expected_last);
        Ok(())
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::watched_games::run_watched_games;
    use futures::StreamExt;
    use sqlx::PgPool;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn start(state: AppState) -> (String, CancellationToken, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let web_shutdown = shutdown.clone();
        let web = tokio::spawn(async move {
            axum::serve(
                listener,
                crate::routes::routes(state.clone())
                    .expect("asset router")
                    .into_router(),
            )
            .with_graceful_shutdown(web_shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        (format!("ws://{address}"), shutdown, web)
    }

    async fn connect(base: &str, id: Uuid) -> Client {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(format!("{base}/api/games/{id}/events")),
        )
        .await
        .unwrap()
        .unwrap()
        .0
    }

    /// Connect with an explicit query string (e.g. `?startFrame=2`),
    /// first-occurrence forms included.
    async fn connect_query(base: &str, id: Uuid, query: &str) -> Client {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(format!("{base}/api/games/{id}/events?{query}")),
        )
        .await
        .unwrap()
        .unwrap()
        .0
    }

    async fn next(client: &mut Client) -> serde_json::Value {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            panic!("Expected text: {message:?}");
        };
        serde_json::from_str(&text).unwrap()
    }

    async fn close(client: &mut Client) {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // The engine closes with 1000 NORMAL; a frame-absent close would
        // surface to clients as 1005 and must fail here.
        assert!(matches!(
            message,
            tokio_tungstenite::tungstenite::Message::Close(
                Some(ref f)
            ) if f.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal
        ));
        let _ = client.close(None).await;
    }

    /// `game_end` `Data` is the engine's 10-field game-object projection:
    /// field-for-field equal to the same game's `GET /games/{id}` `Game`,
    /// with `ID` the arena id and exactly those keys (no `ArenaStatus`).
    fn assert_game_end(message: &serde_json::Value, id: Uuid, game_info_game: &serde_json::Value) {
        assert_eq!(message["Type"], "game_end");
        let data = &message["Data"];
        for key in [
            "ID",
            "Status",
            "Width",
            "Height",
            "Ruleset",
            "SnakeTimeout",
            "Source",
            "RulesetName",
            "RulesStages",
            "Map",
        ] {
            assert_eq!(
                data[key], game_info_game[key],
                "game_end Data[{key}] must equal Game[{key}]"
            );
        }
        assert_eq!(data["ID"], id.to_string());
        let mut expected: Vec<&str> = [
            "ID",
            "Status",
            "Width",
            "Height",
            "Ruleset",
            "SnakeTimeout",
            "Source",
            "RulesetName",
            "RulesStages",
            "Map",
        ]
        .into();
        expected.sort_unstable();
        let mut keys: Vec<&str> = data
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, expected);
    }

    async fn game(pool: &PgPool) -> Uuid {
        sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(pool).await.unwrap()
    }

    async fn turn(pool: &PgPool, _state: &AppState, id: Uuid, number: i32) {
        crate::models::turn::create_turn(
            pool,
            id,
            number,
            Some(serde_json::json!({"Turn": number})),
        )
        .await
        .unwrap();
    }

    async fn wait_active(watched: &crate::watched_games::WatchedGames, id: Uuid) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !watched.is_active(id).await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn outage_admin(pool: &PgPool) -> (PgPool, String) {
        let db_name = sqlx::query_scalar!("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(db_name, "postgres");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone().database("postgres"))
            .await
            .unwrap();
        (admin, db_name.replace('"', "\"\""))
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn different_process_receives_every_frame_and_finish(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        assert!(!reader.watched_games.same_registry(&writer.watched_games));
        let poll_shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            poll_shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        for number in 0..8 {
            turn(&pool, &writer, id, number).await;
        }
        for number in 0..8 {
            let message = next(&mut client).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], number);
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        let game_info_game = body_json(
            get_game_info(State(reader.clone()), Path(id))
                .await
                .unwrap()
                .into_response(),
        )
        .await["Game"]
            .clone();
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;
        poll_shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn different_process_failure_and_reconnect(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        turn(&pool, &writer, id, 0).await;
        sqlx::query!("UPDATE games SET status = 'failed' WHERE game_id = $1", id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"error","Data":{"message":"Game failed"}})
        );
        let game_info_game = body_json(
            get_game_info(State(reader.clone()), Path(id))
                .await
                .unwrap()
                .into_response(),
        )
        .await["Game"]
            .clone();
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;
        let mut replay = connect(&base, id).await;
        assert_eq!(next(&mut replay).await["Data"]["Turn"], 0);
        assert_eq!(next(&mut replay).await["Data"]["message"], "Game failed");
        let message = next(&mut replay).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut replay).await;
        shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn terminal_with_stale_cursor_drains_frames_before_game_end(pool: PgPool) {
        use axum::{Router, extract::WebSocketUpgrade, routing::get};

        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        for number in 0..3 {
            turn(&pool, &state, id, number).await;
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/stale",
            get(move |upgrade: WebSocketUpgrade| {
                let state = state.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        let (mut sender, mut receiver) = socket.split();
                        let authors = std::collections::HashMap::new();
                        let suppressed = std::collections::HashMap::new();
                        let context = FrameContext {
                            authors: &authors,
                            suppressed: &suppressed,
                        };
                        let mut cursor = -1;
                        send_terminal(
                            &mut sender,
                            &mut receiver,
                            &state,
                            id,
                            GameStatus::Finished,
                            &mut cursor,
                            &context,
                        )
                        .await;
                    })
                }
            }),
        );
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stale"))
            .await
            .unwrap();
        for number in 0..3 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        shutdown.cancel();
        server.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn same_process_reconnect_replays_then_streams(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut first = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        turn(&pool, &state, id, 0).await;
        let persisted_at = std::time::Instant::now();
        assert_eq!(next(&mut first).await["Data"]["Turn"], 0);
        assert!(persisted_at.elapsed() < Duration::from_millis(500));
        first.close(None).await.unwrap();
        turn(&pool, &state, id, 1).await;
        let mut second = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..2 {
            assert_eq!(next(&mut second).await["Data"]["Turn"], number);
        }
        turn(&pool, &state, id, 2).await;
        assert_eq!(next(&mut second).await["Data"]["Turn"], 2);
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(next(&mut second).await["Type"], "game_end");
        close(&mut second).await;
        shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn listener_disconnect_catches_up_all_frames(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        for number in 0..3 {
            turn(&pool, &writer, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        let terminated = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = sqlx::query_scalar!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
                ).fetch_optional(&pool).await.unwrap();
                if result.is_some() { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        terminated.unwrap();
        for number in 3..8 {
            turn(&pool, &writer, id, number).await;
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        for number in 3..8 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while reader.watched_games.reconnect_catchups() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failed_listener_rebuild_catches_up_terminal(pool: PgPool) {
        let id = game(&pool).await;
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        turn(&pool, &reader, id, 0).await;
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);

        // Keep write and admin sessions open while new connections are denied.
        let (admin_pool, quoted_name) = outage_admin(&pool).await;
        let mut admin = admin_pool.acquire().await.unwrap();
        let mut writer = pool.acquire().await.unwrap();
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS false"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        let outage = async {
            sqlx::query!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
            )
            .fetch_one(&mut *writer)
            .await?;
            for number in 1..8 {
                sqlx::query!(
                    "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)",
                    id,
                    number,
                    serde_json::json!({"Turn": number}),
                )
                .execute(&mut *writer)
                .await?;
            }
            sqlx::query!("UPDATE games SET status = 'finished' WHERE game_id = $1", id)
                .execute(&mut *writer)
                .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while reader.watched_games.listener_errors() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok::<(), color_eyre::Report>(())
        }
        .await;
        // Restore even when an outage operation fails. No assertions occur above.
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS true"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        outage.unwrap();
        for number in 1..8 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        assert!(reader.watched_games.listener_errors() > 0);
        assert!(reader.watched_games.reconnect_catchups() > 0);
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn listener_outage_rewind_and_regrow_closes_old_stream(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..6 {
            turn(&pool, &state, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        let (admin_pool, quoted_name) = outage_admin(&pool).await;
        let mut admin = admin_pool.acquire().await.unwrap();
        let mut writer = pool.acquire().await.unwrap();
        let spare = pool.acquire().await.unwrap();
        drop(spare);
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS false"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        let outage = async {
            sqlx::query!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
            )
            .fetch_one(&mut *writer)
            .await?;
            crate::models::game::reset_game_state_for_retry(&pool, id).await?;
            for number in 0..8 {
                sqlx::query!(
                    "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)",
                    id,
                    number,
                    serde_json::json!({"Turn": number}),
                )
                .execute(&mut *writer)
                .await?;
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.watched_games.listener_errors() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok::<(), color_eyre::Report>(())
        }
        .await;
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS true"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        outage.unwrap();
        assert_eq!(
            next(&mut client).await["Data"]["message"],
            "Game restarted, please reconnect"
        );
        close(&mut client).await;
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn retry_reset_closes_old_stream_before_regrowth(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..6 {
            turn(&pool, &state, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        crate::models::game::reset_game_state_for_retry(&pool, id)
            .await
            .unwrap();
        for number in 0..8 {
            turn(&pool, &state, id, number).await;
        }
        assert_eq!(
            next(&mut client).await["Data"]["message"],
            "Game restarted, please reconnect"
        );
        close(&mut client).await;
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rolled_back_turn_has_no_frame(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        let mut transaction = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 0, $2)",
            id,
            serde_json::json!({"Turn":0})
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        transaction.rollback().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), client.next())
                .await
                .is_err()
        );
        turn(&pool, &state, id, 0).await;
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);
        client.close(None).await.unwrap();
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    // --- Archived-game websocket serving (DEV-1979) ---

    use super::fixtures as fx;
    use crate::object_store::InMemoryObjectStore;
    use crate::object_store::ObjectStore as _;
    use std::sync::Arc;

    fn ws_snake_public_shape(snake: &serde_json::Value) {
        assert_eq!(snake["URL"], "");
        assert_eq!(snake["ProxyURL"], "");
        assert_eq!(snake["APIVersion"], "");
        assert_eq!(snake["StatusCode"], 0);
        assert_eq!(snake["Error"], "");
        assert_eq!(snake["IsBot"], false);
        assert_eq!(snake["IsEnvironment"], false);
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_websocket_streams_frames_then_game_end(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, fx::v1_archive(&engine_id))
            .await
            .unwrap();
        let game_id = fx::fixture_archived_game(&pool, &engine_id, &gcs_path)
            .await
            .unwrap();
        let state = fx::archived_state(pool, store);
        let play_id = Uuid::parse_str(&engine_id).unwrap();

        let game_info_game = body_json(
            get_game_info(State(state.clone()), Path(play_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await["Game"]
            .clone();
        let (base, web_shutdown, web) = start(state).await;
        for id in [play_id, game_id] {
            let mut client = connect(&base, id).await;
            for turn in 0..2 {
                let message = next(&mut client).await;
                assert_eq!(message["Type"], "frame");
                assert_eq!(message["Data"]["Turn"], turn);
                ws_snake_public_shape(&message["Data"]["Snakes"][0]);
            }
            let message = next(&mut client).await;
            assert_game_end(&message, game_id, &game_info_game);
            close(&mut client).await;
        }
        web_shutdown.cancel();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_websocket_missing_object_closes_socket(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        let game_id = fx::fixture_archived_game(&pool, &engine_id, &gcs_path)
            .await
            .unwrap();
        let state = fx::archived_state(pool, store);

        let (base, web_shutdown, web) = start(state).await;
        let mut client = connect(&base, game_id).await;
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"error","Data":{"message":"Game not found"}})
        );
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_websocket_storage_error_closes_socket(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, fx::v1_archive(&engine_id))
            .await
            .unwrap();
        let game_id = fx::fixture_archived_game(&pool, &engine_id, &gcs_path)
            .await
            .unwrap();
        store.fail_next_get();
        let state = fx::archived_state(pool, store);

        let (base, web_shutdown, web) = start(state).await;
        let mut client = connect(&base, game_id).await;
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"error","Data":{"message":"Internal server error"}})
        );
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();
    }

    /// One viewer load — info, two frames pages, one websocket — triggers
    /// exactly one object fetch while the game stays cached.
    #[sqlx::test(migrations = "../migrations")]
    async fn archive_is_fetched_once_while_cached(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, fx::v1_archive(&engine_id))
            .await
            .unwrap();
        let game_id = fx::fixture_archived_game(&pool, &engine_id, &gcs_path)
            .await
            .unwrap();
        let state = fx::archived_state(pool, store.clone());
        let play_id = Uuid::parse_str(&engine_id).unwrap();

        get_game_info(State(state.clone()), Path(play_id))
            .await
            .unwrap();
        for (offset, limit) in [(0, 1), (1, 100)] {
            get_game_frames(
                State(state.clone()),
                Path(play_id),
                Query(FramesQuery {
                    offset: Some(offset),
                    limit: Some(limit),
                }),
            )
            .await
            .unwrap();
        }

        let (base, web_shutdown, web) = start(state).await;
        let mut client = connect(&base, game_id).await;
        for _ in 0..2 {
            assert_eq!(next(&mut client).await["Type"], "frame");
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();

        assert_eq!(store.get_count(), 1);
    }

    /// A native game serves byte-identically before and after its `turns`
    /// rows are pruned (DEV-1981): `Game` from the arena row both times,
    /// frames from the archive after pruning, authors filled from
    /// `game_battlesnakes`.
    #[sqlx::test(migrations = "../migrations")]
    async fn pruned_native_game_serves_from_archive_like_db(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let (game_id, snake_id) = fx::fixture_native_game(&pool).await.unwrap();
        let native_path = format!("native/2026/10/31/{game_id}.json.zst");
        store
            .put(
                &native_path,
                fx::native_archive(&game_id.to_string(), &snake_id.to_string()),
            )
            .await
            .unwrap();
        let state = fx::archived_state(pool.clone(), store);

        // (a) DB path: turns present, archive ignored.
        for turn in 0..2 {
            crate::models::turn::create_turn(
                &pool,
                game_id,
                turn,
                Some(fx::native_frame_value(turn, &snake_id.to_string())),
            )
            .await
            .unwrap();
        }
        let db_info = body_json(
            get_game_info(State(state.clone()), Path(game_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await;

        // (b) Archive path: prune turns, serve again, `Game` is byte-identical.
        sqlx::query("DELETE FROM turns WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let archive_info = body_json(
            get_game_info(State(state.clone()), Path(game_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await;
        assert_eq!(db_info["Game"], archive_info["Game"]);

        // (c) The archive path's last frame: author filled from
        // `game_battlesnakes`, private fields blank.
        let last = &archive_info["LastFrame"];
        assert_eq!(last["Turn"], 1);
        assert_eq!(last["Snakes"][0]["Author"], "test-user-424242");
        assert_eq!(last["Snakes"][0]["ID"], snake_id.to_string());
        ws_snake_public_shape(&last["Snakes"][0]);

        // (d) Frames pages.
        let page = body_json(
            get_game_frames(
                State(state.clone()),
                Path(game_id),
                Query(FramesQuery {
                    offset: Some(0),
                    limit: Some(100),
                }),
            )
            .await
            .unwrap()
            .into_response(),
        )
        .await;
        assert_eq!(page["Count"], 2);
        assert_eq!(page["Frames"][0]["Turn"], 0);
        assert_eq!(page["Frames"][1]["Turn"], 1);
        let past_end = body_json(
            get_game_frames(
                State(state.clone()),
                Path(game_id),
                Query(FramesQuery {
                    offset: Some(2),
                    limit: Some(100),
                }),
            )
            .await
            .unwrap()
            .into_response(),
        )
        .await;
        assert_eq!(past_end["Count"], 0);

        // (e) The websocket replays every frame, then game_end, then close.
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, game_id).await;
        for turn in 0..2 {
            let message = next(&mut client).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], turn);
            assert_eq!(message["Data"]["Snakes"][0]["Author"], "test-user-424242");
            ws_snake_public_shape(&message["Data"]["Snakes"][0]);
        }
        let message = next(&mut client).await;
        assert_game_end(&message, game_id, &archive_info["Game"]);
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();
    }

    /// The archived capture's websocket: 78 frames ascending (no dupes),
    /// then `game_end` whose `Data` matches the capture's
    /// `nonFrame[0]["Data"]` on the nine non-ID fields (ID is the arena
    /// id, DEV-1979), then a close with code 1000 like the engine.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_websocket_game_end_matches_engine(pool: PgPool) {
        let (state, arena_id) = fx::capture_fixture(&pool).await.unwrap();
        let play_id = Uuid::parse_str(fx::CAPTURE_ADDEDB91).unwrap();

        let (base, web_shutdown, web) = start(state).await;
        let mut client = connect(&base, play_id).await;
        for turn in 0..78 {
            let message = next(&mut client).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], turn, "frames ascending, no dupes");
        }
        let engine_data = &fx::capture_ws_summary()["nonFrame"][0]["Data"];
        let message = next(&mut client).await;
        assert_eq!(message["Type"], "game_end");
        let data = &message["Data"];
        for key in [
            "Status",
            "Width",
            "Height",
            "Ruleset",
            "SnakeTimeout",
            "Source",
            "RulesetName",
            "RulesStages",
            "Map",
        ] {
            assert_eq!(data[key], engine_data[key], "game_end Data[{key}]");
        }
        assert_eq!(data["ID"], arena_id.to_string());
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();
    }

    /// `?startFrame=N`: starts at turn N; missing/unparseable/empty → 0;
    /// N past the end → no frames, straight to game_end; repeated param
    /// → first wins (raw-query parse, never a serde 400).
    #[sqlx::test(migrations = "../migrations")]
    async fn websocket_startframe_skips_earlier_turns(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        for number in 0..=3 {
            turn(&pool, &state, id, number).await;
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        let game_info_game = body_json(
            get_game_info(State(state.clone()), Path(id))
                .await
                .unwrap()
                .into_response(),
        )
        .await["Game"]
            .clone();
        let (base, web_shutdown, web) = start(state).await;

        // startFrame=2 → turns 2,3 then game_end.
        let mut client = connect_query(&base, id, "startFrame=2").await;
        for turn in [2, 3] {
            let message = next(&mut client).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], turn);
        }
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;

        // Unparseable, empty, and missing → start at turn 0.
        for query in ["startFrame=banana", "startFrame="] {
            let mut client = connect_query(&base, id, query).await;
            for turn in 0..=3 {
                assert_eq!(next(&mut client).await["Data"]["Turn"], turn, "{query}");
            }
            let message = next(&mut client).await;
            assert_game_end(&message, id, &game_info_game);
            close(&mut client).await;
        }
        let mut client = connect(&base, id).await;
        for turn in 0..=3 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], turn, "no param");
        }
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;

        // startFrame=99 → no frames, straight to game_end.
        let mut client = connect_query(&base, id, "startFrame=99").await;
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;

        // First occurrence wins: starts at 2 despite the trailing 0
        // (a `Query` extractor would have 400'd on the duplicate).
        let mut client = connect_query(&base, id, "startFrame=2&startFrame=0").await;
        for turn in [2, 3] {
            assert_eq!(next(&mut client).await["Data"]["Turn"], turn);
        }
        let message = next(&mut client).await;
        assert_game_end(&message, id, &game_info_game);
        close(&mut client).await;

        web_shutdown.cancel();
        web.await.unwrap();
    }

    /// The archived websocket honors `?startFrame` too: a two-frame v1
    /// archive with `startFrame=1` replays only turn 1, then game_end.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_websocket_startframe_skips_earlier_turns(pool: PgPool) {
        let store = Arc::new(InMemoryObjectStore::default());
        let engine_id = Uuid::new_v4().to_string();
        let gcs_path = format!("games/2026/01/01/{engine_id}.json.zst");
        store
            .put(&gcs_path, fx::v1_archive(&engine_id))
            .await
            .unwrap();
        let game_id = fx::fixture_archived_game(&pool, &engine_id, &gcs_path)
            .await
            .unwrap();
        let state = fx::archived_state(pool, store);

        let game_info_game = body_json(
            get_game_info(State(state.clone()), Path(game_id))
                .await
                .unwrap()
                .into_response(),
        )
        .await["Game"]
            .clone();
        let (base, web_shutdown, web) = start(state).await;
        let mut client = connect_query(&base, game_id, "startFrame=1").await;
        let message = next(&mut client).await;
        assert_eq!(message["Type"], "frame");
        assert_eq!(message["Data"]["Turn"], 1);
        let message = next(&mut client).await;
        assert_game_end(&message, game_id, &game_info_game);
        close(&mut client).await;
        web_shutdown.cancel();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "controlled local load run: 500 sockets and a job worker"]
    async fn watched_games_local_load_harness(pool: PgPool) {
        use std::{collections::HashMap, sync::Arc, time::Instant};
        use tokio::sync::{Mutex, mpsc};
        let ids = futures::future::join_all((0..50).map(|_| game(&pool))).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let watched = reader.watched_games.clone();
        let poll_shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            watched.clone(),
            poll_shutdown.clone(),
        ));
        let job_shutdown = CancellationToken::new();
        let job_config = reader.config.job.clone();
        let job_interval_ms = job_config.poll_interval_ms;
        let job_state = reader.clone();
        let job_gate = job_state.idle_poll_gate.clone();
        let job_cancel = job_shutdown.clone();
        let job_worker = tokio::spawn(async move {
            cja::jobs::worker::job_worker(
                job_state,
                crate::jobs::Jobs,
                Duration::from_millis(job_config.poll_interval_ms),
                job_config.max_retries,
                job_cancel,
                job_config.worker_config(
                    Duration::from_secs(job_config.shutdown_drain_secs),
                    job_gate,
                ),
            )
            .await
        });
        let (base, web_shutdown, web) = start(reader).await;
        let (tx, mut rx) = mpsc::unbounded_channel::<(Uuid, i32, Instant)>();
        let mut clients = tokio::task::JoinSet::new();
        for id in &ids {
            for _ in 0..10 {
                let mut socket = connect(&base, *id).await;
                let tx = tx.clone();
                let id = *id;
                clients.spawn(async move {
                    for expected in 0..2 {
                        let message = next(&mut socket).await;
                        assert_eq!(message["Type"], "frame");
                        let number = message["Data"]["Turn"].as_i64().unwrap() as i32;
                        assert_eq!(number, expected);
                        tx.send((id, number, Instant::now())).unwrap();
                    }
                    socket.close(None).await.unwrap();
                });
            }
        }
        drop(tx);
        tokio::time::timeout(Duration::from_secs(30), async {
            while watched.subscriber_count().await != 500 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let timestamps = Arc::new(Mutex::new(HashMap::<(Uuid, i32), Instant>::new()));
        let mut unwatched = 0;
        for number in 0..2 {
            for id in &ids {
                turn(&pool, &writer, *id, number).await;
                timestamps
                    .lock()
                    .await
                    .insert((*id, number), Instant::now());
                let other = game(&pool).await;
                turn(&pool, &writer, other, 0).await;
                unwatched += 1;
            }
        }
        let mut latencies = Vec::new();
        for _ in 0..1000 {
            let (id, number, received) = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let persisted = timestamps.lock().await[&(id, number)];
            latencies.push(received.saturating_duration_since(persisted));
        }
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
        latencies.sort();
        let p50 = latencies[latencies.len() / 2];
        let p95 = latencies[latencies.len() * 95 / 100];
        let (status_reads, range_reads) = watched.read_counts().await;
        println!(
            "samples={} p50={p50:?} p95={p95:?} status_reads={status_reads} range_reads={range_reads} watched_inserts=100 unwatched_inserts={unwatched} configured_job_claim_interval_ms={}",
            latencies.len(),
            job_interval_ms
        );
        assert!(p95 <= Duration::from_secs(1));
        poll_shutdown.cancel();
        web_shutdown.cancel();
        job_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
        job_worker.await.unwrap().unwrap();
    }
}
