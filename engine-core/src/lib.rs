//! Shared game engine for arena and the battlesnake CLI: turns `rules` into
//! games. Everything here is sync, side-effect-free, and free of HTTP: the
//! server (and later the battlesnake CLI) drives it. Proxy routing and frame
//! storage stay in the server.

pub mod game;
pub mod move_response;
pub mod wire;

pub use game::{
    EngineGame, GameMeta, GameMode, GameResult, GameSource, InitialSnake, MAX_TURNS,
    MOVE_TIMEOUT_MS, advance_turn, apply_turn, create_initial_game, is_game_over, mode_rules,
    run_game_with_random_moves, seeded_game, spawn_food,
};
