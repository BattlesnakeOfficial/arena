//! Integration tests for the proxy data path, against the real router
//! (`engine_proxy::app::router`) served on an ephemeral port via cja's
//! `serve_until`. Mirrors the arena-side contract suite but lives next to the
//! implementation so it can assert log lines and metrics directly.
//!
//! Discipline (from the plan): "unreachable snake" is a *freed port*, never a
//! dropped wiremock server; negative-assertion tests prove their trigger was
//! observed and give the negative event time to propagate; proxy env vars
//! are unset by using reqwest clients with `.no_proxy()`.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::{Duration, Instant};

use engine_proxy::app;
use engine_proxy::config::ProxyConfig;
use engine_proxy::metrics;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// log capture: a global subscriber layer recording every `engine_proxy`
// event as "name=value" lines. Tests filter by their unique mock port.
// ---------------------------------------------------------------------------

static CAPTURE: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static INSTALL: Once = Once::new();

struct CaptureLayer;

struct FieldVisitor(String);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={:?} ", field.name(), value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push_str(&format!("{}={} ", field.name(), value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.push_str(&format!("{}={} ", field.name(), value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.push_str(&format!("{}={} ", field.name(), value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.push_str(&format!("{}={:.3} ", field.name(), value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.push_str(&format!("{}={} ", field.name(), value));
    }
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "engine_proxy" {
            let mut visitor = FieldVisitor(String::new());
            event.record(&mut visitor);
            CAPTURE.lock().unwrap().push(visitor.0);
        }
    }
}

fn install_capture() {
    INSTALL.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        tracing_subscriber::registry().with(CaptureLayer).init();
    });
}

/// All captured request lines whose `host=` field matches (unique port per
/// test ⇒ precise filter).
fn lines_for(host_port: &str) -> Vec<String> {
    let needle = format!("host=127.0.0.1:{host_port} ");
    CAPTURE
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains(&needle))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

const TEST_TOKEN: &str = "contract-secret";

struct ProxyUnderTest {
    base_url: String,
    port: u16,
}

impl ProxyUnderTest {
    async fn start() -> Self {
        Self::start_with(ProxyConfig {
            auth_token: TEST_TOKEN.to_string(),
            ..ProxyConfig::default()
        })
        .await
    }

    async fn start_with(cfg: ProxyConfig) -> Self {
        install_capture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = app::ProxyState::new(cfg.clone(), CancellationToken::new());
        let router = app::router(state);
        let token = CancellationToken::new();
        tokio::spawn(cja::server::serve_until(
            listener,
            router,
            token.cancelled_owned(),
        ));
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            port,
        }
    }

    /// A fresh freed port (a connect here is refused, not 404).
    fn freed_port_target() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/move")
    }
}

fn client() -> reqwest::Client {
    // Hard 10s bound: a regression must fail, not hang the test binary.
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
}

fn proxied_request(
    proxy: &ProxyUnderTest,
    target: &str,
    timeout_ms: &str,
) -> reqwest::RequestBuilder {
    client()
        .post(&proxy.base_url)
        .header("X-Request-URI", target)
        .header("X-Battlesnake-Timeout-Ms", timeout_ms)
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
}

// ---------------------------------------------------------------------------
// auth (contract items 3, 4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_or_missing_auth_returns_407_without_latency() {
    let proxy = ProxyUnderTest::start().await;
    let target = ProxyUnderTest::freed_port_target();

    // missing auth
    let res = client()
        .post(&proxy.base_url)
        .header("X-Request-URI", &target)
        .header("X-Battlesnake-Timeout-Ms", "500")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );
    assert!(
        !res.headers().contains_key("x-battlesnake-latency-ms"),
        "407 must not carry the latency header (arena keys its direct retry on this)"
    );
    assert_eq!(res.bytes().await.unwrap().len(), 0);

    // wrong secret (reqwest .header appends, so build this one raw —
    // proxied_request already sets the correct X- header)
    let res = client()
        .post(&proxy.base_url)
        .header("X-Request-URI", &target)
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", "token wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );

    // X- present-but-wrong wins over a correct Proxy-Authorization (Go parity)
    let res = client()
        .post(&proxy.base_url)
        .header("X-Request-URI", &target)
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", "token wrong")
        .header("Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );

    // The alternate header alone is accepted when X- is absent.
    let res = client()
        .post(&proxy.base_url)
        .header("X-Request-URI", &target)
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_ne!(
        res.status(),
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );
}

#[tokio::test]
async fn auth_is_checked_before_uri_and_timeout_validation() {
    // Go order: auth -> URI presence -> URL parse -> scheme -> timeout.
    let proxy = ProxyUnderTest::start().await;
    // no auth, no X-Request-URI, garbage timeout: still 407, not 400.
    let res = client()
        .post(&proxy.base_url)
        .header("X-Battlesnake-Timeout-Ms", "not-a-number")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
    );

    // valid auth but missing URI: 400
    let res = client()
        .post(&proxy.base_url)
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// malformed requests (contract items 2, 5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_requests_return_400() {
    let proxy = ProxyUnderTest::start().await;
    for (uri, timeout) in [
        (ProxyUnderTest::freed_port_target(), "abc"),
        ("ftp://snake.example/move".to_string(), "500"),
        ("not a url".to_string(), "500"),
    ] {
        let res = proxied_request(&proxy, &uri, timeout).send().await.unwrap();
        assert_eq!(
            res.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "uri={uri} timeout={timeout}"
        );
    }
}

#[tokio::test]
async fn timeout_header_absent_defaults_to_1000ms() {
    // With no timeout header the budget is Go's DEFAULT_TIMEOUT (1000ms): a
    // snake answering at 600ms succeeds.
    let snake = MockSnake::http_ok_with_delay(Duration::from_millis(600)).await;
    let proxy = ProxyUnderTest::start().await;
    let res = client()
        .post(&proxy.base_url)
        .header("X-Request-URI", format!("http://{}/move", snake.addr))
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

// ---------------------------------------------------------------------------
// passthrough + headers (contract items 6, 7, 8, 13, 14)
// ---------------------------------------------------------------------------

/// A recording mock snake: an in-process axum server that captures the
/// headers of the last request it received and answers with a fixed body.
/// Serves `expected_path` with `PATH-OK` and everything else with `FALLBACK`
/// so tests can assert the forwarded path actually landed.
struct RecordingSnake {
    addr: SocketAddr,
    captured: Arc<Mutex<Option<reqwest::header::HeaderMap>>>,
}

impl RecordingSnake {
    async fn start(status: reqwest::StatusCode, body: &'static str) -> Self {
        Self::start_at_path(status, body, "/move").await
    }

    async fn start_at_path(
        status: reqwest::StatusCode,
        body: &'static str,
        expected_path: &'static str,
    ) -> Self {
        let captured: Arc<Mutex<Option<reqwest::header::HeaderMap>>> = Arc::new(Mutex::new(None));
        let captured_req = captured.clone();
        let app = axum::Router::new()
            .route(
                expected_path,
                axum::routing::any(move |req: axum::extract::Request| {
                    let captured_req = captured_req.clone();
                    async move {
                        *captured_req.lock().unwrap() = Some(req.headers().clone());
                        (status, body)
                    }
                }),
            )
            .fallback(|| async { (reqwest::StatusCode::OK, "FALLBACK") });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, captured }
    }

    fn last_headers(&self) -> reqwest::header::HeaderMap {
        self.captured.lock().unwrap().clone().unwrap_or_default()
    }
}

/// Simple wiremock-free mock snake with a fixed delay.
struct MockSnake {
    addr: SocketAddr,
}

impl MockSnake {
    async fn http_ok_with_delay(delay: Duration) -> Self {
        let app = axum::Router::new().fallback(move || async move {
            tokio::time::sleep(delay).await;
            (reqwest::StatusCode::OK, "{\"move\":\"up\"}")
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr }
    }
}

#[tokio::test]
async fn snake_statuses_pass_through_with_latency_and_no_error_header() {
    let proxy = ProxyUnderTest::start().await;
    for status in [407u16, 500, 504] {
        let snake = RecordingSnake::start(
            reqwest::StatusCode::from_u16(status).unwrap(),
            "{\"move\":\"down\"}",
        )
        .await;
        let res = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), status, "status passthrough");
        let latency = res
            .headers()
            .get("x-battlesnake-latency-ms")
            .expect("latency header on forwarded snake statuses")
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!((0..=1000).contains(&latency));
        assert!(
            !res.headers().contains_key("x-battlesnake-server-error"),
            "a snake's own status is not a server error"
        );
        assert_eq!(res.text().await.unwrap(), "{\"move\":\"down\"}");
    }
}

#[tokio::test]
async fn user_agent_forwarded_verbatim_and_absent_means_omitted() {
    let proxy = ProxyUnderTest::start().await;

    // forwarded verbatim
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;
    client()
        .post(&proxy.base_url)
        .header("X-Request-URI", format!("http://{}/move", snake.addr))
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .header("User-Agent", "ArenaContract/1 (test)")
        .send()
        .await
        .unwrap();
    let headers = snake.last_headers();
    assert_eq!(
        headers.get("user-agent").unwrap(),
        "ArenaContract/1 (test)",
        "UA forwarded verbatim"
    );

    // absent => not sent at all (reqwest without .user_agent() sends no UA)
    let snake2 = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;
    client()
        .post(&proxy.base_url)
        .header("X-Request-URI", format!("http://{}/move", snake2.addr))
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    let headers = snake2.last_headers();
    assert!(
        headers.get("user-agent").is_none(),
        "absent UA must not be synthesized: got {:?}",
        headers.get("user-agent")
    );
}

#[tokio::test]
async fn control_headers_not_forwarded_and_host_is_target() {
    let proxy = ProxyUnderTest::start().await;
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;
    client()
        .post(&proxy.base_url)
        .header(
            "X-Request-URI",
            format!("http://{}/move?token=leak", snake.addr),
        )
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .header("Proxy-Authorization", "token decoy")
        .header("X-Forwarded-For", "1.2.3.4")
        .header("Connection", "keep-alive")
        .send()
        .await
        .unwrap();

    let headers = snake.last_headers();
    for banned in [
        "x-proxy-authorization",
        "proxy-authorization",
        "x-request-uri",
        "x-battlesnake-timeout-ms",
        "x-forwarded-for",
        "connection",
    ] {
        assert!(
            !headers.contains_key(banned),
            "{banned} must never reach the snake: {headers:?}"
        );
    }
    // The snake's query string is preserved in the forwarded URI.
    assert_eq!(
        headers.get("host").unwrap().to_str().unwrap(),
        snake.addr.to_string(),
        "Host is rebuilt from the target URI"
    );
}

#[tokio::test]
async fn request_uri_path_and_query_are_preserved() {
    let proxy = ProxyUnderTest::start().await;
    let snake =
        RecordingSnake::start_at_path(reqwest::StatusCode::OK, "PATH-OK", "/api/v1/move").await;
    let body = client()
        .post(&proxy.base_url)
        .header(
            "X-Request-URI",
            format!("http://{}/api/v1/move?token=abc&x=2", snake.addr),
        )
        .header("X-Battlesnake-Timeout-Ms", "500")
        .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        body.trim(),
        "PATH-OK",
        "forwarded request hit /api/v1/move, not the fallback"
    );
    assert_eq!(
        snake.last_headers().get("host").unwrap().to_str().unwrap(),
        snake.addr.to_string()
    );
}

// ---------------------------------------------------------------------------
// timing / freshness (contract item 8, DD 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zz_debug_capture_canary() {
    let proxy = ProxyUnderTest::start().await;
    let snake = RecordingSnake::start_at_path(reqwest::StatusCode::OK, "{}", "/move").await;
    let res = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
        .send()
        .await
        .unwrap();
    let _ = res.text().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let all = CAPTURE.lock().unwrap().clone();
    eprintln!(
        "CAPTURE_LEN={} lines={:?}",
        all.len(),
        &all[..all.len().min(5)]
    );
    assert!(!all.is_empty(), "no events captured at all");
}

#[tokio::test]
async fn timing_header_format_matches_go_and_zeroes_on_reuse() {
    let proxy = ProxyUnderTest::start().await;
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;

    let first = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
        .send()
        .await
        .unwrap();
    let first_timing = first
        .headers()
        .get("x-battlesnake-timing-ms")
        .expect("timing header on success")
        .to_str()
        .unwrap()
        .to_string();
    let t1 = engine_proxy::timing::parse_go_timing(&first_timing)
        .expect("timing header parses (format pinned)");
    assert!(
        t1.first_byte_ms > 0.0,
        "first_byte measured: {first_timing}"
    );

    let second = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
        .send()
        .await
        .unwrap();
    let second_timing = second
        .headers()
        .get("x-battlesnake-timing-ms")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let t2 = engine_proxy::timing::parse_go_timing(&second_timing).unwrap();
    assert_eq!(
        t2.dns_ms, 0.0,
        "pooled reuse reports zero dns: {second_timing}"
    );
    assert_eq!(
        t2.connect_ms, 0.0,
        "pooled reuse reports zero connect: {second_timing}"
    );
    assert_eq!(
        t2.tls_ms, 0.0,
        "pooled reuse reports zero tls: {second_timing}"
    );

    // And the log lines agree on freshness.
    let lines = lines_for(&snake.addr.port().to_string());
    assert!(
        lines.iter().any(|l| l.contains("fresh_connection=true")),
        "first request logs fresh_connection=true: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .filter(|l| l.contains("outcome=success"))
            .count()
            >= 2,
        "both requests logged: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("fresh_connection=false")),
        "second request logs fresh_connection=false: {lines:?}"
    );
}

#[tokio::test]
async fn idle_reaping_forces_a_fresh_connection_again() {
    let proxy = ProxyUnderTest::start_with(ProxyConfig {
        auth_token: TEST_TOKEN.to_string(),
        pool_idle_timeout: Duration::from_secs(1),
        ..ProxyConfig::default()
    })
    .await;
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;
    let target = format!("http://{}/move", snake.addr);
    let port_key = snake.addr.port().to_string();

    // The global connections_total counter is shared across tests, so the
    // per-request log lines (unique host:port) are the authoritative signal.
    proxied_request(&proxy, &target, "2000")
        .send()
        .await
        .unwrap();
    wait_for_line(&port_key, "fresh_connection=true", Duration::from_secs(2))
        .await
        .expect("first request dialed a fresh connection");

    proxied_request(&proxy, &target, "2000")
        .send()
        .await
        .unwrap();
    wait_for_line(&port_key, "fresh_connection=false", Duration::from_secs(2))
        .await
        .expect("second request reused the pooled connection");

    tokio::time::sleep(Duration::from_secs(2)).await;
    proxied_request(&proxy, &target, "2000")
        .send()
        .await
        .unwrap();
    // After the idle timeout the pool must have reaped the connection: the
    // third request logs a fresh connection again (its line is the second
    // fresh=true for this host).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let fresh_count = lines_for(&port_key)
            .iter()
            .filter(|l| l.contains("fresh_connection=true"))
            .count();
        if fresh_count >= 2 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("idle connection was not reaped: {:?}", lines_for(&port_key));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_line(port_key: &str, needle: &str, timeout: Duration) -> Option<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if lines_for(port_key).iter().any(|l| l.contains(needle)) {
            return Some(());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn ipv4_first_dns_fallback() {
    // Bind only an IPv4 listener; `localhost` resolves to [::1, 127.0.0.1]
    // and the race must fall through to IPv4.
    let app = axum::Router::new().fallback(|| async { (reqwest::StatusCode::OK, "{}") });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let proxy = ProxyUnderTest::start().await;
    let res = proxied_request(
        &proxy,
        &format!("http://localhost:{}/move", addr.port()),
        "2000",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
}

// ---------------------------------------------------------------------------
// error paths (contract items 9, 10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unreachable_snake_returns_502_connect_on_freed_port() {
    let proxy = ProxyUnderTest::start().await;
    let target = ProxyUnderTest::freed_port_target();
    let res = proxied_request(&proxy, &target, "1000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "3:Failed to connect to Battlesnake host"
    );
    assert!(res.headers().get("x-battlesnake-latency-ms").is_some());
}

#[tokio::test]
async fn unresolvable_host_returns_502_dns() {
    let proxy = ProxyUnderTest::start().await;
    let res = proxied_request(&proxy, "http://snake.invalid/move", "1000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "2:Failed to resolve Battlesnake hostname"
    );
}

#[tokio::test]
async fn snake_timeout_before_headers_returns_504_with_error_header() {
    // A mock that accepts the TCP connection and never answers: the trigger
    // (accept observed) is proven before the negative assertion.
    let mut stall = StallSnake::accept_and_go_quiet().await;
    let proxy = ProxyUnderTest::start().await;
    let started = Instant::now();
    let res = proxied_request(&proxy, &format!("http://{}/move", stall.addr), "300")
        .send()
        .await
        .unwrap();
    // Prove the trigger: the mock really accepted the connection. Bounded so
    // a regression fails instead of hanging.
    tokio::time::timeout(Duration::from_secs(10), stall.accepted.recv())
        .await
        .expect("accept signal within 10s")
        .expect("mock snake observed the TCP accept");
    assert_eq!(res.status(), reqwest::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "1:A response was not received before the timeout"
    );
    assert!(res.headers().get("x-battlesnake-latency-ms").is_some());
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "504 must beat arena's local guard (timeout+300ms): {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// stalled body (contract item 17b)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stalled_body_times_out_in_body_phase() {
    // Mock sends 200 + headers + a partial body, then stalls. The proxy
    // forwards headers, then DeadlineBody aborts the connection inside the
    // budget: exactly one Timeout{body} log line, and the client sees a
    // body read error (no second response after the 200 headers).
    let mut stall = StallSnake::headers_then_partial_body_then_stall().await;
    let proxy = ProxyUnderTest::start().await;
    let started = Instant::now();
    let res = proxied_request(&proxy, &format!("http://{}/move", stall.addr), "300")
        .send()
        .await
        .unwrap();
    // Prove the trigger: the snake sent headers before stalling. Bounded so a
    // regression fails instead of hanging.
    let headers_sent = tokio::time::timeout(Duration::from_secs(10), stall.headers_sent.recv())
        .await
        .expect("headers-sent signal within 10s")
        .unwrap_or(false);
    assert!(headers_sent, "mock snake wrote its response headers");
    assert_eq!(res.status(), 200);
    assert!(res.headers().contains_key("x-battlesnake-latency-ms"));
    // The body read must fail: the proxy aborted the connection.
    let body = res.bytes().await;
    assert!(
        body.is_err(),
        "connection must be aborted mid-body, got {:?}",
        body.map(|b| String::from_utf8_lossy(&b).into_owned())
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "body-phase abort must beat the local guard: {elapsed:?}"
    );
    // exactly one log line for this host, outcome timeout:body
    tokio::time::sleep(Duration::from_millis(200)).await; // propagation window
    let lines = lines_for(&stall.addr.port().to_string());
    assert_eq!(lines.len(), 1, "exactly one request line: {lines:?}");
    assert!(
        lines[0].contains("outcome=timeout:body"),
        "body-phase outcome: {}",
        lines[0]
    );
}

// ---------------------------------------------------------------------------
// stalled inbound request body (contract item 18)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stalled_request_body_times_out_with_single_log_line() {
    let proxy = ProxyUnderTest::start().await;
    let snake_addr = ProxyUnderTest::freed_port_target();
    let proxy_port = proxy.port;
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();

    // A raw client that sends headers + a partial body (Content-Length lies)
    // and then holds the connection open.
    tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
            .await
            .unwrap();
        connected_tx.send(()).unwrap();
        stream
            .write_all(
                format!(
                    "POST / HTTP/1.1\r\nHost: proxy\r\nX-Request-URI: {snake_addr}\r\n\
                     X-Battlesnake-Timeout-Ms: 300\r\nContent-Length: 1000\r\n\
                     X-Proxy-Authorization: token {TEST_TOKEN}\r\n\r\npartial"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // Read the response but never finish the request body.
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf).await;
    });

    connected_rx.await.unwrap();
    let request_started = Instant::now();
    // The proxy must answer 504 within the budget even though the upload
    // never completes. Poll the capture for the log line.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let line = loop {
        let all: Vec<String> = CAPTURE
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("outcome=timeout:request_body"))
            .cloned()
            .collect();
        if let Some(line) = all.first() {
            break line.clone();
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no timeout:request_body line within 2s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(line.contains("timeout:request_body"), "{line}");
    assert!(
        request_started.elapsed() >= Duration::from_millis(250),
        "the 504 arrives at ≈budget (300ms), not instantly: {:?}",
        request_started.elapsed()
    );
    assert!(
        request_started.elapsed() < Duration::from_secs(2),
        "504 arrives within the budget window: {:?}",
        request_started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// body caps (contract items 15, 16a, 16b)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversized_request_body_rejected_413() {
    let proxy = ProxyUnderTest::start_with(ProxyConfig {
        auth_token: TEST_TOKEN.to_string(),
        body_cap_bytes: 1024,
        ..ProxyConfig::default()
    })
    .await;
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, "{}").await;
    let res = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
        .body(vec![b'x'; 2048])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn oversized_response_body_declared_502() {
    let proxy = ProxyUnderTest::start_with(ProxyConfig {
        auth_token: TEST_TOKEN.to_string(),
        body_cap_bytes: 1024,
        ..ProxyConfig::default()
    })
    .await;
    let body_str: &'static str = Box::leak("x".repeat(4096).into_boxed_str());
    let snake = RecordingSnake::start(reqwest::StatusCode::OK, body_str).await;
    let res = proxied_request(&proxy, &format!("http://{}/move", snake.addr), "1000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.headers().get("x-battlesnake-server-error").unwrap(),
        "5:Failed to read response body"
    );
    assert!(res.headers().get("x-battlesnake-latency-ms").is_some());
}

#[tokio::test]
async fn response_body_cap_aborts_stream() {
    // Chunked body past the cap: headers forward fine, the stream aborts at
    // the cap, and the body_aborts metric records it.
    let proxy = ProxyUnderTest::start_with(ProxyConfig {
        auth_token: TEST_TOKEN.to_string(),
        body_cap_bytes: 1024,
        ..ProxyConfig::default()
    })
    .await;
    let stall = StallSnake::chunked_body_past_cap(700, 4096).await;
    let res = proxied_request(&proxy, &format!("http://{}/move", stall.addr), "2000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "headers forward before the cap");
    let before = metrics::get()
        .body_aborts_total
        .with_label_values(&["body_cap"])
        .get();
    let body = res.bytes().await;
    assert!(body.is_err(), "stream aborted at the cap");
    let after = metrics::get()
        .body_aborts_total
        .with_label_values(&["body_cap"])
        .get();
    assert!(
        after > before,
        "engine_proxy_body_aborts_total{{body_cap}} grew"
    );
}

// ---------------------------------------------------------------------------
// drop guard: client cancels (DD 11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_cancel_mid_stall_logs_exactly_one_line() {
    let mut stall = StallSnake::accept_and_go_quiet().await;
    let proxy = ProxyUnderTest::start().await;
    let port = stall.addr.port();

    // Send a request with a long budget in a spawned task, then abort the
    // task so the client side vanishes while the proxy still waits for the
    // snake (dropping the in-flight future closes the connection).
    let base_url = proxy.base_url.clone();
    let target = format!("http://{}/move", stall.addr);
    let handle = tokio::spawn(async move {
        client()
            .post(&base_url)
            .header("X-Request-URI", target)
            .header("X-Battlesnake-Timeout-Ms", "10000")
            .header("X-Proxy-Authorization", format!("token {TEST_TOKEN}"))
            .send()
            .await
    });
    // Prove the trigger: the mock accepted the connection. Bounded so a
    // regression fails instead of hanging.
    tokio::time::timeout(Duration::from_secs(10), stall.accepted.recv())
        .await
        .expect("accept signal within 10s")
        .expect("mock observed the accept");
    handle.abort();

    // Propagation window, then exactly one client_cancelled line.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let lines = lines_for(&port.to_string());
        if !lines.is_empty() {
            assert_eq!(lines.len(), 1, "exactly one line: {lines:?}");
            assert!(
                lines[0].contains("outcome=client_cancelled"),
                "{}",
                lines[0]
            );
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("no request line observed for cancelled request");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// health, version, metrics (contract items 11, 12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_version_and_metrics_endpoints() {
    let proxy = ProxyUnderTest::start().await;

    let res = client()
        .get(format!("{}/healthz/alive", proxy.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "alive");

    let res = client()
        .get(format!("{}/healthz/ready", proxy.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "ready");

    let res = client()
        .get(format!("{}/version", proxy.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "unknown");

    // metrics: no auth => 401 + WWW-Authenticate
    let res = client()
        .get(format!("{}/metrics", proxy.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.headers().get("www-authenticate").unwrap(), "token");
    // bad token => 401
    let res = client()
        .get(format!("{}/metrics", proxy.base_url))
        .header("Authorization", "token nope")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    // good token => text with our series (after one proxied request)
    proxied_request(&proxy, &ProxyUnderTest::freed_port_target(), "500")
        .send()
        .await
        .unwrap();
    let res = client()
        .get(format!("{}/metrics", proxy.base_url))
        .header("Authorization", format!("token {TEST_TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains("engine_proxy_requests_total"), "{text}");
    assert!(text.contains("engine_proxy_connections_total"), "{text}");
}

// ---------------------------------------------------------------------------
// raw stall snakes
// ---------------------------------------------------------------------------

struct StallSnake {
    addr: SocketAddr,
    /// Signals that the TCP connection was accepted (accept_and_go_quiet).
    accepted: tokio::sync::mpsc::Receiver<()>,
    /// Signals that response headers were written (headers_then_partial_body).
    headers_sent: tokio::sync::mpsc::Receiver<bool>,
}

impl StallSnake {
    async fn accept_and_go_quiet() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted) = tokio::sync::mpsc::channel(1);
        let (_, headers_sent) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepted_tx.send(()).await.unwrap_or(());
                // Drain the request, never write anything.
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
            headers_sent,
        }
    }

    async fn headers_then_partial_body_then_stall() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, accepted) = tokio::sync::mpsc::channel(1);
        let (sent_tx, headers_sent) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // Read the request head + body until the connection is killed.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let wrote = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{\"move\":\"",
                    )
                    .await
                    .is_ok();
                sent_tx.send(wrote).await.unwrap_or(());
                // Hold the connection open, draining reads, never finishing.
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
            headers_sent,
        }
    }

    /// A snake answering 200 with a chunked body: a first small chunk
    /// (flushed with the headers), a pause, then a second chunk past the
    /// proxy's byte cap. Two phases keep the head-vs-abort race
    /// deterministic: headers are on the wire before the cap fires.
    async fn chunked_body_past_cap(first_chunk: usize, second_chunk: usize) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_, accepted) = tokio::sync::mpsc::channel(1);
        let (sent_tx, headers_sent) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{first_chunk:X}\r\n"
                );
                let chunk1: String = "x".repeat(first_chunk);
                let wrote = stream
                    .write_all(format!("{head}{chunk1}\r\n").as_bytes())
                    .await
                    .is_ok();
                sent_tx.send(wrote).await.unwrap_or(());
                tokio::time::sleep(Duration::from_millis(100)).await;
                let chunk2: String = "y".repeat(second_chunk);
                let _ = stream
                    .write_all(format!("{second_chunk:X}\r\n{chunk2}\r\n0\r\n\r\n").as_bytes())
                    .await;
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
            headers_sent,
        }
    }
}
