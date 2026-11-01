//! The engine-host routing surface: the legacy engine's public read API
//! served at the engine's own paths for engine hostnames
//! (`AppConfig::engine_api_hosts`, default `engine.battlesnake.com`).
//!
//! Contract:
//! * The engine host serves ONLY the allowlisted read endpoints below;
//!   every other path 404s with an empty body — arena's site, cookies,
//!   and OAuth are never reachable under the engine hostname.
//! * No redirects: the websocket upgrade answers 101 directly (this
//!   router has no redirect layer), because browsers fail a websocket
//!   handshake that receives a 3xx.
//! * Host dispatch itself lives in `cja::server::App` — it routes by the
//!   Host header BEFORE route matching (`Router::layer` would be too
//!   late). This module only builds the engine router.
//! * The game handlers are the same functions the `/api/...` routes use,
//!   so "returns exactly what `/api/...` returns" holds by construction.

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::cors::CorsLayer;

use crate::state::AppState;

/// Build the engine-host router (state applied). Reuses the `/api` game
/// handlers verbatim; only `/version`, `/healthz/*`, and the 404 fallback
/// are engine-host-specific. The fallback is registered BEFORE the layers
/// so CORS headers and trace context also cover 404s (axum layers only
/// wrap what exists at call time).
pub fn engine_router(app_state: AppState, cors: CorsLayer) -> Router {
    Router::new()
        .route("/games/{id}", get(crate::routes::game::get_game_info))
        .route(
            "/games/{id}/events",
            get(crate::routes::game::game_events_websocket),
        )
        .route(
            "/games/{id}/frames",
            get(crate::routes::game::get_game_frames),
        )
        .route("/version", get(version_text))
        .route("/healthz/alive", get(alive_text))
        .route("/healthz/ready", get(ready_text))
        .fallback(engine_host_not_found)
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::routes::inject_trace_context,
        ))
        .layer(cors)
        .with_state(app_state)
}

/// The engine's `GET /version`: the running build's git SHA as plain text.
async fn version_text() -> impl IntoResponse {
    (
        [("content-type", "text/plain; charset=utf-8")],
        env!("VERGEN_GIT_SHA"),
    )
}

/// The engine's `GET /healthz/alive`. No DB probe: the engine's liveness
/// was a process check, and the endpoint must stay stateless/DB-write-free.
async fn alive_text() -> &'static str {
    "alive"
}

/// The engine's `GET /healthz/ready`.
async fn ready_text() -> &'static str {
    "ready"
}

/// Everything not on the allowlist 404s with an empty body. The engine's
/// own 404 had no body; never a redirect, never the branded page.
async fn engine_host_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
