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
//! `start`/`end` return their failure text and `run_game` logs it.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use rules::Direction;

use engine_core::move_response::{
    BODY_READ_CAP_BYTES, MoveResult, interpret_move_response, parse_failure_result,
    transport_fallback_result,
};
use engine_core::wire::Game;

/// Thin async client for one snake server. `timeout` bounds the ENTIRE
/// round trip — send and body read — unlike the server's `request_move`
/// (which wraps only `.send()`); a snake that answers headers and then
/// stalls wedges a local game forever under a send-only bound, where Go's
/// `http.Client{Timeout}` would fire.
pub struct SnakeHttpClient {
    client: reqwest::Client,
    timeout: Duration,
    verbose: bool,
}

impl SnakeHttpClient {
    pub fn new(timeout_ms: i64, verbose: bool) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("battlesnake-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client construction has no failure inputs here");
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
    pub async fn ping(&self, url: &str) -> Result<engine_core::wire::SnakeInfoResponse> {
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
                serde_json::from_str(&body)
                    .map_err(|err| eyre!("Failed to parse response from {url}: {err}"))
            }
        }
    }

    /// POST `/start`. Fire-and-forget like Go: failures are returned as
    /// text for the caller to log.
    pub async fn start(&self, url: &str, body: &Game) -> Option<String> {
        self.post_fire_and_forget(url, "start", body).await
    }

    /// POST `/end`. Fire-and-forget like Go: failures are returned as text
    /// for the caller to log.
    pub async fn end(&self, url: &str, body: &Game) -> Option<String> {
        self.post_fire_and_forget(url, "end", body).await
    }

    /// POST `/move` and interpret the response. Non-200 with a valid move
    /// uses that move (arena semantics, not Go's fallback-to-last).
    pub async fn request_move(
        &self,
        url: &str,
        body: &Game,
        snake_id: &str,
        last_direction: Option<Direction>,
    ) -> MoveResult {
        let move_url = engine_core::http_util::build_endpoint_url(url, "move");
        let json = serde_json::to_string(body).expect("wire Game is always serializable");

        let start = std::time::Instant::now();
        let outcome = self
            .round_trip(|client| {
                let request = client
                    .post(&move_url)
                    .header("content-type", "application/json")
                    .body(json.clone());
                async move { request.send().await }
            })
            .await;
        let elapsed = start.elapsed().as_millis() as i64;

        match outcome {
            RoundTrip::TimedOut => transport_fallback_result(snake_id, last_direction, None, true),
            RoundTrip::Transport(_) => {
                // Same as the server: a send-phase network error is a
                // transport fallback, timed_out = true.
                transport_fallback_result(snake_id, last_direction, None, true)
            }
            RoundTrip::Body(status, Err(_)) => {
                parse_failure_result(snake_id, last_direction, Some(elapsed), Some(status))
            }
            RoundTrip::Body(status, Ok(body)) => {
                match interpret_move_response(
                    snake_id,
                    &body,
                    Some(status),
                    Some(elapsed),
                    last_direction,
                ) {
                    Ok(result) => result,
                    Err(_) => {
                        parse_failure_result(snake_id, last_direction, Some(elapsed), Some(status))
                    }
                }
            }
        }
    }

    async fn post_fire_and_forget(&self, url: &str, endpoint: &str, body: &Game) -> Option<String> {
        let endpoint_url = engine_core::http_util::build_endpoint_url(url, endpoint);
        let json = serde_json::to_string(body).expect("wire Game is always serializable");
        let outcome = self
            .round_trip(|client| {
                let request = client
                    .post(&endpoint_url)
                    .header("content-type", "application/json")
                    .body(json.clone());
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
    /// inside the bound (Rule: the whole HTTP round trip is bounded).
    async fn round_trip<F, Fut>(&self, make_request: F) -> RoundTrip
    where
        F: FnOnce(&reqwest::Client) -> Fut,
        Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    {
        let client = &self.client;
        let fut = async {
            let response = make_request(client).await?;
            let status = response.status().as_u16();
            let body =
                engine_core::http_util::read_body_capped(response, BODY_READ_CAP_BYTES).await;
            Ok::<_, reqwest::Error>((status, body))
        };
        match tokio::time::timeout(self.timeout, fut).await {
            Err(_elapsed) => RoundTrip::TimedOut,
            Ok(Ok((status, body))) => RoundTrip::Body(status, body),
            Ok(Err(err)) => RoundTrip::Transport(err),
        }
    }
}

/// One bounded round trip's outcome. `Body(_, Err(_))` means headers arrived
/// but the body read failed (a non-timeout read error).
enum RoundTrip {
    TimedOut,
    Transport(reqwest::Error),
    Body(u16, std::result::Result<String, reqwest::Error>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::move_response::MoveResult;
    use engine_core::wire;
    use rules::Direction;
    use std::io::{Read as _, Write as _};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(timeout_ms: i64) -> SnakeHttpClient {
        SnakeHttpClient::new(timeout_ms, false)
    }

    fn body_for_move() -> Game {
        // Minimal serializable Game; the content doesn't matter to the mock.
        let mut game = Game {
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
        game.game.ruleset.name = "standard".into();
        game
    }

    #[tokio::test]
    async fn non_200_with_valid_move_uses_the_move_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"move":"right"}"#))
            .mount(&server)
            .await;

        let result: MoveResult = client(2000)
            .request_move(&server.uri(), &body_for_move(), "s1", None)
            .await;

        assert_eq!(
            result.direction,
            Direction::Right,
            "the move is used, like arena"
        );
        assert!(result.errored, "non-2xx is still an error");
        assert!(!result.timed_out);
        assert_eq!(result.status_code, Some(500));
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
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client(100).request_move(&server.uri(), &body_for_move(), "s1", Some(Direction::Left)),
        )
        .await
        .expect("no hang under the whole-round-trip bound (10x margin)");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        assert_eq!(result.direction, Direction::Left);
        assert!(result.timed_out);
        assert!(!result.errored);
        assert_eq!(result.status_code, None);
    }

    #[tokio::test]
    async fn unparseable_body_is_a_parse_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let result = client(2000)
            .request_move(&server.uri(), &body_for_move(), "s1", Some(Direction::Down))
            .await;

        assert_eq!(result.direction, Direction::Down, "fallback to last move");
        assert!(result.errored);
        assert!(!result.timed_out);
        assert_eq!(result.status_code, Some(200));
    }

    #[tokio::test]
    async fn transport_failure_falls_back_timed_out() {
        // Bind then drop a listener: the port answers nothing (wiremock pools
        // servers, so a dropped MockServer would keep answering 404).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let result = client(500)
            .request_move(&url, &body_for_move(), "s1", Some(Direction::Up))
            .await;
        assert!(result.timed_out);
        assert!(!result.errored);
        assert_eq!(result.direction, Direction::Up);
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
        assert_eq!(info.color.as_deref(), Some("#888800"));
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
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client(100).request_move(&url, &body_for_move(), "s1", None),
        )
        .await
        .expect("stalled body does not hang (10x margin over the 100ms budget)");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(result.timed_out);
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
        let start = client.start(&url, &body_for_move()).await;
        assert!(start.is_some(), "start failure text returned");
        let end = client.end(&url, &body_for_move()).await;
        assert!(end.is_some(), "end failure text returned");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        assert!(
            client
                .start(&server.uri(), &body_for_move())
                .await
                .is_none()
        );
        assert!(client.end(&server.uri(), &body_for_move()).await.is_none());
    }
}
