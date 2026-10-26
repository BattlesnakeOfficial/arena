//! The engine-proxy wire-contract suite (DEV-1943).
//!
//! Drives arena's *real* proxied snake-call paths (`request_move_routed`,
//! `request_info_routed`, `execute_proxy`) against a proxy under test plus
//! mock snakes, asserting the arena-visible outcome of every wire-contract
//! case. The same suite runs against the Go image when
//! `ENGINE_PROXY_GO_TARGET=docker://<image>` is set, so the two
//! implementations are pinned to one contract; behaviour differences go in
//! the diff table in `docs/engine-proxy.md`.
//!
//! Notes carried from the plan:
//! - "Unreachable snake" is a freed port, never a dropped wiremock server
//!   (dropped wiremock listeners still answer 404).
//! - Negative assertions prove their trigger first and give the negative
//!   event time to propagate.
//! - All test clients are built `.no_proxy()` so ambient HTTP(S)_PROXY vars
//!   cannot reroute "proxied" calls.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine_proxy::config::ProxyConfig;
use reqwest::{Client, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::EngineProxyConfig;
use crate::models::battlesnake::EngineRegion;
use crate::snake_client::{ProxyClients, SnakeEndpoint};
use rules::Direction;

const TOKEN: &str = "contract-suite-token";

// ---------------------------------------------------------------------------
// proxy under test
// ---------------------------------------------------------------------------

/// One proxy per test process: in-process Rust by default, the Go image in a
/// `--network host` container when `ENGINE_PROXY_GO_TARGET=docker://<image>`.
/// `--network host` is required: a bridge-networked container cannot reach
/// this suite's host-loopback mock snakes (`127.0.0.1` inside the container
/// is the container).
enum ProxyUnderTest {
    Rust {
        base_url: String,
    },
    Docker {
        base_url: String,
        token: String,
        container: String,
    },
}

impl ProxyUnderTest {
    async fn start() -> Self {
        match std::env::var("ENGINE_PROXY_GO_TARGET") {
            Ok(target) => Self::start_docker(&target).await,
            Err(_) => Self::Rust {
                base_url: rust_proxy_base_url().to_string(),
            },
        }
    }

    async fn start_docker(target: &str) -> Self {
        let Some(image) = target.strip_prefix("docker://") else {
            panic!("ENGINE_PROXY_GO_TARGET must be docker://<image>, got {target:?}");
        };
        // The shared PROXY is a process-lifetime static and never dropped, so
        // `Drop` cannot be the cleanup story (coordinator note, PR #277):
        // reap stale labeled containers, run with --rm + a label, and arm a
        // watchdog that removes the container when the test process exits.
        const LABEL: &str = "dev1943.contract=go";
        let stale = std::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", &format!("label={LABEL}")])
            .output()
            .expect("docker ps");
        for id in String::from_utf8_lossy(&stale.stdout).lines() {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
        let port = free_port();
        // Random per-run token: never printed, never committed.
        let token = format!(
            "go-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let container = format!("engine-proxy-contract-{port}");
        // NB: never debug!("{:?}") this command — it carries the token in env.
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &container,
                "--label",
                LABEL,
                "--network",
                "host",
                "-e",
                &format!("PORT={port}"),
                "-e",
                &format!("BATTLESNAKE_PROXY_AUTH_TOKEN={token}"),
                "--entrypoint",
                "engine",
                image,
                "proxy_server",
            ])
            .output()
            .expect("docker run");
        assert!(
            out.status.success(),
            "docker run failed (stderr redacted, {} bytes)",
            out.stderr.len()
        );
        // Watchdog: when the test process exits (cleanly or killed), remove
        // the container. Belt (Drop) + suspenders (--rm) already exist; this
        // covers the process-lifetime-static case.
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "while kill -0 {pid} 2>/dev/null; do sleep 0.5; done; docker rm -f {container} >/dev/null 2>&1",
                pid = std::process::id()
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let base_url = format!("http://127.0.0.1:{port}");
        wait_alive(&base_url).await;
        Self::Docker {
            base_url,
            token,
            container,
        }
    }

    fn base_url(&self) -> &str {
        match self {
            Self::Rust { base_url } | Self::Docker { base_url, .. } => base_url,
        }
    }

    fn token(&self) -> &str {
        match self {
            Self::Rust { .. } => TOKEN,
            Self::Docker { token, .. } => token,
        }
    }
}

impl Drop for ProxyUnderTest {
    fn drop(&mut self) {
        if let Self::Docker { container, .. } = self {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", container])
                .output();
        }
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

static RUST_PROXY_BASE_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The shared in-process Rust proxy lives on a dedicated OS thread that owns
/// its own multi-thread runtime for the whole process.
///
/// It must NOT be spawned on a `#[tokio::test]` runtime: that runtime drops
/// when its test finishes, killing the proxy task with it, and every later
/// test then gets `ConnectionRefused` (attempt-1 blocker — 14 of 18 tests
/// failed when run together, all passed alone). The bound address crosses the
/// thread boundary over a std mpsc channel, behind a `OnceLock`.
fn rust_proxy_base_url() -> &'static str {
    RUST_PROXY_BASE_URL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("rust-proxy-under-test".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("proxy runtime");
                // The proxy task lives on this thread's runtime; block_on a
                // pending future so the runtime never shuts down.
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("bind proxy listener");
                    let port = listener.local_addr().unwrap().port();
                    let state = engine_proxy::app::ProxyState::new(
                        ProxyConfig {
                            auth_token: TOKEN.to_string(),
                            ..ProxyConfig::default()
                        },
                        CancellationToken::new(),
                    )
                    .expect("test config (no extra CA) always builds a connector");
                    let router = engine_proxy::app::router(state);
                    let cancel = CancellationToken::new();
                    tokio::spawn(cja::server::serve_until(
                        listener,
                        router,
                        cancel.cancelled_owned(),
                    ));
                    let _ = tx.send(format!("http://127.0.0.1:{port}"));
                    std::future::pending::<()>().await
                });
            })
            .expect("spawn rust-proxy-under-test thread");
        rx.recv().expect("rust proxy base url")
    })
}

async fn wait_alive(base_url: &str) {
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(res) = client.get(format!("{base_url}/healthz/alive")).send().await
            && res.status().is_success()
        {
            return;
        }
        assert!(Instant::now() < deadline, "proxy never became alive");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

static PROXY: tokio::sync::OnceCell<ProxyUnderTest> = tokio::sync::OnceCell::const_new();

async fn proxy() -> &'static ProxyUnderTest {
    PROXY.get_or_init(ProxyUnderTest::start).await
}

// ---------------------------------------------------------------------------
// arena-path helpers
// ---------------------------------------------------------------------------

fn test_client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// ProxyClients wired so `EngineRegion::UsEast4` calls hit the proxy under
/// test with the matching token.
async fn arena_clients(token_override: Option<&str>) -> (Client, EngineProxyConfig) {
    let proxy = proxy().await;
    let token = token_override.unwrap_or(proxy.token()).to_string();
    let config = EngineProxyConfig {
        token: Some(token),
        us_east4_url: proxy.base_url().to_string(),
        europe_west4_url: proxy.base_url().to_string(),
    };
    (test_client(), config)
}

async fn move_via_proxy(
    clients: &ProxyClients<'_>,
    endpoint: &SnakeEndpoint,
    timeout: Duration,
    last_direction: Option<Direction>,
) -> crate::snake_client::MoveResult {
    crate::snake_client::request_move_routed(
        clients,
        endpoint,
        &create_test_engine_game_with_snakes(vec!["snake-1"]),
        timeout,
        last_direction,
        &HashMap::new(),
        &HashMap::new(),
    )
    .await
}

/// A raw request to the proxy itself (arena's exact header set), for cases
/// arena's own code can never express (malformed values, header shapes).
fn raw_proxy_request(target: &str, timeout_ms: &str, auth: bool) -> reqwest::RequestBuilder {
    let proxy_url = {
        // blocking fetch of the shared proxy base url; the proxy is already
        // started by the time tests call this (they await proxy() first).
        PROXY.get().expect("proxy started").base_url().to_string()
    };
    let mut rb = test_client()
        .post(proxy_url)
        .header("X-Request-URI", target)
        .header("X-Battlesnake-Timeout-Ms", timeout_ms);
    if auth {
        let token = PROXY.get().expect("proxy started").token().to_string();
        rb = rb.header("X-Proxy-Authorization", format!("token {token}"));
    }
    rb
}

/// Copied from `snake_client.rs`'s private test module (it is a private `fn`
/// there): a minimal 11x11 standard game with the given snakes.
fn create_test_engine_game_with_snakes(snake_ids: Vec<&str>) -> crate::engine::EngineGame {
    use crate::engine::GameMeta;
    use crate::engine::GameSource;
    use rules::{BoardState, EliminationCause, Point, Snake, StandardSettings};

    let snakes: Vec<Snake> = snake_ids
        .iter()
        .map(|id| Snake {
            id: id.to_string(),
            body: vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)],
            health: 100,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        })
        .collect();

    let mut snake_names = HashMap::new();
    for id in &snake_ids {
        snake_names.insert(id.to_string(), format!("Snake {id}"));
    }

    crate::engine::EngineGame {
        board: BoardState {
            turn: 5,
            width: 11,
            height: 11,
            food: vec![Point::new(3, 3)],
            snakes,
            hazards: vec![],
        },
        meta: GameMeta {
            game_id: "contract-game".to_string(),
            ruleset_name: "standard".to_string(),
            timeout: 500,
            settings: StandardSettings::default(),
            royale: None,
            source: GameSource::Custom,
        },
        snake_names,
    }
}

fn endpoint_for(url: String) -> SnakeEndpoint {
    SnakeEndpoint {
        snake_id: "snake-1".to_string(),
        url,
        engine_region: EngineRegion::UsEast4,
    }
}

// ---------------------------------------------------------------------------
// contract cases
// ---------------------------------------------------------------------------

/// Case 1: the proxied request reaches the mock snake at the target's exact
/// path and query, and arena uses the move.
#[tokio::test]
async fn proxied_request_reaches_mock_snake() {
    let proxy = proxy().await;
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .and(query_param("token", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"left"}"#))
        .expect(1)
        .mount(&snake)
        .await;

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(format!("{}?token=abc", snake.uri()));
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;

    assert_eq!(result.direction, Direction::Left);
    assert!(!result.timed_out);
    assert!(!result.errored);
    assert!(
        result.latency_ms.is_some(),
        "proxy-measured latency: {result:?}"
    );
    assert_eq!(result.status_code, Some(200));
    snake.verify().await;
    let _ = proxy; // keep the &'static alive across the await points
}

/// Case 2: `X-Battlesnake-Timeout-Ms` is parsed (arena always sends it).
/// The default-when-absent leg (1000ms) lives in the engine-proxy suite —
/// arena's own client can never omit the header.
#[tokio::test]
async fn timeout_header_parsed_and_defaulted() {
    // 300ms budget against a 2s snake: 504 -> timed_out, fallback direction.
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"move":"up"}"#)
                .set_delay(Duration::from_secs(2)),
        )
        .expect(1)
        .mount(&snake)
        .await;

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let result = move_via_proxy(
        &clients,
        &endpoint,
        Duration::from_millis(300),
        Some(Direction::Down),
    )
    .await;
    assert_eq!(result.direction, Direction::Down, "fallback move");
    assert!(result.timed_out, "504 maps to timed_out: {result:?}");
    assert!(!result.errored);
    assert_eq!(result.latency_ms, None);
    assert_eq!(result.status_code, None);
    // DEV-1946 hardening: a proxy 504 carries X-Battlesnake-Server-Error, so
    // arena classifies it as a *snake-side* timeout — exactly one hit on the
    // snake, no direct retry (a retry would double the hits and burn the
    // snake's budget a second time).
    snake.verify().await;
}

/// Case 3/4: bad auth -> 407 without the latency header -> arena retries
/// direct and the game proceeds (the snake really receives the direct call).
#[tokio::test]
async fn bad_or_missing_auth_returns_407() {
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"right"}"#))
        .expect(1) // exactly one direct call, no successful proxied call
        .mount(&snake)
        .await;

    // arena configured with a token the proxy does not accept
    let (client, config) = arena_clients(Some("wrong-token")).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(500), None).await;

    assert_eq!(result.direction, Direction::Right, "move via direct retry");
    assert!(!result.timed_out);
    assert!(!result.errored);
    assert!(result.latency_ms.is_some(), "direct retry is measured");
    snake.verify().await;
}

/// Case 5: malformed requests 400. Arena can never send these, so this is a
/// raw probe.
#[tokio::test]
async fn malformed_requests_return_400() {
    proxy().await;
    for (uri, timeout) in [
        ("ftp://snake.example/move", "500"),
        ("http://snake.invalid/move", "abc"),
    ] {
        let res = raw_proxy_request(uri, timeout, true).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "uri={uri}");
    }
    // missing X-Request-URI entirely
    let base = PROXY.get().unwrap().base_url().to_string();
    let token = PROXY.get().unwrap().token().to_string();
    let res = test_client()
        .post(base)
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", format!("token {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

/// Case 6: a snake's own 407/500/504 passes through as a measured
/// SnakeResponse — never classified as a proxy fault.
#[tokio::test]
async fn snake_statuses_pass_through() {
    for status in [407u16, 500, 504] {
        let snake = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(status).set_body_string(r#"{"move":"down"}"#))
            .expect(1)
            .mount(&snake)
            .await;

        let (client, config) = arena_clients(None).await;
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = endpoint_for(snake.uri());
        let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;
        assert_eq!(result.direction, Direction::Down, "move still used");
        assert!(!result.timed_out, "snake status is not a proxy timeout");
        assert!(result.errored, "non-2xx is the snake erroring: {result:?}");
        assert!(result.latency_ms.is_some());
        assert_eq!(result.status_code, Some(status));
    }
}

/// Case 7: the latency header is present and sane on success.
#[tokio::test]
async fn latency_header_present_and_sane() {
    proxy().await;
    // target a mock snake so the call succeeds
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .mount(&snake)
        .await;
    let res = raw_proxy_request(&format!("{}/move", snake.uri()), "1000", true)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let latency = res
        .headers()
        .get("x-battlesnake-latency-ms")
        .expect("latency header on success")
        .to_str()
        .unwrap()
        .parse::<i64>()
        .expect("whole milliseconds");
    assert!((0..=2000).contains(&latency));
}

/// Case 8: the timing header matches Go's format, and pooled reuse reports
/// zero dns/connect/tls.
#[tokio::test]
async fn timing_header_format_matches_go() {
    proxy().await;
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .mount(&snake)
        .await;
    let target = format!("{}/move", snake.uri());

    let parse = |res: &reqwest::Response| {
        engine_proxy::timing::parse_go_timing(
            res.headers()
                .get("x-battlesnake-timing-ms")
                .expect("timing header")
                .to_str()
                .unwrap(),
        )
        .expect("go format parses")
    };
    let first = raw_proxy_request(&target, "1000", true)
        .send()
        .await
        .unwrap();
    let _ = parse(&first);
    let second = raw_proxy_request(&target, "1000", true)
        .send()
        .await
        .unwrap();
    let t2 = parse(&second);
    assert_eq!(t2.dns_ms, 0.0, "reuse reports zero dns");
    assert_eq!(t2.connect_ms, 0.0, "reuse reports zero connect");
    assert_eq!(t2.tls_ms, 0.0, "reuse reports zero tls");
}

/// Case 9: a snake that stalls before headers -> 504 -> `timed_out: true`,
/// and the whole call completes before arena's local guard fires.
#[tokio::test]
async fn snake_stalls_before_headers_times_out() {
    let mut stall = StallSnake::accept_and_go_quiet().await;
    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(format!("http://{}/move", stall.addr));
    let started = Instant::now();
    let result = move_via_proxy(
        &clients,
        &endpoint,
        Duration::from_millis(300),
        Some(Direction::Left),
    )
    .await;
    // Prove the trigger: the snake accepted the TCP connection before going
    // quiet — this really was a pre-headers stall, not a connect failure.
    // Bounded: a regression must fail here, not hang the suite.
    tokio::time::timeout(Duration::from_secs(10), stall.accepted.recv())
        .await
        .expect("stall snake accept signal within 10s")
        .expect("stall snake observed the TCP accept");

    assert_eq!(result.direction, Direction::Left, "fallback move");
    assert!(
        result.timed_out,
        "pre-headers stall is a timeout: {result:?}"
    );
    assert!(!result.errored);
    assert_eq!(result.latency_ms, None);
    assert!(
        started.elapsed() < Duration::from_millis(300 + 300),
        "completes before arena's local guard: {:?}",
        started.elapsed()
    );
    // DEV-1946 hardening: the 504 + error header is snake-side, so arena
    // makes NO direct retry — exactly one connection to the snake.
    let accepts = stall.accepted_count.load(Ordering::SeqCst);
    assert_eq!(accepts, 1, "no direct retry after a snake-side 504");
}

/// Case 10: DNS failure and connect-refused both map to 502 + the matching
/// `X-Battlesnake-Server-Error` code.
#[tokio::test]
async fn unreachable_snake_returns_502_dns() {
    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for("http://snake.invalid".to_string());
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;
    assert!(result.timed_out, "transport failure: {result:?}");
    assert_eq!(result.latency_ms, None);

    // raw probe asserts the header itself
    let res = raw_proxy_request("http://snake.invalid/move", "1000", true)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "2:Failed to resolve Battlesnake hostname"
    );
}

#[tokio::test]
async fn refused_port_returns_502_connect() {
    let port = free_port(); // freed: connect is refused, not 404
    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(format!("http://127.0.0.1:{port}"));
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;
    assert!(result.timed_out, "transport failure: {result:?}");

    let res = raw_proxy_request(&format!("http://127.0.0.1:{port}/move"), "1000", true)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "3:Failed to connect to Battlesnake host"
    );
}

/// Case 10, TLS leg: an `https://` target that speaks plain HTTP fails the
/// handshake ⇒ 502 + `4:Failed to set up a TLS connection`. This leg is also
/// where the DEV-1946 no-retry pin for 502 + X-Battlesnake-Server-Error
/// lives: the DNS/refused legs have no snake to count hits on, but a
/// counting listener here observes exactly one proxied connection and zero
/// direct retries (a retry would show up as a second accept).
#[tokio::test]
async fn tls_handshake_fail_returns_502_tls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted_count = Arc::new(AtomicU64::new(0));
    let count = Arc::clone(&accepted_count);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            // Read the TLS ClientHello, then answer plain HTTP — guaranteed
            // garbage from rustls's point of view, so the handshake fails.
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await;
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(format!("https://{addr}"));
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;
    assert!(result.timed_out, "transport failure: {result:?}");
    assert!(!result.errored);
    assert_eq!(result.latency_ms, None);

    // DEV-1946 hardening: the 502 + error header is snake-side, so arena
    // makes NO direct retry — exactly one connection, the proxied one.
    let accepts = accepted_count.load(Ordering::SeqCst);
    assert_eq!(accepts, 1, "no direct retry after a snake-side 502");

    // raw probe asserts the header itself
    let res = raw_proxy_request(&format!("https://{addr}/move"), "1000", true)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "4:Failed to set up a TLS connection"
    );
}

/// Case 11: health endpoints answer the documented bodies.
#[tokio::test]
async fn health_endpoints_respond() {
    proxy().await;
    let alive = test_client()
        .get(format!("{}/healthz/alive", PROXY.get().unwrap().base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(alive.status(), 200);
    assert_eq!(alive.text().await.unwrap().trim(), "alive");
    let ready = test_client()
        .get(format!("{}/healthz/ready", PROXY.get().unwrap().base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    assert_eq!(ready.text().await.unwrap().trim(), "ready");
}

/// Case 12: `/metrics` is token-gated.
#[tokio::test]
async fn metrics_requires_token() {
    proxy().await;
    let base = PROXY.get().unwrap().base_url().to_string();
    let no_auth = test_client()
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(no_auth.headers().get("www-authenticate").unwrap(), "token");

    let token = PROXY.get().unwrap().token().to_string();
    let ok = test_client()
        .get(format!("{base}/metrics"))
        .header("Authorization", format!("token {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let body = ok.text().await.unwrap();
    assert!(!body.is_empty());
}

/// Case 13: arena's User-Agent is forwarded verbatim to the snake.
#[tokio::test]
async fn user_agent_forwarded_verbatim() {
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .and(header("user-agent", "ArenaContract/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .expect(1)
        .mount(&snake)
        .await;

    let (_, config) = arena_clients(None).await;
    let east = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .user_agent("ArenaContract/1")
        .build()
        .unwrap();
    let direct = test_client();
    let clients = ProxyClients {
        direct: &direct,
        east: &east,
        europe: &direct,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(1000), None).await;
    assert_eq!(result.direction, Direction::Up);
    snake.verify().await;
}

/// Case 13b: when the caller sends no User-Agent, none is synthesized on the
/// way to the snake (arena always sends one; this is the raw leg, and it runs
/// against the Go target too in the differential).
#[tokio::test]
async fn absent_user_agent_omitted() {
    proxy().await;
    let snake = MockServer::start().await;
    // Leak detector (mounted FIRST so it wins on match): if a UA were
    // synthesized, this fires and the primary never matches.
    Mock::given(method("POST"))
        .and(path("/move"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(418).set_body_string("UA-LEAKED"))
        .expect(0)
        .mount(&snake)
        .await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .expect(1)
        .mount(&snake)
        .await;

    // raw_proxy_request sends no User-Agent (reqwest default).
    let res = raw_proxy_request(&format!("{}/move", snake.uri()), "1000", true)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "no synthesized UA reached the snake");
    snake.verify().await;
}

/// Case 14: control headers (proxy auth, request URI, timeout, forwarded-for,
/// hop-by-hop) never reach the snake. Raw leg so the decoy headers can be
/// attached; runs against the Go target too in the differential.
#[tokio::test]
async fn control_headers_not_forwarded() {
    proxy().await;
    let snake = MockServer::start().await;
    const BANNED: [&str; 5] = [
        "x-proxy-authorization",
        "proxy-authorization",
        "x-request-uri",
        "x-battlesnake-timeout-ms",
        "x-forwarded-for",
    ];
    for banned in BANNED {
        // Leak detector (mounted FIRST): if the control header survives to
        // the snake, this fires instead of the primary mock.
        Mock::given(method("POST"))
            .and(path("/move"))
            .and(header_exists(banned))
            .respond_with(ResponseTemplate::new(418).set_body_string(format!("LEAKED:{banned}")))
            .expect(0)
            .mount(&snake)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/move"))
        .and(query_param("token", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .expect(1)
        .mount(&snake)
        .await;

    let res = raw_proxy_request(&format!("{}/move?token=abc", snake.uri()), "500", true)
        .header("Proxy-Authorization", "token decoy")
        .header("X-Forwarded-For", "1.2.3.4")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "control headers leaked to the snake");
    snake.verify().await;
}

/// Case 15: an inbound request body over the 1 MiB cap is rejected 413.
/// Deliberate diff vs Go (no request cap): against the Go target the body is
/// forwarded and the snake's 200 passes through — asserted as-is.
#[tokio::test]
async fn oversized_request_body_rejected() {
    proxy().await;
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
        .mount(&snake)
        .await;

    let res = raw_proxy_request(&format!("{}/move", snake.uri()), "2000", true)
        .body(vec![b'x'; 2 * 1024 * 1024]) // > the proxy's 1 MiB default cap
        .send()
        .await
        .unwrap();
    if matches!(proxy().await, ProxyUnderTest::Docker { .. }) {
        assert_eq!(
            res.status(),
            200,
            "Go has no request body cap: the oversized body is forwarded"
        );
    } else {
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}

/// Case 16a (arena-visible leg): a declared oversized body maps to 502+5: ->
/// SnakeTransportFailure -> timed_out (fallback), not a truncated 200.
///
/// Deliberate diff vs Go (recorded in docs/engine-proxy.md): Go has no
/// Content-Length pre-check, so it streams the body until the reader gives
/// up — arena sees a truncated 200 -> parse failure -> errored fallback.
/// Against the Go target that outcome is asserted as-is.
#[tokio::test]
async fn oversized_response_body_declared_502() {
    let snake = MockServer::start().await;
    let big: String = "x".repeat(2 * 1024 * 1024); // > the proxy's 1 MiB cap
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(big))
        .mount(&snake)
        .await;

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let result = move_via_proxy(&clients, &endpoint, Duration::from_millis(2000), None).await;
    let go = matches!(proxy().await, ProxyUnderTest::Docker { .. });
    if go {
        assert!(
            !result.timed_out,
            "Go streams the declared body; truncated 200 is not a timeout: {result:?}"
        );
        assert!(
            result.errored,
            "truncated body is a parse failure: {result:?}"
        );
        return;
    }
    assert!(
        result.timed_out,
        "502+5: is a transport failure: {result:?}"
    );
    assert!(!result.errored);
    assert_eq!(result.latency_ms, None);
}

/// Case 16b (arena-visible leg): a body arena truncates at its own read cap
/// yields a parse failure -> errored fallback move with measured latency.
#[tokio::test]
async fn oversized_response_body_falls_back() {
    let snake = MockServer::start().await;
    let big: String = "x".repeat(100 * 1024); // > arena's 64 KiB read cap
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(200).set_body_string(big))
        .mount(&snake)
        .await;

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let result = move_via_proxy(
        &clients,
        &endpoint,
        Duration::from_millis(2000),
        Some(Direction::Left),
    )
    .await;
    assert_eq!(result.direction, Direction::Left, "fallback move");
    assert!(!result.timed_out);
    assert!(result.errored, "unparseable body is the snake erroring");
    assert!(result.latency_ms.is_some());
}

/// Case 17b (post-DEV-1946 semantics): headers arrive, the body stalls —
/// the proxy aborts the connection inside its budget, arena classifies the
/// abort as a proxy *transport fault* and (DEV-1946, #276) retries the move
/// directly with the snake's full timeout. The stall snake stalls only its
/// FIRST connection and answers later ones, so the retry lands on a working
/// snake: the turn must end with the snake's real direct move — never the
/// fallback, never `timed_out`, never `errored` (a proxy fault is never the
/// snake's error).
#[tokio::test]
async fn stalled_body_aborts_as_transport_fault() {
    let proxy_budget_ms = 300u64;
    let direct_timeout_ms = 300u64; // DEV-1946 retry uses the same fresh timeout
    let mut stall = StallSnake::stall_first_then_answer_left().await;
    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(format!("http://{}/move", stall.addr));
    let started = Instant::now();
    let result = move_via_proxy(
        &clients,
        &endpoint,
        Duration::from_millis(proxy_budget_ms),
        Some(Direction::Up),
    )
    .await;
    // Prove the trigger: headers were sent on the stalled connection before
    // the stall. Bounded so a regression fails instead of hanging.
    let headers_sent = tokio::time::timeout(Duration::from_secs(10), stall.headers_sent.recv())
        .await
        .expect("headers-sent signal within 10s")
        .unwrap_or(false);
    assert!(headers_sent, "stall snake wrote its response headers");

    // Exactly two connections: the proxied call (stalled, aborted by the
    // proxy inside its budget) and DEV-1946's one direct retry. A third
    // would mean an extra retry; a single one would mean the proxy never
    // aborted the body (or the fault was misclassified as snake-side).
    let accepts = stall.accepted_count.load(Ordering::SeqCst);
    assert_eq!(
        accepts, 2,
        "proxied call + one direct retry, no more ({accepts} accepts)"
    );

    assert_eq!(
        result.direction,
        Direction::Left,
        "the direct retry's real move is used, not the fallback: {result:?}"
    );
    assert!(
        !result.timed_out,
        "no 504 can follow forwarded headers: {result:?}"
    );
    assert!(!result.errored, "a proxy fault is never the snake's error");
    assert!(
        result.latency_ms.is_some(),
        "the direct retry's move is measured: {result:?}"
    );
    assert_eq!(result.status_code, Some(200), "the direct answer's status");
    assert!(
        started.elapsed() < Duration::from_millis(proxy_budget_ms + direct_timeout_ms + 400),
        "proxy abort + direct retry fit inside both budgets + slack: {:?}",
        started.elapsed()
    );

    // Raw leg (proxy contract itself): against a snake that stalls the body
    // on EVERY connection, the proxy must abort inside the budget — the raw
    // client sees the 200 headers, then an error mid-body (the connection
    // killed), never a 200 body that completes late.
    let mut raw_stall = StallSnake::headers_then_partial_body_then_stall().await;
    let res = tokio::time::timeout(
        Duration::from_secs(15),
        raw_proxy_request(
            &format!("http://{}/move", raw_stall.addr),
            &proxy_budget_ms.to_string(),
            true,
        )
        .send(),
    )
    .await
    .expect("raw request resolves within 15s")
    .expect("raw request sends");
    assert_eq!(
        res.status(),
        200,
        "the stalled snake's headers pass through"
    );
    let body = tokio::time::timeout(Duration::from_secs(15), res.text()).await;
    let body = body.expect("body read resolves within 15s (bounded)");
    assert!(
        body.is_err(),
        "the proxy must abort the stalled body inside the budget, not complete it"
    );
    let raw_headers_sent =
        tokio::time::timeout(Duration::from_secs(10), raw_stall.headers_sent.recv())
            .await
            .expect("raw-stall headers-sent signal within 10s")
            .unwrap_or(false);
    assert!(
        raw_headers_sent,
        "raw-stall snake wrote its response headers"
    );
}

/// Request-info path through the proxy (arena's other proxied call shape).
#[tokio::test]
async fn request_info_routed_through_proxy() {
    let snake = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r##"{"color":"#ff00ff"}"##))
        .expect(1)
        .mount(&snake)
        .await;

    let (client, config) = arena_clients(None).await;
    let clients = ProxyClients {
        direct: &client,
        east: &client,
        europe: &client,
        config: &config,
    };
    let endpoint = endpoint_for(snake.uri());
    let info =
        crate::snake_client::request_info_routed(&clients, &endpoint, Duration::from_millis(1000))
            .await;
    let info = info.expect("info parsed through the proxy");
    assert_eq!(info.color.as_deref(), Some("#ff00ff"));
    snake.verify().await;
}

/// Differential table: raw probes against whichever proxy the env selected,
/// printed for pasting into `docs/engine-proxy.md`. Without
/// `ENGINE_PROXY_GO_TARGET` this prints a skip note and passes (the Rust
/// suite above already pins every case).
#[tokio::test]
async fn contract_diff_table() {
    let Ok(target) = std::env::var("ENGINE_PROXY_GO_TARGET") else {
        eprintln!("SKIP contract_diff_table: ENGINE_PROXY_GO_TARGET not set");
        return;
    };
    let proxy = proxy().await;
    let kind = match proxy {
        ProxyUnderTest::Rust { .. } => "rust",
        ProxyUnderTest::Docker { .. } => "go",
    };
    eprintln!("| case | {kind} observation |");
    eprintln!("|---|---|");

    // freed-port connect probe
    let port = free_port();
    let res = raw_proxy_request(&format!("http://127.0.0.1:{port}/move"), "1000", true)
        .send()
        .await
        .unwrap();
    eprintln!(
        "| freed-port connect | {} {} |",
        res.status(),
        res.headers()
            .get("x-battlesnake-server-error")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into())
    );

    // missing auth
    let res = raw_proxy_request("http://snake.invalid/move", "1000", false)
        .send()
        .await
        .unwrap();
    eprintln!(
        "| missing auth | {} latency={} |",
        res.status(),
        res.headers()
            .get("x-battlesnake-latency-ms")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into())
    );

    // bad timeout header
    let res = raw_proxy_request("http://snake.invalid/move", "abc", true)
        .send()
        .await
        .unwrap();
    eprintln!("| bad timeout header | {} |", res.status());

    // metrics unauthenticated
    let res = test_client()
        .get(format!("{}/metrics", proxy.base_url()))
        .send()
        .await
        .unwrap();
    eprintln!(
        "| /metrics no auth | {} www-auth={} |",
        res.status(),
        res.headers()
            .get("www-authenticate")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into())
    );

    // version endpoint
    let res = test_client()
        .get(format!("{}/version", proxy.base_url()))
        .send()
        .await
        .unwrap();
    eprintln!(
        "| /version | {} {} |",
        res.status(),
        res.text().await.unwrap()
    );

    // snake-status passthrough: does the proxy forward a snake 500?
    let snake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&snake)
        .await;
    let res = raw_proxy_request(&format!("{}/move", snake.uri()), "1000", true)
        .send()
        .await
        .unwrap();
    eprintln!(
        "| snake 500 passthrough | {} latency={} server-error={} |",
        res.status(),
        res.headers()
            .get("x-battlesnake-latency-ms")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into()),
        res.headers()
            .get("x-battlesnake-server-error")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into())
    );

    // timing header on success
    let res = raw_proxy_request(&format!("{}/move", snake.uri()), "1000", true)
        .send()
        .await
        .unwrap();
    eprintln!(
        "| timing header | {} |",
        res.headers()
            .get("x-battlesnake-timing-ms")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    let _ = target;
}

// ---------------------------------------------------------------------------
// raw stall snakes
// ---------------------------------------------------------------------------

struct StallSnake {
    addr: SocketAddr,
    accepted: tokio::sync::mpsc::Receiver<()>,
    /// Every TCP connection this snake ever accepted (DEV-1946 hardening:
    /// pins that arena makes no direct retry when the failure is the
    /// snake's own — a retry would show up as a second accept).
    accepted_count: Arc<AtomicU64>,
    headers_sent: tokio::sync::mpsc::Receiver<bool>,
}

impl StallSnake {
    async fn accept_and_go_quiet() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted) = tokio::sync::mpsc::channel(1);
        let (_, headers_sent) = tokio::sync::mpsc::channel(1);
        let accepted_count = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&accepted_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                accepted_tx.send(()).await.unwrap_or(());
                let mut buf = [0u8; 2048];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        Self {
            addr,
            accepted,
            accepted_count,
            headers_sent,
        }
    }

    /// Stalls only the FIRST connection (200 headers, partial body, then
    /// silence — the case-17b stall) and answers 200 `{"move":"left"}` on
    /// every later connection, so a DEV-1946 direct retry after the proxy's
    /// body abort lands on a working snake.
    async fn stall_first_then_answer_left() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, accepted) = tokio::sync::mpsc::channel(1);
        let (sent_tx, headers_sent) = tokio::sync::mpsc::channel(1);
        let accepted_count = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&accepted_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let idx = count.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                if idx == 0 {
                    // connection 1: the stall (headers + partial body, then
                    // hold the connection open until the proxy aborts it)
                    let _ = stream.read(&mut buf).await;
                    let wrote = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Game: stall\r\n\r\n{\"move\":\"",
                        )
                        .await
                        .is_ok();
                    sent_tx.send(wrote).await.unwrap_or(());
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                } else {
                    // later connections: a working snake. Read the request,
                    // answer a complete move, then drain so the close is a
                    // clean FIN (dropping with unread request bytes sends a
                    // RST that can fail the client's response read).
                    let _ = stream.read(&mut buf).await;
                    let body = b"{\"move\":\"left\"}";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(resp.as_bytes()).await.is_ok()
                        && stream.write_all(body).await.is_ok()
                    {
                        let _ = stream.flush().await;
                    }
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await;
                }
            }
        });
        Self {
            addr,
            accepted,
            accepted_count,
            headers_sent,
        }
    }

    async fn headers_then_partial_body_then_stall() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, accepted) = tokio::sync::mpsc::channel(1);
        let (sent_tx, headers_sent) = tokio::sync::mpsc::channel(1);
        let accepted_count = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&accepted_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                count.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let wrote = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Game: stall\r\n\r\n{\"move\":\"",
                    )
                    .await
                    .is_ok();
                sent_tx.send(wrote).await.unwrap_or(());
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        Self {
            addr,
            accepted,
            accepted_count,
            headers_sent,
        }
    }
}
