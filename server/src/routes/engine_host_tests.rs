//! Host-routing tests for the engine API (DEV-1988).
//!
//! Every test drives `routes(state).into_router()` — the exact cja stack
//! (TraceLayer + CookieManagerLayer per branch, Host dispatch before route
//! matching) that production serves. cja unit-tests its own host-matching
//! table; these tests pin arena's wiring: what the engine host serves, what
//! it must never serve, CORS parity with `/api`, span labels, and the
//! websocket answering 101 directly.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::routes::test_support::send_with_host;
use crate::state::AppState;

const ENGINE_HOST: &str = "engine.battlesnake.com";

fn app(state: &AppState) -> axum::Router {
    crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router()
}

/// Insert a bare finished game row (11x11 Standard) + one turn frame.
async fn seed_game_with_turn(pool: &PgPool) -> Uuid {
    let game_id: Uuid = sqlx::query_scalar(
        "INSERT INTO games (board_size, game_type, status)
         VALUES ('11x11', 'Standard', 'finished') RETURNING game_id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 0, $2)")
        .bind(game_id)
        .bind(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []}))
        .execute(pool)
        .await
        .unwrap();
    game_id
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// All `access-control-*` response headers (name + every value), sorted —
/// the full CORS surface, so tests can demand byte-identical sets.
fn cors_headers(response: &axum::response::Response) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in response.headers() {
        if !name
            .as_str()
            .to_ascii_lowercase()
            .starts_with("access-control-")
        {
            continue;
        }
        let value = value.to_str().unwrap().to_string();
        if let Some(entry) = out.iter_mut().find(|(n, _)| n == name.as_str()) {
            entry.1.push(value);
        } else {
            out.push((name.as_str().to_string(), vec![value]));
        }
    }
    out.sort();
    out
}

#[sqlx::test(migrations = "../migrations")]
async fn engine_host_serves_engine_read_api(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let game_id = seed_game_with_turn(&pool).await;
    let router = app(&state);

    // The engine host serves exactly what the matching /api route serves.
    let pairs = [
        (format!("/games/{game_id}"), format!("/api/games/{game_id}")),
        (
            format!("/games/{game_id}/frames"),
            format!("/api/games/{game_id}/frames"),
        ),
    ];
    for (engine_path, arena_path) in pairs {
        let engine = send_with_host(&router, "GET", &engine_path, ENGINE_HOST, &[]).await;
        assert_eq!(engine.status(), axum::http::StatusCode::OK, "{engine_path}");
        let arena = send_with_host(&router, "GET", &arena_path, "arena.battlesnake.com", &[]).await;
        assert_eq!(arena.status(), axum::http::StatusCode::OK, "{arena_path}");
        assert_eq!(
            body_string(engine).await,
            body_string(arena).await,
            "engine host {engine_path} must equal arena host {arena_path}"
        );
    }

    let info: serde_json::Value = serde_json::from_str(
        &body_string(
            send_with_host(
                &router,
                "GET",
                &format!("/games/{game_id}"),
                ENGINE_HOST,
                &[],
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(info["Game"]["Width"], 11);
    assert_eq!(info["Game"]["Height"], 11);

    // The archived Play game resolves by its Play id on the engine host —
    // the main post-cutover use: the board's default engine loading old
    // Play links.
    let (state, _arena_id) = crate::routes::game::api::fixtures::capture_fixture(&pool)
        .await
        .unwrap();
    let router = app(&state);
    let play_id = crate::routes::game::api::fixtures::CAPTURE_ADDEDB91;
    let engine = send_with_host(
        &router,
        "GET",
        &format!("/games/{play_id}"),
        ENGINE_HOST,
        &[],
    )
    .await;
    assert_eq!(engine.status(), axum::http::StatusCode::OK);
    let engine_body = body_string(engine).await;
    let archived: serde_json::Value = serde_json::from_str(&engine_body).unwrap();
    assert_eq!(archived["Game"]["Width"], 11);
    assert_eq!(archived["Game"]["Height"], 11);
    let arena = send_with_host(
        &router,
        "GET",
        &format!("/api/games/{play_id}"),
        "arena.battlesnake.com",
        &[],
    )
    .await;
    assert_eq!(engine_body, body_string(arena).await);
}

#[sqlx::test(migrations = "../migrations")]
async fn engine_host_cors_byte_identical(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let game_id = seed_game_with_turn(&pool).await;
    // The archived Play game gets the same CORS surface.
    let (archived_state, _arena_id) = crate::routes::game::api::fixtures::capture_fixture(&pool)
        .await
        .unwrap();
    let play_id = crate::routes::game::api::fixtures::CAPTURE_ADDEDB91;

    let router = app(&state);
    let archived_router = app(&archived_state);

    let origin = ("origin", "https://board.battlesnake.com");
    let games = [
        (
            router.clone(),
            format!("/games/{game_id}"),
            format!("/api/games/{game_id}"),
        ),
        (
            archived_router.clone(),
            format!("/games/{play_id}"),
            format!("/api/games/{play_id}"),
        ),
    ];

    for (router, engine_path, arena_path) in games {
        let engine = send_with_host(&router, "GET", &engine_path, ENGINE_HOST, &[origin]).await;
        assert_eq!(engine.status(), axum::http::StatusCode::OK, "{engine_path}");
        let arena = send_with_host(
            &router,
            "GET",
            &arena_path,
            "arena.battlesnake.com",
            &[origin],
        )
        .await;
        assert_eq!(arena.status(), axum::http::StatusCode::OK, "{arena_path}");
        assert_eq!(
            cors_headers(&engine),
            cors_headers(&arena),
            "CORS surface must be identical for {engine_path} vs {arena_path}"
        );
        assert!(
            !cors_headers(&engine).is_empty(),
            "the engine host must send CORS headers"
        );
    }

    // Preflight: CorsLayer answers at the layer, before routing, so the
    // engine router's GET-only route set does not matter.
    let preflight = [
        origin,
        ("access-control-request-method", "GET"),
        ("access-control-request-headers", "authorization"),
    ];
    let engine = send_with_host(
        &router,
        "OPTIONS",
        &format!("/games/{game_id}"),
        ENGINE_HOST,
        &preflight,
    )
    .await;
    let arena = send_with_host(
        &router,
        "OPTIONS",
        &format!("/api/games/{game_id}"),
        "arena.battlesnake.com",
        &preflight,
    )
    .await;
    assert_eq!(engine.status(), arena.status());
    assert_eq!(cors_headers(&engine), cors_headers(&arena));
}

#[sqlx::test(migrations = "../migrations")]
async fn engine_host_version_and_healthz(pool: PgPool) {
    let state = AppState::test_from_pool(pool);
    let router = app(&state);

    let version = send_with_host(&router, "GET", "/version", ENGINE_HOST, &[]).await;
    assert_eq!(version.status(), axum::http::StatusCode::OK);
    assert!(
        version.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert_eq!(body_string(version).await, env!("VERGEN_GIT_SHA"));

    let alive = send_with_host(&router, "GET", "/healthz/alive", ENGINE_HOST, &[]).await;
    assert_eq!(alive.status(), axum::http::StatusCode::OK);
    assert_eq!(body_string(alive).await, "alive");

    let ready = send_with_host(&router, "GET", "/healthz/ready", ENGINE_HOST, &[]).await;
    assert_eq!(ready.status(), axum::http::StatusCode::OK);
    assert_eq!(body_string(ready).await, "ready");
}

/// Everything off the allowlist 404s with an empty body — never the
/// branded page, never a redirect, and arena's site/auth/api are
/// unreachable under the engine hostname. Status + empty body only:
/// never assert the Content-Type header (prod's Google Front End injects
/// `text/html` on empty 404s while local axum omits it).
#[sqlx::test(migrations = "../migrations")]
async fn engine_host_404s_everything_else(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let game_id = seed_game_with_turn(&pool).await;
    let router = app(&state);

    let paths = [
        "/".to_string(),
        format!("/api/games/{game_id}"),
        "/auth/github".to_string(),
        "/health".to_string(),
        "/_/version".to_string(),
        "/me".to_string(),
        "/leaderboards".to_string(),
        "/favicon.ico".to_string(),
        "/robots.txt".to_string(),
    ];
    for path in paths {
        let response = send_with_host(&router, "GET", &path, ENGINE_HOST, &[]).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{path} must 404 on the engine host"
        );
        assert!(
            body_string(response).await.is_empty(),
            "{path} must have an empty body"
        );
    }
    let response = send_with_host(
        &router,
        "POST",
        &format!("/games/{game_id}/rematch"),
        ENGINE_HOST,
        &[],
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(body_string(response).await.is_empty());
}

/// Arena's one host-matching test (cja unit-tests its own table): engine
/// hosts (case-insensitive, port stripped) reach the engine router; arena,
/// Cloud Run hosts, and no-Host requests reach the site router.
#[sqlx::test(migrations = "../migrations")]
async fn host_dispatch_wiring(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let game_id = seed_game_with_turn(&pool).await;
    let router = app(&state);

    for host in ["engine.battlesnake.com", "ENGINE.BATTLESNAKE.COM:443"] {
        let response = send_with_host(&router, "GET", "/version", host, &[]).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{host}");
        assert_eq!(body_string(response).await, env!("VERGEN_GIT_SHA"));
    }

    for host in ["arena.battlesnake.com", "tf-arena-xxxx-uc.a.run.app"] {
        let home = send_with_host(&router, "GET", "/", host, &[]).await;
        assert_eq!(home.status(), axum::http::StatusCode::OK, "{host} /");
        assert!(
            home.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains("html")
        );

        let api = send_with_host(&router, "GET", &format!("/api/games/{game_id}"), host, &[]).await;
        assert_eq!(api.status(), axum::http::StatusCode::OK, "{host} /api");

        // The site's HTML game page — the path the engine host also uses
        // for JSON; on the arena host it stays the HTML page.
        let page = send_with_host(&router, "GET", &format!("/games/{game_id}"), host, &[]).await;
        assert_eq!(page.status(), axum::http::StatusCode::OK, "{host} /games");
        assert!(
            page.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains("html")
        );
    }

    // The engine-host JSON equals the arena-host /api JSON for the same game.
    let engine_body = body_string(
        send_with_host(
            &router,
            "GET",
            &format!("/games/{game_id}"),
            ENGINE_HOST,
            &[],
        )
        .await,
    )
    .await;
    let arena_body = body_string(
        send_with_host(
            &router,
            "GET",
            &format!("/api/games/{game_id}"),
            "arena.battlesnake.com",
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(engine_body, arena_body);

    // No Host header at all → the default (site) router.
    use tower::ServiceExt as _;
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

/// `http.route` survives host dispatch: engine-host requests carry the
/// engine-router templates, arena-host requests the site templates, and
/// the engine 404 fallback is the only `""` case. The span and its fields
/// are created at request time, so capture happens at `on_new_span`.
#[sqlx::test(migrations = "../migrations")]
async fn http_route_labels_survive_host_dispatch(pool: PgPool) {
    use tracing::field::Visit;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Default)]
    struct RouteVisitor {
        route: Option<String>,
    }

    impl Visit for RouteVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "http.route" {
                self.route = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }

    struct RouteCapture {
        routes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for RouteCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() != "server.request" {
                return;
            }
            let mut visitor = RouteVisitor::default();
            attrs.record(&mut visitor);
            // A server.request span without the field must fail loudly,
            // never record vacuously.
            self.routes.lock().unwrap().push(
                visitor
                    .route
                    .expect("server.request span must carry http.route"),
            );
        }
    }

    let state = AppState::test_from_pool(pool.clone());
    let game_id = seed_game_with_turn(&pool).await;
    let router = app(&state);
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let subscriber = tracing_subscriber::registry().with(RouteCapture {
        routes: std::sync::Arc::clone(&captured),
    });
    // Thread-local default: sqlx::test runs current-thread tokio, so the
    // guard covers the awaited oneshot calls.
    let _guard = tracing::subscriber::set_default(subscriber);

    let cases = [
        ("arena.battlesnake.com", format!("/api/games/{game_id}")), // "/api/games/{id}"
        ("arena.battlesnake.com", format!("/games/{game_id}")),     // "/games/{id}"
        (ENGINE_HOST, format!("/games/{game_id}")),                 // "/games/{id}"
        (ENGINE_HOST, format!("/games/{game_id}/frames")),          // "/games/{id}/frames"
        // A plain GET (no upgrade headers) still routes to the events
        // route before the handshake extractor rejects it.
        (ENGINE_HOST, format!("/games/{game_id}/events")), // "/games/{id}/events"
        (ENGINE_HOST, "/nope".to_string()),                // "" (the 404 fallback)
    ];
    let expected = [
        "/api/games/{id}",
        "/games/{id}",
        "/games/{id}",
        "/games/{id}/frames",
        "/games/{id}/events",
        "",
    ];
    for ((host, path), want) in cases.iter().zip(expected) {
        let before = captured.lock().unwrap().len();
        let response = send_with_host(&router, "GET", path, host, &[]).await;
        // Drain the response so the request fully completes.
        let _ = body_string(response).await;
        let routes = captured.lock().unwrap();
        let new: Vec<&String> = routes.iter().skip(before).collect();
        assert_eq!(
            new.len(),
            1,
            "exactly one server.request span for {host}{path}"
        );
        assert_eq!(new[0].as_str(), want, "http.route for {host}{path}");
    }

    // Vacuous-pass guard: the suite captured every case above.
    assert_eq!(captured.lock().unwrap().len(), cases.len());
}

// --- Real-server websocket tests (engine host answers 101 directly) ---

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Serve the full app (cja stack + host dispatch) on an ephemeral port.
async fn start(
    state: AppState,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let web_shutdown = shutdown.clone();
    let web = tokio::spawn(async move {
        axum::serve(listener, app(&state))
            .with_graceful_shutdown(web_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    (format!("ws://{address}"), shutdown, web)
}

/// Connect with an explicit Host header: a properly-formed client handshake
/// request (so the `Sec-WebSocket-*` headers exist) whose Host header is
/// overwritten — tokio-tungstenite 0.28 sends a hand-supplied request
/// verbatim, so our Host goes over the wire instead of the URI's.
async fn connect_host(base: &str, path: &str, host: &str) -> Client {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = format!("{base}{path}").into_client_request().unwrap();
    request.headers_mut().insert(
        axum::http::header::HOST,
        axum::http::HeaderValue::from_str(host).unwrap(),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}

async fn next(client: &mut Client) -> serde_json::Value {
    use futures::StreamExt as _;
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
    use futures::StreamExt as _;
    let message = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        message,
        tokio_tungstenite::tungstenite::Message::Close(
            Some(ref f)
        ) if f.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal
    ));
    let _ = client.close(None).await;
}

/// The engine-host websocket answers 101 directly (no redirect — a 3xx
/// fails browser handshakes), streams all 78 capture frames ascending,
/// sends `game_end` with the game object, and closes with 1000.
#[sqlx::test(migrations = "../migrations")]
async fn engine_host_websocket_answers_101_directly(pool: PgPool) {
    let (state, _arena_id) = crate::routes::game::api::fixtures::capture_fixture(&pool)
        .await
        .unwrap();
    let play_id = crate::routes::game::api::fixtures::CAPTURE_ADDEDB91;

    let (base, web_shutdown, web) = start(state).await;
    let mut client = connect_host(&base, &format!("/games/{play_id}/events"), ENGINE_HOST).await;
    let mut turns = Vec::new();
    let mut game_ends = 0;
    loop {
        let message = next(&mut client).await;
        match message["Type"].as_str() {
            Some("frame") => turns.push(message["Data"]["Turn"].clone()),
            Some("game_end") => {
                game_ends += 1;
                break;
            }
            other => panic!("unexpected message type: {other:?}"),
        }
    }
    assert_eq!(turns.len(), 78);
    for (i, turn) in turns.iter().enumerate() {
        assert_eq!(turn.as_i64(), Some(i as i64), "ascending, no dupes");
    }
    assert_eq!(game_ends, 1, "exactly one game_end");
    close(&mut client).await;

    // Negative: the same request against the arena host fails the
    // handshake — the site router has no /games/{id}/events route, so
    // there is no cross-routing.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("{base}/games/{play_id}/events")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        axum::http::header::HOST,
        axum::http::HeaderValue::from_str("arena.battlesnake.com").unwrap(),
    );
    let refused = tokio_tungstenite::connect_async(request).await;
    assert!(
        refused.is_err(),
        "arena host must not upgrade /games/<id>/events"
    );

    web_shutdown.cancel();
    web.await.unwrap();
}

/// The engine-host websocket for a NATIVE game: frames ascending, exactly
/// one game_end, close 1000.
#[sqlx::test(migrations = "../migrations")]
async fn engine_host_websocket_native_game_streams_to_game_end(pool: PgPool) {
    let game_id = seed_game_with_turn(&pool).await;
    sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 1, $2)")
        .bind(game_id)
        .bind(serde_json::json!({"Turn": 1, "Snakes": [], "Food": [], "Hazards": []}))
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState::test_from_pool(pool);

    let (base, web_shutdown, web) = start(state).await;
    let mut client = connect_host(&base, &format!("/games/{game_id}/events"), ENGINE_HOST).await;
    assert_eq!(next(&mut client).await["Data"]["Turn"], 0);
    assert_eq!(next(&mut client).await["Data"]["Turn"], 1);
    let message = next(&mut client).await;
    assert_eq!(message["Type"], "game_end");
    close(&mut client).await;

    web_shutdown.cancel();
    web.await.unwrap();
}
