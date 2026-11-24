//! The `play` subcommand's HTTP caller for `GET /`, `/start`, `/move`, `/end`.
//!
//! Deliberately separate from the server's snake client (no proxy routing,
//! no tracing): the server keeps arena-prod behaviour, and `play` runs
//! offline games. All *response interpretation* (direction parsing, shout
//! sanitizing, fallbacks, the 64 KB cap, `MoveResult` semantics) comes from
//! `engine_core::move_response` and `engine_core::http_util`.
//!
//! Client methods never write logs: `run_game` owns the log writer (a
//! `&mut dyn Write` cannot be shared across concurrent futures), so
//! `start`/`end` return their failure text and `request_move` returns the
//! warning text (Go's WARN lines: URL, status, body, real error) alongside
//! the `MoveResult` for `run_game` to log.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use rules::Direction;

use engine_core::move_response::{
    BODY_READ_CAP_BYTES, MoveOutcome, MoveResponse, MoveResult, interpret_move_response,
    parse_failure_result, transport_fallback_result,
};

/// `GET /` as the CLI reads it: arena's shared metadata plus the
/// `author` only the board view uses. Not a field on
/// `SnakeInfoResponse`, which the server parses every snake's metadata
/// through — a non-string `author` must stay an ignored unknown field
/// there, not a parse failure (Architecture Rule 12 of DEV-2146).
#[derive(Debug)]
pub struct PingInfo {
    pub info: engine_core::wire::SnakeInfoResponse,
    pub author: String,
}

/// Thin async client for one snake server. `timeout` bounds the ENTIRE
/// round trip — send and body read — matching the server's `request_move`;
/// a snake that answers headers and then stalls is a real timeout, like
/// Go's `http.Client{Timeout}` would produce.
///
/// Request bodies arrive pre-serialized (`&str`): the client has no
/// `Result` return to propagate a serialization failure through, so the
/// caller — which does — serializes.
pub struct SnakeHttpClient {
    /// `Err` carries the formatted construction error: every request then
    /// fails at "transport" with that text instead of panicking on a
    /// build failure (CLAUDE.md forbids unwrap/expect in production code;
    /// reqwest offers no infallible constructor to fall back to).
    client: std::result::Result<reqwest::Client, String>,
    timeout: Duration,
    verbose: bool,
}

impl SnakeHttpClient {
    pub fn new(timeout_ms: i64, verbose: bool) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("battlesnake-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| err.to_string());
        Self {
            client,
            timeout: Duration::from_millis(timeout_ms.max(1) as u64),
            verbose,
        }
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    /// `GET /` — fetch snake metadata. Non-2xx is fatal (a deliberate
    /// divergence from the Go CLI, whose ping ignores the status code: a
    /// deployed snake developer's local run should not print a board for a
    /// snake that is actually down).
    pub async fn ping(&self, url: &str) -> Result<PingInfo> {
        let outcome = self
            .round_trip(|client| {
                let request = client.get(url);
                async move { request.send().await }
            })
            .await;

        match outcome {
            RoundTrip::TimedOut => Err(eyre!(
                "Snake metadata request to {url} failed: request timed out"
            )),
            RoundTrip::Transport(err) => {
                Err(eyre!("Snake metadata request to {url} failed: {err}"))
            }
            RoundTrip::Body(_, Err(err)) => {
                Err(eyre!("Error reading from snake metadata URL {url}: {err}"))
            }
            RoundTrip::Body(status, Ok(body)) => {
                if !(200..300).contains(&status) {
                    return Err(eyre!(
                        "Snake metadata request to {url} returned status {status}"
                    ));
                }
                let info = serde_json::from_str(&body)
                    .map_err(|err| eyre!("Failed to parse response from {url}: {err}"))?;
                // The ping's `author` is board-view-only; read it from the
                // same body as a Value so a missing or non-string value
                // becomes "" instead of failing the ping.
                let author = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("author")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                Ok(PingInfo { info, author })
            }
        }
    }

    /// POST `/start`. Fire-and-forget like Go: failures are returned as
    /// text for the caller to log.
    pub async fn start(&self, url: &str, json_body: &str) -> Option<String> {
        self.post_fire_and_forget(url, "start", json_body).await
    }

    /// POST `/end`. Fire-and-forget like Go: failures are returned as text
    /// for the caller to log.
    pub async fn end(&self, url: &str, json_body: &str) -> Option<String> {
        self.post_fire_and_forget(url, "end", json_body).await
    }

    /// POST `/move` and interpret the response. Non-200 with a valid move
    /// uses that move (arena semantics, not Go's fallback-to-last).
    ///
    /// Returns the `MoveResult` plus, when the round trip failed in any way
    /// Go would WARN about, Go's warning text (URL, status, body, real
    /// error — `play.go` `getSnakeUpdate`) for the caller to log. The log
    /// text must NOT be derived from `MoveOutcome::timed_out()`, which is
    /// true for every transport failure (server parity) and would call a
    /// refused connection a timeout.
    pub async fn request_move(
        &self,
        url: &str,
        json_body: &str,
        snake_id: &str,
        last_direction: Option<Direction>,
    ) -> (MoveResult, Option<String>) {
        let move_url = engine_core::http_util::build_endpoint_url(url, "move");

        let start = std::time::Instant::now();
        let outcome = self
            .round_trip(|client| {
                let request = client
                    .post(&move_url)
                    .header("content-type", "application/json")
                    .body(json_body.to_string());
                async move { request.send().await }
            })
            .await;
        let elapsed = start.elapsed().as_millis() as i64;

        match outcome {
            RoundTrip::TimedOut => (
                transport_fallback_result(snake_id, last_direction, None, MoveOutcome::TimedOut),
                Some(format!(
                    "Request to {move_url} failed\n\tError: request timed out (after {} ms)",
                    self.timeout.as_millis()
                )),
            ),
            RoundTrip::Transport(err) => (
                // Same as the server: a send-phase network error is a
                // transport fallback, timed_out = true.
                transport_fallback_result(snake_id, last_direction, None, MoveOutcome::Transport),
                Some(format!("Request to {move_url} failed\n\tError: {err}")),
            ),
            RoundTrip::Body(status, Err(err)) => (
                parse_failure_result(snake_id, last_direction, Some(elapsed), status),
                Some(format!(
                    "Failed to read response body from {move_url}\n\tError: {err}"
                )),
            ),
            RoundTrip::Body(status, Ok(body)) => {
                let interpreted =
                    interpret_move_response(snake_id, &body, status, Some(elapsed), last_direction);
                // Go checks the status before decoding (play.go
                // getSnakeUpdate): every status other than exactly 200 —
                // including other 2xx like 201 — warns with the body it
                // got, whatever the body is.
                if status != 200 {
                    let result = interpreted.unwrap_or_else(|_| {
                        parse_failure_result(snake_id, last_direction, Some(elapsed), status)
                    });
                    (
                        result,
                        Some(format!(
                            "Got non-ok status code from {move_url}\n\
                             \tStatusCode: {status} (expected 200)\n\
                             \tBody: {body:?}"
                        )),
                    )
                } else {
                    match interpreted {
                        Ok(result) if !result.outcome.errored() => (result, None),
                        // 2xx but the direction was unrecognized.
                        Ok(result) => {
                            let raw_move = serde_json::from_str::<MoveResponse>(&body)
                                .map(|r| r.direction)
                                .unwrap_or_default();
                            (
                                result,
                                Some(format!(
                                    "Failed to parse JSON data from {move_url}\n\
                                     \tError: invalid move {raw_move:?}, valid moves are \
                                     \"up\", \"down\", \"left\" or \"right\"\n\
                                     \tBody: {body:?}\n\
                                     \tSee https://docs.battlesnake.com/references/api#post-move"
                                )),
                            )
                        }
                        // 2xx but the body is not a move response.
                        Err(json_err) => (
                            parse_failure_result(snake_id, last_direction, Some(elapsed), status),
                            Some(format!(
                                "Failed to decode JSON from {move_url}\n\
                                 \tError: {json_err}\n\
                                 \tBody: {body:?}\n\
                                 \tSee https://docs.battlesnake.com/references/api#post-move"
                            )),
                        ),
                    }
                }
            }
        }
    }

    async fn post_fire_and_forget(
        &self,
        url: &str,
        endpoint: &str,
        json_body: &str,
    ) -> Option<String> {
        let endpoint_url = engine_core::http_util::build_endpoint_url(url, endpoint);
        let outcome = self
            .round_trip(|client| {
                let request = client
                    .post(&endpoint_url)
                    .header("content-type", "application/json")
                    .body(json_body.to_string());
                async move { request.send().await }
            })
            .await;
        match outcome {
            RoundTrip::TimedOut => Some(format!("Request to {endpoint_url} timed out")),
            RoundTrip::Transport(err) => Some(format!("Request to {endpoint_url} failed: {err}")),
            RoundTrip::Body(_, Ok(_)) => None,
            RoundTrip::Body(status, Err(err)) => Some(format!(
                "Request to {endpoint_url} failed: status {status}, {err}"
            )),
        }
    }

    /// Run one full send→read round trip under the timeout. The body read is
    /// inside the bound (Rule: the whole HTTP round trip is bounded). A
    /// client-construction failure surfaces here as a transport failure.
    async fn round_trip<F, Fut>(&self, make_request: F) -> RoundTrip
    where
        F: FnOnce(&reqwest::Client) -> Fut,
        Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    {
        let client = match &self.client {
            Ok(client) => client,
            // Construction failed: every request reports the same
            // transport failure instead of panicking.
            Err(err) => return RoundTrip::Transport(err.clone()),
        };
        let fut = async {
            let response = make_request(client).await?;
            let status = response.status().as_u16();
            let body =
                engine_core::http_util::read_body_capped(response, BODY_READ_CAP_BYTES).await;
            Ok::<_, reqwest::Error>((status, body))
        };
        match tokio::time::timeout(self.timeout, fut).await {
            Err(_elapsed) => RoundTrip::TimedOut,
            Ok(Ok((status, body))) => RoundTrip::Body(status, body.map_err(|err| err.to_string())),
            Ok(Err(err)) => RoundTrip::Transport(err.to_string()),
        }
    }
}

/// One bounded round trip's outcome. `Transport` carries the formatted
/// error text (either the reqwest error or the stored construction
/// failure); `Body(_, Err(_))` means headers arrived but the body read
/// failed (a non-timeout read error).
enum RoundTrip {
    TimedOut,
    Transport(String),
    Body(u16, std::result::Result<String, String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::wire;
    use rules::Direction;
    use std::io::{Read as _, Write as _};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(timeout_ms: i64) -> SnakeHttpClient {
        SnakeHttpClient::new(timeout_ms, false)
    }

    fn body_for_move() -> String {
        // Minimal serializable Game; the content doesn't matter to the mock.
        let game = wire::Game {
            game: wire::NestedGame {
                id: "g".into(),
                ruleset: wire::Ruleset {
                    name: "standard".into(),
                    version: "cli".into(),
                    settings: Default::default(),
                },
                timeout: 500,
                map: "standard".into(),
                source: "custom".into(),
            },
            turn: 0,
            board: wire::Board {
                height: 11,
                width: 11,
                food: vec![],
                snakes: vec![],
                hazards: vec![],
            },
            you: wire::BattleSnake {
                id: "s1".into(),
                name: "S1".into(),
                health: 100,
                body: vec![],
                head: wire::Position { x: 0, y: 0 },
                length: 0,
                latency: "0".into(),
                shout: String::new(),
                squad: String::new(),
                customizations: Default::default(),
            },
        };
        serde_json::to_string(&game).unwrap()
    }

    #[tokio::test]
    async fn non_200_with_valid_move_uses_the_move_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"move":"right"}"#))
            .mount(&server)
            .await;

        let (result, warning) = client(2000)
            .request_move(&server.uri(), &body_for_move(), "s1", None)
            .await;

        assert_eq!(
            result.direction,
            Direction::Right,
            "the move is used, like arena"
        );
        assert_eq!(result.outcome, MoveOutcome::HttpError { status_code: 500 });
        let warning = warning.expect("non-200 carries Go's warning");
        assert!(
            warning.starts_with(&format!(
                "Got non-ok status code from {}/move",
                server.uri()
            )),
            "{warning}"
        );
        assert!(
            warning.contains("StatusCode: 500 (expected 200)"),
            "{warning}"
        );
        // `{:?}` quotes the body like Go's %q.
        assert!(
            warning.contains("Body: \"{\\\"move\\\":\\\"right\\\"}\""),
            "{warning}"
        );
    }

    /// Go's getSnakeUpdate warns for every status other than 200. A 201
    /// response may carry a usable move, but it must still name the endpoint
    /// and status so the developer can fix the snake's HTTP contract.
    #[tokio::test]
    async fn created_move_response_warns_about_non_200_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"move":"right"}"#))
            .mount(&server)
            .await;

        let (result, warning) = client(2000)
            .request_move(&server.uri(), &body_for_move(), "s1", None)
            .await;
        assert_eq!(result.direction, Direction::Right);
        let warning = warning.expect("Go warns on status 201 even with a usable move");
        assert!(warning.contains(&format!("{}/move", server.uri())));
        assert!(warning.contains("StatusCode: 201 (expected 200)"));
    }

    #[tokio::test]
    async fn timeout_falls_back_to_last_direction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3)))
            .mount(&server)
            .await;

        let started = std::time::Instant::now();
        let (result, warning) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client(100).request_move(&server.uri(), &body_for_move(), "s1", Some(Direction::Left)),
        )
        .await
        .expect("no hang under the whole-round-trip bound (10x margin)");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        assert_eq!(result.direction, Direction::Left);
        assert!(result.outcome.timed_out());
        assert!(!result.outcome.errored());
        assert_eq!(result.outcome.status_code(), None);
        let warning = warning.expect("a timeout carries Go's warning");
        assert!(
            warning.starts_with(&format!("Request to {}/move failed", server.uri())),
            "{warning}"
        );
        assert!(warning.contains("timed out"), "{warning}");
    }

    #[tokio::test]
    async fn unparseable_body_is_a_parse_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let (result, warning) = client(2000)
            .request_move(&server.uri(), &body_for_move(), "s1", Some(Direction::Down))
            .await;

        assert_eq!(result.direction, Direction::Down, "fallback to last move");
        assert_eq!(result.outcome, MoveOutcome::Invalid { status_code: 200 });
        let warning = warning.expect("a parse failure carries Go's warning");
        assert!(
            warning.starts_with(&format!("Failed to decode JSON from {}/move", server.uri())),
            "{warning}"
        );
        assert!(warning.contains("Body: \"not json\""), "{warning}");
        assert!(
            warning.contains("https://docs.battlesnake.com/references/api#post-move"),
            "{warning}"
        );
    }

    #[tokio::test]
    async fn transport_failure_falls_back_timed_out() {
        // A listener that accepts every connection and closes it without a
        // response: the request fails at transport, deterministically, on
        // every OS. A refused connect reaches the same Transport branch in
        // round_trip, but bind-then-drop relies on the OS refusing the
        // released port quickly: CI showed a connect to the closed port
        // taking longer than 500 ms on the Windows runner, so at
        // client(500) it hit the timeout instead — whether it would
        // eventually refuse given longer was never measured (wiremock
        // pools servers, so a dropped MockServer would keep answering
        // 404).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });

        let (result, warning) = client(2000)
            .request_move(&url, &body_for_move(), "s1", Some(Direction::Up))
            .await;
        assert!(result.outcome.timed_out());
        assert!(!result.outcome.errored());
        assert_eq!(result.direction, Direction::Up);
        // The warning names the URL and the real error — a dead connection
        // is NOT reported as a timeout.
        let warning = warning.expect("a transport failure carries Go's warning");
        assert!(
            warning.starts_with(&format!("Request to {url}/move failed")),
            "{warning}"
        );
        assert!(!warning.contains("timed out"), "{warning}");
    }

    #[tokio::test]
    async fn ping_non_2xx_names_the_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = client(2000)
            .ping(&server.uri())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("returned status 503"), "{err}");
        assert!(err.contains(&server.uri()), "{err}");
    }

    #[tokio::test]
    async fn ping_transport_failure_names_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let err = client(500).ping(&url).await.unwrap_err().to_string();
        assert!(err.contains("Snake metadata request to"), "{err}");
        assert!(err.contains(&url), "{err}");
    }

    #[tokio::test]
    async fn ping_parse_failure_names_the_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&server)
            .await;

        let err = client(2000)
            .ping(&server.uri())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to parse response from"), "{err}");
        assert!(err.contains(&server.uri()), "{err}");
    }

    /// End-to-end UA assertion: the mock only matches when the request
    /// carries the CLI's user agent.
    #[tokio::test]
    async fn requests_identify_as_battlesnake_cli() {
        let server = MockServer::start().await;
        let expected_ua = format!("battlesnake-cli/{}", env!("CARGO_PKG_VERSION"));
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("user-agent", expected_ua))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r##"{"apiversion":"1","color":"#888800"}"##),
            )
            .mount(&server)
            .await;

        let info = client(2000).ping(&server.uri()).await.expect("UA matched");
        assert_eq!(info.info.color.as_deref(), Some("#888800"));
    }

    /// The ping's `author`: a string is carried through; a non-string or
    /// missing value is `""` and never fails the ping (it is board-view
    /// display data, not snake metadata the game depends on).
    #[tokio::test]
    async fn ping_author_is_string_or_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r##"{"apiversion":"1","author":"byte","color":"#fff"}"##),
            )
            .mount(&server)
            .await;
        let ping = client(2000).ping(&server.uri()).await.expect("parses");
        assert_eq!(ping.author, "byte");
        assert_eq!(ping.info.color.as_deref(), Some("#fff"));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r##"{"apiversion":"1","author":5,"color":"#fff"}"##),
            )
            .mount(&server)
            .await;
        let ping = client(2000).ping(&server.uri()).await.expect("parses");
        assert_eq!(ping.author, "");
        assert_eq!(ping.info.color.as_deref(), Some("#fff"));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r##"{"apiversion":"1","color":"#fff"}"##),
            )
            .mount(&server)
            .await;
        let ping = client(2000).ping(&server.uri()).await.expect("parses");
        assert_eq!(ping.author, "");
    }

    /// A snake that writes headers and then stalls: the whole round trip
    /// (send AND body read) is bounded by the timeout, so play cannot hang.
    #[tokio::test]
    async fn stalled_body_is_bounded_by_the_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read (and discard) the request, answer headers only.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                       Content-Length: 16\r\nConnection: close\r\n\r\n",
                );
                // Never send the body. Hold the stream open until the test
                // client gives up.
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });

        let url = format!("http://{addr}");
        let started = std::time::Instant::now();
        let (result, _warning) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client(100).request_move(&url, &body_for_move(), "s1", None),
        )
        .await
        .expect("stalled body does not hang (10x margin over the 100ms budget)");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(result.outcome.timed_out());
        assert_eq!(result.direction, Direction::Up);
        let _ = server_thread.join();
    }

    /// /start and /end are warn-only: a dead endpoint returns failure text
    /// instead of erroring.
    #[tokio::test]
    async fn start_and_end_failures_return_text() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let client = client(200);
        let body = body_for_move();
        let start = client.start(&url, &body).await;
        assert!(start.is_some(), "start failure text returned");
        let end = client.end(&url, &body).await;
        assert!(end.is_some(), "end failure text returned");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        assert!(client.start(&server.uri(), &body).await.is_none());
        assert!(client.end(&server.uri(), &body).await.is_none());
    }
}
