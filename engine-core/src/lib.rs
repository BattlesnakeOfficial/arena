//! Shared game engine for arena and the battlesnake CLI: turns `rules` into
//! games. Everything here is sync and side-effect-free apart from
//! `http_util`: there is no HTTP *client* in this crate (construction,
//! proxy routing, and logging stay with each caller), but the shared
//! HTTP-boundary helpers a snake caller needs — capped body reads and
//! endpoint URL building — live in `http_util` (async, on `reqwest` types).
//! Proxy routing and frame storage stay in the server.

pub mod game;
pub mod http_util;
pub mod move_response;
pub mod placement;
pub mod wire;

pub use game::{
    EngineGame, GameMeta, GameMode, GameResult, GameSource, InitialSnake, MAX_TURNS,
    MOVE_TIMEOUT_MS, advance_turn, apply_turn, create_initial_game, is_game_over, is_game_over_for,
    mode_rules, run_game_with_random_moves, seeded_game, spawn_food,
};
