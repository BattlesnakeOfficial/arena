//! Integration tests for `battlesnake play --browser` (DEV-2146) through
//! the real binary: the board server's wire API on 127.0.0.1, the exit
//! bounds, the flag combinations, and the offline guarantee.

use futures::StreamExt as _;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_tungstenite::tungstenite::Message as Tm;

const BIN: &str = env!("CARGO_BIN_EXE_battlesnake");

// --- MockSnake (copied from play_cli.rs, as merged by DEV-2164/#313) ---

/// A minimal HTTP/1.1 mock snake on a dedicated std thread. Answers:
///   GET /                       -> metadata JSON
///   POST <anything>/start|/end  -> {}
///   POST <anything>/move        -> {"move": CYCLE[turn%4], "shout": "hi"}
struct MockSnake {
    port: u16,
    #[allow(dead_code)]
    thread: std::thread::JoinHandle<u16>,
}

impl MockSnake {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock snake");
        listener.set_nonblocking(false).expect("blocking listener");
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Ok(request) = read_request(&mut stream) else {
                    continue;
                };
                let response = respond(&request);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                // Connection: close — one request per connection.
            }
            port
        });
        Self { port, thread }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        // Headers complete + body complete (Content-Length satisfied).
        if let Some(header_end) = text.find("\r\n\r\n") {
            let content_length = text
                .lines()
                .find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + content_length {
                return Ok(text.into_owned());
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

const CYCLE: [&str; 4] = ["up", "right", "down", "left"];

/// "POST /move HTTP/1.1" -> ("POST", "/move").
fn request_method_and_path(request: &str) -> (&str, &str) {
    let mut words = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    (words.next().unwrap_or(""), words.next().unwrap_or(""))
}

fn respond(request: &str) -> String {
    let (method, path) = request_method_and_path(request);
    let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let body = &request[body_start..];

    let (status, content_type, body_out) = if method == "GET" {
        (
            "200 OK",
            "application/json",
            r##"{"apiversion":"1","color":"#888800","head":"safe","tail":"bolt"}"##.to_string(),
        )
    } else if path.ends_with("/move") {
        // Key the move off the request's turn so interleaving cannot change
        // outcomes.
        let turn = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["turn"].as_i64())
            .unwrap_or(0);
        (
            "200 OK",
            "application/json",
            format!(
                r#"{{"move": "{}", "shout": "hi"}}"#,
                CYCLE[turn as usize % 4]
            ),
        )
    } else {
        ("200 OK", "application/json", "{}".to_string())
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_out}",
        body_out.len()
    )
}

// --- Process running ---

/// A `play` run with streaming stderr: lines arrive on a channel while the
/// process runs (to observe the board URL mid-game and timestamp the
/// completion line), stdout is collected on a thread (never fills the
/// pipe), and exit is polled non-blockingly.
struct StreamingRun {
    child: std::process::Child,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: Arc<std::sync::Mutex<Vec<u8>>>,
    /// Set by the reader thread when the completion line appears — the
    /// timestamp the 30s exit bound is measured from.
    completed_at: Arc<std::sync::Mutex<Option<Instant>>>,
    /// Every stderr line, in order (the launch lines must survive past a
    /// long game's turn lines for the later assertions).
    stderr_tail: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl StreamingRun {
    fn spawn(args: &[&str], envs: &[(&str, String)]) -> Self {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Never inherit an interactive shell's proxy env (this VM runs a
        // secret-broker MITM); tests that need a witness set their own.
        for var in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            cmd.env_remove(var);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn battlesnake binary");

        let stderr = child.stderr.take().expect("stderr piped");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let completed_at = Arc::new(std::sync::Mutex::new(None));
        let stderr_tail: Arc<std::sync::Mutex<std::collections::VecDeque<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        {
            let completed_at = completed_at.clone();
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if line.starts_with("Game completed after ") {
                        let mut stamp = completed_at.lock().unwrap();
                        if stamp.is_none() {
                            *stamp = Some(Instant::now());
                        }
                    }
                    tail.lock().unwrap().push_back(line.clone());
                    let _ = tx.send(line);
                }
                drop(tx);
            });
        }

        let stdout = child.stdout.take().expect("stdout piped");
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let collected = collected.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut reader = stdout;
                let _ = reader.read_to_end(&mut buf);
                *collected.lock().unwrap() = buf;
            });
        }

        Self {
            child,
            lines: rx,
            stdout: collected,
            completed_at,
            stderr_tail,
        }
    }

    async fn next_line(&mut self) -> Option<String> {
        self.lines.recv().await
    }

    /// Wait for the next line starting with `prefix` (60s bound).
    async fn wait_for_line(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        while let Some(line) = self.next_line().await {
            if line.starts_with(prefix) {
                return line;
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let tail = self.stderr_tail.lock().unwrap().clone();
        panic!(
            "no line starting with {prefix:?}; stderr tail:\n{}",
            tail.iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Non-blocking exit poll (the runtime must stay free for ws clients).
    async fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.child.try_wait().expect("child alive") {
                return status;
            }
            assert!(Instant::now() < deadline, "binary did not exit within 90s");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn completed_at(&self) -> Instant {
        self.completed_at.lock().unwrap().unwrap_or_else(|| {
            panic!(
                "no completion line seen; stderr tail:\n{}",
                self.stderr_tail
                    .lock()
                    .unwrap()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
    }

    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout.lock().unwrap()).into_owned()
    }
}

struct RunOutput {
    status: i32,
    #[allow(dead_code)] // C4 asserts on stderr; stdout is collected for
    // completeness of the failure dump
    stdout: String,
    stderr: String,
}

fn run_play(args: &[&str], envs: &[(&str, String)]) -> RunOutput {
    let mut cmd = Command::new(BIN);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for var in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
        cmd.env_remove(var);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("run battlesnake binary");
    RunOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn dont_open_browser() -> (&'static str, String) {
    ("BATTLESNAKE_DONT_OPEN_BROWSER", "1".to_string())
}

// --- Board-server client helpers ---

/// The engine URL, game id, and the raw `Opening board URL:` line,
/// parsed from the two launch log lines.
async fn wait_for_board(run: &mut StreamingRun) -> (String, String, String) {
    let listening = run
        .wait_for_line("Board server listening on http://127.0.0.1:")
        .await;
    let engine_url = listening
        .strip_prefix("Board server listening on ")
        .expect("prefix")
        .to_string();
    let url_line = run.wait_for_line("Opening board URL: ").await;
    let url = url_line
        .strip_prefix("Opening board URL: ")
        .expect("prefix")
        .to_string();
    let game_id = url
        .split("&game=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("game id in URL")
        .to_string();
    (engine_url, game_id, url)
}

/// Connect to the events websocket and read until the server closes:
/// returns the data messages and the close code.
async fn read_board_stream(
    engine_url: &str,
    game_id: &str,
) -> (Vec<serde_json::Value>, Option<u16>) {
    let url = format!(
        "{}/games/{}/events",
        engine_url.replacen("http", "ws", 1),
        game_id
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    let mut messages = Vec::new();
    let mut close_code = None;
    while let Some(message) = tokio::time::timeout(Duration::from_secs(60), ws.next())
        .await
        .expect("message within 60s")
    {
        match message.expect("ws ok") {
            Tm::Text(text) => {
                messages.push(serde_json::from_str(&text).expect("board JSON"));
            }
            Tm::Close(frame) => {
                close_code = frame.map(|f| u16::from(f.code));
                break;
            }
            _ => {}
        }
    }
    (messages, close_code)
}

/// Frames' turns must cover 0..=last exactly once, in order; the final
/// data message is game_end.
fn assert_complete_frame_stream(messages: &[serde_json::Value]) {
    let turns: Vec<i64> = messages
        .iter()
        .filter(|m| m["Type"] == "frame")
        .map(|m| m["Data"]["Turn"].as_i64().expect("turn"))
        .collect();
    let last = turns.last().copied().unwrap_or(-1);
    assert!(last >= 0, "no frames: {messages:?}");
    let want: Vec<i64> = (0..=last).collect();
    assert_eq!(turns, want, "frames must cover every turn once, in order");
    assert_eq!(
        messages.last().expect("non-empty")["Type"],
        "game_end",
        "the final data message must be game_end"
    );
}

// --- C.1: every mode streams the whole game ---

#[tokio::test]
async fn browser_streams_whole_game_in_every_mode() {
    for (mode, snake_count) in [
        ("solo", 1),
        ("standard", 2),
        ("royale", 2),
        ("constrictor", 2),
        ("snail_mode", 2),
    ] {
        let snake = MockSnake::spawn();
        let mut args: Vec<String> = ["play", "-g", mode, "--browser", "-r", "42"]
            .into_iter()
            .map(String::from)
            .collect();
        let urls: Vec<String> = (0..snake_count).map(|_| snake.url()).collect();
        for url in &urls {
            args.push("--url".into());
            args.push(url.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut run = StreamingRun::spawn(&arg_refs, &[dont_open_browser()]);

        let (engine_url, game_id, url_line) = wait_for_board(&mut run).await;
        // Info endpoint: CORS + the game.
        let response = reqwest::get(format!("{engine_url}/games/{game_id}"))
            .await
            .expect("info get");
        assert_eq!(response.status(), 200, "{mode}");
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "*",
            "{mode}"
        );
        let info: serde_json::Value = response.json().await.unwrap();
        assert_eq!(info["Game"]["ID"], game_id, "{mode}");

        // Websocket: the whole game through to the end.
        let (messages, close_code) = read_board_stream(&engine_url, &game_id).await;
        assert_complete_frame_stream(&messages);
        assert_eq!(messages[0]["Type"], "frame", "{mode}");
        assert_eq!(messages[0]["Data"]["Turn"], 0, "{mode}: turn 0 first");
        assert_eq!(close_code, Some(1000), "{mode}: close code");

        let status = run.wait_exit().await;
        assert_eq!(status.code(), Some(0), "{mode}");

        // Solo also checks the exact URL built (byte-for-byte) and the
        // Local Network Access hint.
        assert_eq!(
            url_line,
            format!(
                "https://board.battlesnake.com?engine={engine_url}&game={game_id}&autoplay=true"
            ),
            "{mode}: the board URL is byte-exact (engine first, colon unencoded)"
        );
        let tail = run.stderr_tail.lock().unwrap().clone();
        let full: Vec<&str> = tail.iter().map(String::as_str).collect();
        assert!(
            full.iter().any(|l| l.contains("choose Allow")),
            "{mode}: Local Network Access hint missing"
        );
    }
}

// --- C.2: no browser -> exit 0 within the bound ---

#[tokio::test]
async fn no_browser_connect_exits_zero_within_bound() {
    let snake = MockSnake::spawn();
    let mut run = StreamingRun::spawn(
        &[
            "play",
            "--name",
            "A",
            "--url",
            &snake.url(),
            "-g",
            "solo",
            "--browser",
        ],
        &[dont_open_browser()],
    );
    run.wait_for_line("Game completed after ").await;
    let completed_at = run.completed_at();
    let status = run.wait_exit().await;
    assert_eq!(status.code(), Some(0));
    let elapsed = completed_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "exit took {elapsed:?} after game end (Go hangs forever here)"
    );
}

// --- C.3: a late client still gets the full replay ---

#[tokio::test]
async fn late_client_gets_full_replay_after_game_end() {
    let snake = MockSnake::spawn();
    let mut run = StreamingRun::spawn(
        &[
            "play",
            "--name",
            "A",
            "--url",
            &snake.url(),
            "-g",
            "solo",
            "-r",
            "7",
            "--browser",
        ],
        &[dont_open_browser()],
    );
    let (engine_url, game_id, _url_line) = wait_for_board(&mut run).await;
    run.wait_for_line("Game completed after ").await;

    // Connect AFTER game end, inside the grace window anchored there.
    let (messages, close_code) = read_board_stream(&engine_url, &game_id).await;
    assert_complete_frame_stream(&messages);
    assert_eq!(close_code, Some(1000));

    let status = run.wait_exit().await;
    assert_eq!(status.code(), Some(0));
}

// --- C.4: browser-open failure prints the URL and continues ---

/// macOS runs `/usr/bin/open` by absolute path (open 5.3.3 src/macos.rs),
/// so an empty PATH fails nothing there — and would open a real browser.
#[test]
#[cfg(target_os = "linux")]
fn browser_open_failure_prints_url_and_continues() {
    let snake = MockSnake::spawn();
    // No BATTLESNAKE_DONT_OPEN_BROWSER: the spawn runs and fails for real.
    let out = run_play(
        &[
            "play",
            "--name",
            "A",
            "--url",
            &snake.url(),
            "-g",
            "solo",
            "--browser",
        ],
        &[("PATH", String::new())],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("Failed to open browser:"),
        "open failure must be logged:\n{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("Opening board URL: https://board.battlesnake.com?engine="),
        "the URL must be on screen for hand-opening:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("Game completed after"),
        "{}",
        out.stderr
    );
}

// --- C.5: --browser combines with --output, --viewmap, --delay ---

#[tokio::test]
async fn browser_combines_with_output_viewmap_and_delay() {
    let snake = MockSnake::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = tmp.path().join("out.jsonl");
    let output_str = output.to_str().unwrap().to_string();
    let mut run = StreamingRun::spawn(
        &[
            "play",
            "--name",
            "A",
            "--url",
            &snake.url(),
            "-g",
            "solo",
            "-r",
            "7",
            "--browser",
            "-o",
            &output_str,
            "-v",
            "-d",
            "5",
        ],
        &[dont_open_browser()],
    );
    let (engine_url, game_id, _url_line) = wait_for_board(&mut run).await;
    let (messages, close_code) = read_board_stream(&engine_url, &game_id).await;
    assert_complete_frame_stream(&messages);
    assert_eq!(close_code, Some(1000));

    let status = run.wait_exit().await;
    assert_eq!(status.code(), Some(0));
    let stdout = run.stdout_text();
    assert!(stdout.contains("Turn: 0"), "viewmap on stdout:\n{stdout}");
    let lines = std::fs::read_to_string(&output).unwrap();
    let final_turn = messages
        .iter()
        .filter(|m| m["Type"] == "frame")
        .count()
        .saturating_sub(1);
    assert_eq!(
        lines.lines().count(),
        final_turn + 3,
        "game + (turn+1) frames + result"
    );
}

// --- C.6: the CLI never dials beyond the snake URL ---

/// A listener that records every connection and immediately closes it:
/// any request to it is proof of a stray dial.
struct RejectingWitness {
    port: u16,
    hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RejectingWitness {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind witness");
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(stream); // reject
            }
        });
        Self { port, hits }
    }
}

#[tokio::test]
async fn browser_makes_no_outbound_dials() {
    let snake = MockSnake::spawn();
    let witness = RejectingWitness::spawn();
    let proxy = format!("http://127.0.0.1:{}", witness.port);
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut run = StreamingRun::spawn(
        &[
            "play",
            "--name",
            "A",
            "--url",
            &snake.url(),
            "-g",
            "solo",
            "--browser",
        ],
        &[
            dont_open_browser(),
            ("HOME", tmp.path().to_str().unwrap().to_string()),
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy.clone()),
            ("NO_PROXY", "127.0.0.1,localhost".to_string()),
        ],
    );
    // A ws client so the run drains and exits promptly instead of waiting
    // out the connect grace.
    let (engine_url, game_id, _url_line) = wait_for_board(&mut run).await;
    let (messages, close_code) = read_board_stream(&engine_url, &game_id).await;
    assert_complete_frame_stream(&messages);
    assert_eq!(close_code, Some(1000));
    let status = run.wait_exit().await;
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        witness.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "play dialed the network beyond the snake URL (the CLI serves the \
         board; only a real browser would dial board.battlesnake.com)"
    );
}

// --- C.7: the close handshake completes, repeatedly ---

/// Five runs: every run's ws client observes close code 1000 AFTER
/// receiving game_end as its final data message, and the process exits 0
/// (repetition exposes the set-done-before-close race).
#[tokio::test]
async fn close_handshake_completes_from_the_real_binary() {
    for attempt in 1..=5 {
        let snake = MockSnake::spawn();
        let mut run = StreamingRun::spawn(
            &[
                "play",
                "--name",
                "A",
                "--url",
                &snake.url(),
                "-g",
                "solo",
                "--browser",
            ],
            &[dont_open_browser()],
        );
        let (engine_url, game_id, _url_line) = wait_for_board(&mut run).await;
        let (messages, close_code) = read_board_stream(&engine_url, &game_id).await;
        assert_complete_frame_stream(&messages);
        assert_eq!(
            close_code,
            Some(1000),
            "attempt {attempt}: a frame-absent close would read 1005"
        );
        let status = run.wait_exit().await;
        assert_eq!(status.code(), Some(0), "attempt {attempt}");
    }
}
