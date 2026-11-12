//! Integration tests for `battlesnake play` through the real binary:
//! offline/no-account proof, README command, --output, --viewmap,
//! completion, and failure rendering.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_battlesnake");

#[path = "support/survivor_policy.rs"]
mod survivor_policy;

/// A minimal HTTP/1.1 mock snake on a dedicated std thread (never
/// tokio::spawn from a test — each #[tokio::test] owns and drops its
/// runtime, killing the task). Answers:
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

struct RunOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run_play(args: &[&str], envs: &[(&str, &str)]) -> RunOutput {
    let mut cmd = Command::new(BIN);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
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

// --- C.1: no account, no network ---

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

/// `play` runs a full game with HOME pointing at an empty directory, a
/// proxy env var routing everything else at a rejecting witness, and the
/// arena API URL unreachable — proving no config read, no token, no dial.
#[test]
fn play_runs_offline_with_no_account_or_network() {
    let snake = MockSnake::spawn();
    let witness = RejectingWitness::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");

    let out = run_play(
        &["play", "--name", "A", "--url", &snake.url(), "-g", "solo"],
        &[
            ("HOME", tmp.path().to_str().unwrap()),
            (
                "XDG_CONFIG_HOME",
                tmp.path().join(".config").to_str().unwrap(),
            ),
            ("USERPROFILE", tmp.path().to_str().unwrap()),
            ("APPDATA", tmp.path().to_str().unwrap()),
            ("HTTPS_PROXY", &format!("http://127.0.0.1:{}", witness.port)),
            ("HTTP_PROXY", &format!("http://127.0.0.1:{}", witness.port)),
            ("NO_PROXY", "127.0.0.1,localhost"),
        ],
    );

    assert_eq!(out.status, 0, "exit 0\nstderr:\n{}", out.stderr);
    assert!(out.stdout.is_empty(), "logs to stderr only: {}", out.stdout);
    assert!(out.stderr.contains("Snake ID:"), "{}", out.stderr);
    assert!(
        out.stderr.contains("Ruleset: solo, Seed:"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("Turn:"), "{}", out.stderr);
    assert!(
        out.stderr.contains("Game completed after"),
        "no completion line:\n{}",
        out.stderr
    );
    assert_eq!(
        witness.hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "play dialed the network beyond the snake URL"
    );
    // No config file created under the isolated home.
    assert!(
        !tmp.path().join(".config/arena/config.toml").exists(),
        "play must never write a config file"
    );
}

/// Positive control (Linux-only: `dirs::config_dir()` reads XDG_CONFIG_HOME
/// only on Linux): a `whoami` with a token under the isolated config dir
/// MUST hit the witness — proving the proxy setup catches real dials and
/// the zero-connection assertion above is meaningful.
#[test]
#[cfg(target_os = "linux")]
fn offline_harness_catches_real_dials() {
    let witness = RejectingWitness::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_dir = tmp.path().join(".config/arena");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[auth]\ntoken = \"fake-token-for-the-positive-control\"\n",
    )
    .unwrap();

    let out = run_play(
        &["whoami"],
        &[
            ("HOME", tmp.path().to_str().unwrap()),
            (
                "XDG_CONFIG_HOME",
                tmp.path().join(".config").to_str().unwrap(),
            ),
            ("HTTPS_PROXY", &format!("http://127.0.0.1:{}", witness.port)),
            ("HTTP_PROXY", &format!("http://127.0.0.1:{}", witness.port)),
            ("NO_PROXY", "127.0.0.1,localhost"),
        ],
    );
    assert_ne!(out.status, 0, "whoami must fail (proxy rejects)");
    assert!(
        witness.hits.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "whoami never dialed — the offline harness would be proving nothing"
    );
}

/// Linux-only: unparseable TOML at the real config path must not break
/// `play` — it never calls CliConfig::load().
#[test]
#[cfg(target_os = "linux")]
fn play_never_loads_the_config_file() {
    let snake = MockSnake::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_dir = tmp.path().join(".config/arena");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "this is {{{ not toml").unwrap();

    let out = run_play(
        &["play", "--name", "A", "--url", &snake.url(), "-g", "solo"],
        &[
            ("HOME", tmp.path().to_str().unwrap()),
            (
                "XDG_CONFIG_HOME",
                tmp.path().join(".config").to_str().unwrap(),
            ),
        ],
    );
    assert_eq!(
        out.status, 0,
        "play ignores the config file:\n{}",
        out.stderr
    );
}

// --- C.2: starter README command + defaults ---

#[test]
fn starter_readme_command_parses_and_runs() {
    let snake = MockSnake::spawn();
    let out = run_play(
        &[
            "play",
            "-W",
            "11",
            "-H",
            "11",
            "--name",
            "Rust Starter Project",
            "--url",
            &snake.url(),
            "-g",
            "solo",
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("Name: \"Rust Starter Project\""),
        "{}",
        out.stderr
    );
}

#[test]
fn bare_play_defaults_to_11x11_and_timeout_500() {
    let snake = MockSnake::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = tmp.path().join("game.jsonl");
    let out = run_play(
        &[
            "play",
            "-n",
            "A",
            "-u",
            &snake.url(),
            "-o",
            output.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    let first_line = std::fs::read_to_string(&output)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let game: serde_json::Value = serde_json::from_str(&first_line).unwrap();
    assert_eq!(game["timeout"], 500);
    let frame: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&output)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(frame["board"]["width"], 11);
    assert_eq!(frame["board"]["height"], 11);
}

// --- C.3: --output ---

#[test]
fn output_file_line_count_and_truncation() {
    let snake = MockSnake::spawn();
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = tmp.path().join("game.jsonl");

    let out = run_play(
        &[
            "play",
            "-n",
            "A",
            "-u",
            &snake.url(),
            "-g",
            "solo",
            "-r",
            "7",
            "-o",
            output.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    let stderr = out.stderr;
    let final_turn: usize = stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("Game completed after ")?
                .split(" turns")
                .next()
        })
        .and_then(|n| n.parse().ok())
        .expect("completion line with a turn count");
    let lines = std::fs::read_to_string(&output).unwrap();
    assert_eq!(lines.lines().count(), final_turn + 3);

    // Re-run with a different seed: the file is replaced, never appended.
    let out = run_play(
        &[
            "play",
            "-n",
            "A",
            "-u",
            &snake.url(),
            "-g",
            "solo",
            "-r",
            "999",
            "-o",
            output.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    let lines2 = std::fs::read_to_string(&output).unwrap();
    let final_turn2: usize = out
        .stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("Game completed after ")?
                .split(" turns")
                .next()
        })
        .and_then(|n| n.parse().ok())
        .expect("second completion line");
    assert_eq!(lines2.lines().count(), final_turn2 + 3);
    assert_ne!(lines, lines2);
}

// --- C.4: --viewmap ---

#[test]
fn viewmap_goes_to_stdout() {
    let snake = MockSnake::spawn();
    let out = run_play(
        &["play", "-n", "A", "-u", &snake.url(), "-g", "solo", "-v"],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(out.stdout.contains("◦"), "board glyphs on stdout");
    assert!(out.stdout.contains("Turn: 0"), "{}", out.stdout);

    let out = run_play(&["play", "-n", "A", "-u", &snake.url(), "-g", "solo"], &[]);
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(out.stdout.is_empty(), "without -v nothing on stdout");
}

// --- C.5: completion ---

#[test]
fn completion_scripts_mention_play() {
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = run_play(&["completion", shell], &[]);
        assert_eq!(out.status, 0, "{shell}");
        assert!(!out.stdout.is_empty(), "{shell} completion empty");
        assert!(out.stdout.contains("play"), "{shell} completion lacks play");
    }
}

// --- C.6: failure rendering ---

#[test]
fn unavailable_gametype_exits_1_with_plain_message() {
    let out = run_play(&["play", "-g", "wrapped", "-u", "http://127.0.0.1:1"], &[]);
    assert_eq!(out.status, 1);
    assert!(
        out.stderr
            .contains("game type 'wrapped' isn't available yet"),
        "{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("Location:"),
        "no color-eyre report, just the message: {}",
        out.stderr
    );
}

#[test]
fn negative_timeout_exits_1_not_2() {
    let out = run_play(&["play", "-t", "-5", "-u", "http://127.0.0.1:1"], &[]);
    assert_eq!(
        out.status, 1,
        "validation error, not clap usage: {}",
        out.stderr
    );
    assert!(out.stderr.contains("timeout"), "{}", out.stderr);
}

#[test]
fn max_turns_below_one_exits_1_not_2() {
    let out = run_play(
        &["play", "--max-turns", "0", "-u", "http://127.0.0.1:1"],
        &[],
    );
    assert_eq!(out.status, 1, "{}", out.stderr);
    assert!(
        out.stderr.contains("--max-turns must be at least 1"),
        "{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("Location:"),
        "no color-eyre report, just the message: {}",
        out.stderr
    );
    // Negatives take the same path (allow_negative_numbers), not clap's exit 2.
    let out = run_play(
        &["play", "--max-turns", "-3", "-u", "http://127.0.0.1:1"],
        &[],
    );
    assert_eq!(out.status, 1, "{}", out.stderr);
}

// --- C.7: turn limit (DEV-2164) ---

/// --max-turns N through the real binary: the game stops at turn N with
/// today's log line, as the final log line.
#[test]
fn max_turns_flag_stops_the_game_at_n() {
    let snake = MockSnake::spawn();
    let out = run_play(
        &[
            "play",
            "-g",
            "solo",
            "--max-turns",
            "2",
            "-n",
            "A",
            "-u",
            &snake.url(),
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    let last = out.stderr.lines().last();
    assert_eq!(
        last,
        Some("Game stopped at the turn limit of 2 turns."),
        "the turn-limit line is the final log line; stderr:\n{}",
        out.stderr
    );
}

/// A snake that plays the verified survivor policy (see
/// `support/survivor_policy.rs`): MockSnake's exact transport shape, with
/// `/move` calling `survivor_move`.
struct SurvivorSnake {
    port: u16,
    #[allow(dead_code)]
    thread: std::thread::JoinHandle<u16>,
}

impl SurvivorSnake {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind survivor snake");
        listener.set_nonblocking(false).expect("blocking listener");
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Ok(request) = read_request(&mut stream) else {
                    continue;
                };
                let response = survivor_respond(&request);
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

fn survivor_respond(request: &str) -> String {
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
        (
            "200 OK",
            "application/json",
            format!(
                r#"{{"move": "{}"}}"#,
                survivor_policy::survivor_move(body, 5_000)
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

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// No --max-turns flag: the game must run past turn 5,000 and end by game
/// over (the survivor suicides after turn 5,000), never at a turn limit.
/// Deterministic: fixed seed + deterministic snake => identical every run
/// (the no-fallback assertion below is what makes that claim airtight).
///
/// Seed 1 is pinned by the plan's engine-level sweep (30/30 seeds
/// survived; seed 1 ends at exactly turn 5,001). If this ever fails with a
/// different final turn or an early death, an engine/rules change moved
/// the RNG path: re-run the ignored `survivor_seed_sweep` and pin the
/// first survivor — do NOT rewrite the policy.
#[test]
fn no_default_turn_limit_game_runs_past_5000_turns() {
    let snake = SurvivorSnake::spawn();
    let out = run_play(
        &[
            "play",
            "-g",
            "solo",
            "-W",
            "19",
            "-H",
            "19",
            "-n",
            "Survivor",
            "-u",
            &snake.url(),
            "-r",
            "1",
            "--foodSpawnChance",
            "0",
            "--minimumFood",
            "15",
            "-t",
            "60000",
        ],
        &[],
    );
    assert_eq!(out.status, 0, "{}", out.stderr);
    let final_turn: i64 = out
        .stderr
        .lines()
        .find_map(|l| {
            l.strip_prefix("Game completed after ")?
                .split(" turns")
                .next()
        })
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "no completion line; stderr tail:\n{}",
                tail_lines(&out.stderr, 20)
            )
        });
    assert!(
        final_turn > 5_000,
        "game stopped at {final_turn}, not past 5,000; stderr tail:\n{}",
        tail_lines(&out.stderr, 20)
    );
    assert!(
        !out.stderr.contains("Game stopped at the turn limit"),
        "no flag was passed, but a turn limit fired; stderr tail:\n{}",
        tail_lines(&out.stderr, 20)
    );
    assert_no_fallback_warnings(&out.stderr);
}

/// Every one of these line prefixes is a move-path warning that replaces
/// the snake's move with a fallback (repeat-last-direction or error
/// result) and silently changes the game (cli/src/play/http.rs: "Request
/// to {move_url} failed" covers both timeouts and transport errors —
/// http.rs:141/149 — plus "Failed to read response body from" :154,
/// "Got non-ok status code from" :176, "Failed to parse JSON data from" :192,
/// "Failed to decode JSON from" :209).
/// One fallback move breaks determinism, so zero of them is required, not
/// just zero timeouts: an early `Game completed after` with exit 0 can be
/// a fallback artifact that merely looks like a policy failure.
fn assert_no_fallback_warnings(stderr: &str) {
    for line in stderr.lines() {
        for prefix in [
            "Request to",
            "Failed to read",
            "Got non-ok",
            "Failed to parse",
            "Failed to decode",
        ] {
            assert!(
                !line.starts_with(prefix),
                "a fallback warning changed the game: {line}; stderr tail:\n{}",
                tail_lines(stderr, 20)
            );
        }
    }
}

/// Re-pinning tool, not part of CI: runs the pinned-policy game for seeds
/// 1..=30 (~7 min) and prints each final turn. If an engine change kills
/// the pinned seed, pin the first survivor from this sweep's output.
#[test]
#[ignore = "sweep tool: ~7 min; re-pins the survivor seed after engine changes"]
fn survivor_seed_sweep() {
    for seed in 1..=30 {
        let snake = SurvivorSnake::spawn();
        let out = run_play(
            &[
                "play",
                "-g",
                "solo",
                "-W",
                "19",
                "-H",
                "19",
                "-n",
                "Survivor",
                "-u",
                &snake.url(),
                "-r",
                &seed.to_string(),
                "--foodSpawnChance",
                "0",
                "--minimumFood",
                "15",
                "-t",
                "60000",
            ],
            &[],
        );
        let final_turn: i64 = out
            .stderr
            .lines()
            .find_map(|l| {
                l.strip_prefix("Game completed after ")?
                    .split(" turns")
                    .next()
            })
            .and_then(|n| n.parse().ok())
            .unwrap_or(-1);
        eprintln!("seed {seed}: final turn {final_turn}");
    }
}
