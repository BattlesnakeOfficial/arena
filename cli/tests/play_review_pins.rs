//! Pinned PR review findings for `battlesnake play` (DEV-2144, round 1).
//!
//! Each test states a contract from the task/plan or from the Go CLI at
//! `BattlesnakeOfficial/rules@87e094e` that `play` currently breaks.

use std::io::{Read as _, Write as _};

use battlesnake_cli::play::http::SnakeHttpClient;
use battlesnake_cli::play::{PlayArgs, run_game, validate};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn solo_args(url: String) -> PlayArgs {
    PlayArgs {
        width: 11,
        height: 11,
        names: vec!["A".into()],
        urls: vec![url],
        // Far above any local connect time, so only a real timeout can say
        // "timed out" (a refused connect is slow on Windows).
        timeout: 5000,
        sequential: false,
        gametype: "solo".into(),
        map: None,
        view_map: false,
        color: false,
        seed: Some(42),
        delay_ms: 0,
        duration_ms: 0,
        output: None,
        food_spawn_chance: 15,
        minimum_food: 1,
        hazard_damage_per_turn: 14,
        shrink_every_n_turns: 25,
    }
}

async fn snake_with_move(move_response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    for endpoint in ["/start", "/end"] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/move"))
        .respond_with(move_response)
        .mount(&server)
        .await;
    server
}

/// Returns (log stream, board stream).
async fn play(args: &PlayArgs) -> (String, String) {
    let cfg = validate(args, false).expect("valid config");
    let client = SnakeHttpClient::new(cfg.timeout_ms, false);
    let mut log = Vec::new();
    let mut board = Vec::new();
    run_game(&cfg, &client, None, &mut log, &mut board)
        .await
        .expect("game runs");
    (
        String::from_utf8(log).expect("utf-8 log"),
        String::from_utf8(board).expect("utf-8 board"),
    )
}

/// Go's `printMap` ends with `fmt.Println(o.String())` on a buffer that already
/// ends in `\n` (play.go:729), so every frame is followed by a blank line. The
/// plan's board_view spec names it: "Trailing blank line". The checked-in
/// viewmap fixtures end with it too; the golden tests only match them because
/// they compare `rendered + "\n"`, a newline production never writes.
#[tokio::test]
async fn viewmap_frames_end_with_gos_blank_line() {
    let server =
        snake_with_move(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#)).await;
    let args = PlayArgs {
        view_map: true,
        ..solo_args(server.uri())
    };

    let (_log, board) = play(&args).await;

    assert!(
        board.contains("\n\nTurn: 1\n"),
        "frame 0 must be followed by a blank line before frame 1, got:\n{board}"
    );
    assert!(
        board.ends_with("\n\n"),
        "the last frame must end with Go's blank line too, got:\n{board}"
    );
}

/// Go's warning names the endpoint URL and quotes the body:
/// `Got non-ok status code from <url>/move`, `StatusCode: 500 (expected 200)`,
/// `Body: "..."` (play.go `getSnakeUpdate`). The plan asks for "Go's WARN
/// texts". The body is what tells a snake developer why the move failed.
#[tokio::test]
async fn non_ok_move_warning_names_the_url_and_the_body() {
    let server =
        snake_with_move(ResponseTemplate::new(500).set_body_string("boom: index out of range"))
            .await;

    let (log, _board) = play(&solo_args(server.uri())).await;

    let move_url = format!("{}/move", server.uri());
    assert!(
        log.contains(&format!("Got non-ok status code from {move_url}")),
        "warning must name the move URL, got:\n{log}"
    );
    assert!(
        log.contains("boom: index out of range"),
        "warning must include the response body, got:\n{log}"
    );
}

/// A snake that went down mid-game refuses the connection immediately. Go logs
/// `Request to <url>/move failed` plus the real error. Saying "request timed
/// out" sends the developer hunting for a slow snake instead of a dead one.
#[tokio::test]
async fn refused_move_is_not_reported_as_a_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    // Answer `GET /` once, then close the port for the rest of the game.
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    });

    let (log, _board) = play(&solo_args(url.clone())).await;

    assert!(
        log.contains(&format!("Request to {url}/move failed")),
        "warning must name the move URL, got:\n{log}"
    );
    assert!(
        !log.contains("timed out"),
        "a refused connection is not a timeout, got:\n{log}"
    );
}
