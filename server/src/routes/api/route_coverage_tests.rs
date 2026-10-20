//! Route-coverage guard: every authenticated `/api` route must be listed in
//! [`routes`] with its required permission, and a zero-permission token must
//! get a 403 naming the missing permission on each. A fourth test cross-checks
//! the table against the `api_routes` builder in `routes.rs` (between the
//! `API-ROUTES-START`/`API-ROUTES-END` markers), so a new route fails CI until
//! it is added here. A test that never sends a Bearer token is a false green
//! (401 ≠ 403) — the assertions below are body-exact.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::api_token::{NewApiToken, Permission, SnakeAccess, create_api_token};
use crate::models::battlesnake::{Battlesnake, CreateBattlesnake, Visibility, create_battlesnake};
use crate::routes::test_support::{api_app, create_user, send_api};
use crate::state::AppState;

struct Setup {
    regular_user: Uuid,
    admin_user: Uuid,
    /// A real snake owned by the regular user.
    snake: Uuid,
    /// A throwaway snake (DELETE /snakes/{id} consumes it).
    throwaway_snake: Uuid,
    /// A throwaway zero-permission token id (DELETE /tokens/{id} consumes
    /// it). Zero permissions so every revoking token's scope covers it.
    throwaway_token_id: Uuid,
}

async fn setup(db: &PgPool) -> Setup {
    let regular_user = create_user(db, 9930001, false).await;
    let admin_user = create_user(db, 9930002, true).await;

    let make = |name: String| CreateBattlesnake {
        name,
        url: "https://coverage.example".to_string(),
        visibility: Visibility::Public,
        engine_region: Default::default(),
    };
    let Battlesnake {
        battlesnake_id: snake,
        ..
    } = create_battlesnake(db, regular_user, make("coverage-snake".into()))
        .await
        .unwrap();
    let Battlesnake {
        battlesnake_id: throwaway_snake,
        ..
    } = create_battlesnake(db, regular_user, make("coverage-throwaway".into()))
        .await
        .unwrap();

    let NewApiToken { token, .. } = create_api_token(
        db,
        regular_user,
        "coverage-throwaway",
        &[],
        SnakeAccess::All,
        &[],
    )
    .await
    .unwrap();

    Setup {
        regular_user,
        admin_user,
        snake,
        throwaway_snake,
        throwaway_token_id: token.id,
    }
}

struct CoveredRoute {
    /// "GET" | "POST" | "PUT" | "DELETE"
    method: &'static str,
    /// BYTE-EXACT route literal from routes.rs (no /api prefix).
    route: &'static str,
    /// VALID body wherever the method takes Json — extractors run before
    /// handlers, so an invalid body dies with 422, not 403.
    body: fn(&Setup) -> Option<serde_json::Value>,
    /// Required permission; `None` = public route.
    permission: Option<Permission>,
    /// Mint the token for the admin user (admin routes).
    admin_user: bool,
}

fn covered(
    method: &'static str,
    route: &'static str,
    body: fn(&Setup) -> Option<serde_json::Value>,
    permission: Option<Permission>,
    admin_user: bool,
) -> CoveredRoute {
    CoveredRoute {
        method,
        route,
        body,
        permission,
        admin_user,
    }
}

fn no_body(_: &Setup) -> Option<serde_json::Value> {
    None
}

/// Every route on the `api_routes` builder in `server/src/routes.rs`.
/// `route` must match the router literal byte for byte; the parser test
/// below enforces it.
fn routes() -> Vec<CoveredRoute> {
    vec![
        covered(
            "POST",
            "/tokens",
            // Name-only: the new token copies the caller's own scope.
            |_s| Some(serde_json::json!({"name": "coverage"})),
            Some(Permission::TokensManage),
            false,
        ),
        covered(
            "GET",
            "/tokens",
            no_body,
            Some(Permission::TokensManage),
            false,
        ),
        covered(
            "DELETE",
            "/tokens/{id}",
            no_body,
            Some(Permission::TokensManage),
            false,
        ),
        covered("GET", "/me", no_body, Some(Permission::ProfileRead), false),
        covered(
            "GET",
            "/snakes",
            no_body,
            Some(Permission::SnakesRead),
            false,
        ),
        covered(
            "POST",
            "/snakes",
            |_s| {
                Some(serde_json::json!({
                    "name": "coverage-created",
                    "url": "https://coverage.example",
                    "is_public": true,
                }))
            },
            Some(Permission::SnakesCreate),
            false,
        ),
        covered(
            "GET",
            "/snakes/{id}",
            no_body,
            Some(Permission::SnakesRead),
            false,
        ),
        covered(
            "PUT",
            "/snakes/{id}",
            // is_public-only: no moderation call, no name-uniqueness hazard.
            |_s| Some(serde_json::json!({"is_public": true})),
            Some(Permission::SnakeEdit),
            false,
        ),
        covered(
            "DELETE",
            "/snakes/{id}",
            no_body,
            Some(Permission::SnakeDelete),
            false,
        ),
        covered(
            "POST",
            "/games",
            |s| {
                Some(serde_json::json!({
                    "snakes": [s.snake.to_string()],
                }))
            },
            Some(Permission::GamesCreate),
            false,
        ),
        covered("GET", "/games", no_body, Some(Permission::GamesRead), false),
        covered(
            "GET",
            "/games/{id}/details",
            no_body,
            Some(Permission::GamesRead),
            false,
        ),
        covered(
            "POST",
            "/games/status",
            |_s| Some(serde_json::json!({"game_ids": []})),
            Some(Permission::GamesRead),
            false,
        ),
        covered(
            "GET",
            "/admin/stats",
            no_body,
            Some(Permission::Admin),
            true,
        ),
        covered("GET", "/stats", no_body, Some(Permission::Admin), true),
        covered(
            "POST",
            "/leaderboards/{id}/entries",
            |s| {
                Some(serde_json::json!({
                    "battlesnake_id": s.snake.to_string(),
                }))
            },
            Some(Permission::SnakeCompete),
            false,
        ),
        covered(
            "DELETE",
            "/leaderboards/{id}/entries/{battlesnake_id}",
            no_body,
            Some(Permission::SnakeCompete),
            false,
        ),
        // Public routes — no auth extractor at all.
        covered("GET", "/games/{id}", no_body, None, false),
        covered("GET", "/games/{id}/events", no_body, None, false),
        covered("GET", "/games/{id}/frames", no_body, None, false),
        covered("GET", "/games/{id}/move-request", no_body, None, false),
        covered("GET", "/leaderboards", no_body, None, false),
        covered("GET", "/leaderboards/{id}/rankings", no_body, None, false),
    ]
}

/// Build the concrete request URI for a row. Param segments get setup-owned
/// IDs where the handler's own semantics need them, a random UUID elsewhere
/// (the permission check precedes existence checks by design).
fn concrete_path(row: &CoveredRoute, s: &Setup) -> String {
    let random = Uuid::new_v4().to_string();
    let substituted = row
        .route
        .replace("{battlesnake_id}", &s.snake.to_string())
        .replace("{id}", &random);
    match (row.method, row.route) {
        // DELETE the throwaway token, not a random one.
        ("DELETE", "/tokens/{id}") => format!("/api/tokens/{}", s.throwaway_token_id),
        // Real owned snake for GET/PUT.
        ("GET", "/snakes/{id}") | ("PUT", "/snakes/{id}") => format!("/api/snakes/{}", s.snake),
        // DELETE the throwaway snake.
        ("DELETE", "/snakes/{id}") => format!("/api/snakes/{}", s.throwaway_snake),
        _ => format!("/api{substituted}"),
    }
}

async fn mint(db: &PgPool, user_id: Uuid, permissions: &[Permission]) -> String {
    create_api_token(db, user_id, "coverage", permissions, SnakeAccess::All, &[])
        .await
        .unwrap()
        .secret
}

#[sqlx::test(migrations = "../migrations")]
async fn zero_permission_token_gets_403_on_every_authenticated_route(db: PgPool) {
    let state = AppState::test_from_pool(db.clone());
    let app = api_app(&state);
    let s = setup(&db).await;

    for row in routes().iter().filter(|r| r.permission.is_some()) {
        let user = if row.admin_user {
            s.admin_user
        } else {
            s.regular_user
        };
        // `all` access so the permission branch (not the access branch)
        // produces the body; the two-step AdminApiUser produces it for admin
        // rows too ("Admin access required" only fires for non-admin users).
        let token = mint(&db, user, &[]).await;
        let (status, body) = send_api(
            &app,
            row.method,
            &concrete_path(row, &s),
            Some(&token),
            (row.body)(&s),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::FORBIDDEN,
            "{} {}: body: {body}",
            row.method,
            row.route
        );
        assert!(
            body.contains("missing permission"),
            "{} {}: expected a permission 403, got body: {body}",
            row.method,
            row.route
        );
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn exactly_listed_permission_passes_auth(db: PgPool) {
    let state = AppState::test_from_pool(db.clone());
    let app = api_app(&state);
    let s = setup(&db).await;

    for row in routes().iter().filter(|r| r.permission.is_some()) {
        let user = if row.admin_user {
            s.admin_user
        } else {
            s.regular_user
        };
        let permission = row.permission.unwrap();
        let token = mint(&db, user, &[permission]).await;
        let (status, body) = send_api(
            &app,
            row.method,
            &concrete_path(row, &s),
            Some(&token),
            (row.body)(&s),
        )
        .await;
        assert!(
            status != axum::http::StatusCode::UNAUTHORIZED
                && status != axum::http::StatusCode::FORBIDDEN,
            "{} {}: expected to pass auth, got {status} — body: {body}",
            row.method,
            row.route
        );
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn public_api_routes_need_no_auth(db: PgPool) {
    let state = AppState::test_from_pool(db.clone());
    let app = api_app(&state);
    let s = setup(&db).await;

    for row in routes().iter().filter(|r| r.permission.is_none()) {
        // No Authorization header at all.
        let (status, body) = send_api(
            &app,
            row.method,
            &concrete_path(row, &s),
            None,
            (row.body)(&s),
        )
        .await;
        assert!(
            status != axum::http::StatusCode::UNAUTHORIZED
                && status != axum::http::StatusCode::FORBIDDEN,
            "{} {}: public route must not require auth, got {status} — body: {body}",
            row.method,
            row.route
        );
    }
}

/// The table must cover exactly the `(method, route)` pairs of the
/// `api_routes` builder in `routes.rs` — new routes fail here until added.
#[test]
fn table_covers_every_route_in_the_router() {
    let source = include_str!("../../routes.rs");
    let start = source
        .find("// API-ROUTES-START\n")
        .expect("API-ROUTES-START marker")
        + "// API-ROUTES-START\n".len();
    let end = source
        .find("// API-ROUTES-END")
        .expect("API-ROUTES-END marker");
    let api_section = &source[start..end];

    let mut router_routes: Vec<(&str, &str)> = Vec::new();
    for chunk in api_section.split(".route(").skip(1) {
        // The first quoted string after `.route(` is the route literal.
        let route = chunk.split('"').nth(1).expect("route literal");
        // Collect EVERY method call in this chunk (before the next `.route(`)
        // so a chained method router like `get(a).post(b)` registers both —
        // each needs its own table row. The boundary check rejects
        // `widget_get(` matching `get(`. Arena is on axum 0.8 — params are
        // `{param}`.
        const METHODS: [(&str, &str); 5] = [
            ("get(", "GET"),
            ("post(", "POST"),
            ("put(", "PUT"),
            ("delete(", "DELETE"),
            ("patch(", "PATCH"),
        ];
        let chunk_methods: Vec<&str> = METHODS
            .iter()
            .filter(|(needle, _)| {
                chunk.match_indices(needle).any(|(pos, _)| {
                    pos == 0
                        || !chunk[..pos]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            })
            .map(|(_, method)| *method)
            .collect();
        assert!(
            !chunk_methods.is_empty(),
            "no method call found in .route({route}..."
        );
        for method in chunk_methods {
            router_routes.push((method, route));
        }
    }

    let mut table_routes: Vec<(&str, &str)> =
        routes().iter().map(|row| (row.method, row.route)).collect();

    router_routes.sort_unstable();
    table_routes.sort_unstable();

    assert_eq!(
        router_routes, table_routes,
        "route table out of sync with routes.rs — add the new route to ROUTES in route_coverage_tests.rs"
    );
}
