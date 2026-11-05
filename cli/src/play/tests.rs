//! Unit tests for `play`: flag surface, validation, wire pairs, request
//! bodies (golden vs Go fixtures), determinism, turn limit, and
//! winner/draw lines.

use std::collections::HashMap;

use color_eyre::eyre::eyre;
use rules::{EliminationCause, Point, RoyaleSettings, StandardSettings};

use engine_core::wire::{self, SnakeContext, SnakeCustomizations};
use engine_core::{EngineGame, GameMeta, GameMode, GameSource};

use super::mode::ResolvedGame;
use super::{PlayArgs, PlayConfig, display_fields, request_body, run_game, validate};

/// Shared by this module's tests and `board_server`'s (sibling module
/// under `play`): the exhaustive `PlayArgs` literal.
pub(super) fn base_args() -> PlayArgs {
    PlayArgs {
        width: 11,
        height: 11,
        names: vec!["A".into()],
        urls: vec!["http://127.0.0.1:1".into()],
        timeout: 500,
        sequential: false,
        gametype: "standard".into(),
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
        max_turns: None,
        browser: false,
        board_url: "https://board.battlesnake.com".into(),
    }
}

// ---------------------------------------------------------------------------
// Flag surface (the guard against clap kebab-case drift)
// ---------------------------------------------------------------------------

#[derive(clap::Parser)]
struct PlayOnly {
    #[command(flatten)]
    args: PlayArgs,
}

fn parse_flags(argv: &[&str]) -> color_eyre::Result<PlayArgs> {
    use clap::Parser as _;
    let mut full = vec!["play"];
    full.extend_from_slice(argv);
    PlayOnly::try_parse_from(&full)
        .map(|p| p.args)
        .map_err(|e| eyre!("{e}"))
}

#[test]
fn every_flag_parses_with_its_exact_name_and_defaults() {
    let args = parse_flags(&[
        "-W",
        "7",
        "-H",
        "7",
        "--name",
        "N1",
        "--name",
        "N2",
        "--url",
        "http://a",
        "--url",
        "http://b",
        "--timeout",
        "500",
        "--sequential",
        "--gametype",
        "royale",
        "--map",
        "royale",
        "--viewmap",
        "--color",
        "--seed",
        "7",
        "--delay",
        "1",
        "--duration",
        "2",
        "--output",
        "/tmp/out.jsonl",
        "--foodSpawnChance",
        "10",
        "--minimumFood",
        "3",
        "--hazardDamagePerTurn",
        "12",
        "--shrinkEveryNTurns",
        "5",
        "--max-turns",
        "9",
        "--browser",
        "--board-url",
        "https://example.test/board",
    ])
    .expect("every flag parses with its exact long name");
    assert_eq!((args.width, args.height), (7, 7));
    assert_eq!(args.names, ["N1", "N2"]);
    assert_eq!(args.urls, ["http://a", "http://b"]);
    assert!(args.sequential);
    assert_eq!(args.gametype, "royale");
    assert_eq!(args.map.as_deref(), Some("royale"));
    assert!(args.view_map);
    assert!(args.color);
    assert_eq!(args.seed, Some(7));
    assert_eq!(args.delay_ms, 1);
    assert_eq!(args.duration_ms, 2);
    assert_eq!(args.output, Some("/tmp/out.jsonl".into()));
    assert_eq!(
        (
            args.food_spawn_chance,
            args.minimum_food,
            args.hazard_damage_per_turn,
            args.shrink_every_n_turns
        ),
        (10, 3, 12, 5)
    );
    assert_eq!(args.max_turns, Some(9));
    assert!(args.browser);
    assert_eq!(args.board_url, "https://example.test/board");

    // Defaults match Go's.
    let defaults = parse_flags(&["-u", "http://a"]).expect("parses");
    assert_eq!((defaults.width, defaults.height), (11, 11));
    assert_eq!(defaults.timeout, 500);
    assert_eq!(defaults.gametype, "standard");
    assert_eq!(defaults.map, None);
    assert_eq!(
        (
            defaults.food_spawn_chance,
            defaults.minimum_food,
            defaults.hazard_damage_per_turn,
            defaults.shrink_every_n_turns
        ),
        (15, 1, 14, 25)
    );
    assert_eq!(defaults.delay_ms, 0);
    assert_eq!(defaults.duration_ms, 0);
    assert_eq!(defaults.seed, None);
    assert_eq!(defaults.max_turns, None);
    assert!(!defaults.browser, "--browser defaults off");
    assert_eq!(defaults.board_url, "https://board.battlesnake.com");
}

/// Any flag clap would kebab-case (`--view-map`, `--view-map`, `--delay-ms`,
/// `--food-spawn-chance`, …) must NOT parse — the Go long names are
/// byte-for-byte.
#[test]
fn kebab_case_drift_does_not_parse() {
    for bad in [
        vec!["--view-map"].as_slice(),
        vec!["--delay-ms", "1"].as_slice(),
        vec!["--duration-ms", "1"].as_slice(),
        vec!["--food-spawn-chance", "1"].as_slice(),
        vec!["--minimum-food", "1"].as_slice(),
        vec!["--hazard-damage-per-turn", "1"].as_slice(),
        vec!["--shrink-every-n-turns", "1"].as_slice(),
        vec!["--game-type", "solo"].as_slice(),
    ] {
        let mut argv = vec!["-u", "http://a"];
        argv.extend_from_slice(bad);
        assert!(
            parse_flags(&argv).is_err(),
            "{bad:?} must not parse (Go long names are byte-for-byte)"
        );
    }
}

#[test]
fn short_flags_parse() {
    let args = parse_flags(&[
        "-W", "11", "-H", "11", "-n", "N", "-u", "http://a", "-t", "500", "-g", "solo", "-m",
        "standard", "-v", "-c", "-r", "5", "-d", "0", "-D", "0",
    ])
    .expect("short flags parse");
    assert_eq!(args.names, ["N"]);
    assert!(args.view_map);
    assert!(args.color);
    assert_eq!(args.seed, Some(5));
}

/// Go's -r accepts negatives (int64 flag); clap needs the opt-in.
#[test]
fn negative_seed_parses() {
    let args = parse_flags(&["-u", "http://a", "-r", "-5"]).expect("negative seed parses");
    assert_eq!(args.seed, Some(-5));
}

#[test]
fn negative_max_turns_parses() {
    let args =
        parse_flags(&["-u", "http://a", "--max-turns", "-5"]).expect("negative max-turns parses");
    assert_eq!(args.max_turns, Some(-5));
}

#[test]
fn timeout_zero_normalizes_to_500() {
    let args = parse_flags(&["-u", "http://a", "-t", "0"]).expect("parses");
    let cfg = validate(&args, false).expect("valid");
    assert_eq!(cfg.timeout_ms, 500);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn negative_timeout_is_rejected() {
    let args = PlayArgs {
        timeout: -5,
        ..base_args()
    };
    let err = validate(&args, false).unwrap_err().to_string();
    assert_eq!(err, "timeout must not be negative");
}

/// The default seed resolves to a value (nanosecond clock; effectively never
/// zero, and zero would still be a usable seed — this pins that resolution
/// never panics).
#[test]
fn default_seed_resolves() {
    let mut args = base_args();
    args.seed = None;
    let cfg = validate(&args, false).expect("valid");
    let _ = cfg.seed;
}

#[test]
fn max_turns_below_one_is_rejected() {
    for bad in [0, -3] {
        let args = PlayArgs {
            max_turns: Some(bad),
            ..base_args()
        };
        let err = validate(&args, false).unwrap_err().to_string();
        assert_eq!(err, format!("--max-turns must be at least 1 (got {bad})"));
    }
}

#[test]
fn max_turns_defaults_to_none_and_passes_through() {
    let cfg = validate(&base_args(), false).expect("valid");
    assert_eq!(cfg.max_turns, None);
    let args = PlayArgs {
        max_turns: Some(7),
        ..base_args()
    };
    let cfg = validate(&args, false).expect("valid");
    assert_eq!(cfg.max_turns, Some(7));
}

#[test]
fn royale_with_zero_shrink_is_rejected_up_front() {
    let args = PlayArgs {
        gametype: "royale".into(),
        shrink_every_n_turns: 0,
        ..base_args()
    };
    let err = validate(&args, false).unwrap_err().to_string();
    assert!(err.contains("shrinkEveryNTurns"), "{err}");
    // Non-royale modes accept 0 (the value only feeds the wire block).
    let args = PlayArgs {
        gametype: "standard".into(),
        shrink_every_n_turns: 0,
        ..base_args()
    };
    assert!(validate(&args, false).is_ok());
}

// ---------------------------------------------------------------------------
// Golden request bodies vs Go's fixtures (rebuilt as engine state)
// ---------------------------------------------------------------------------

/// The hand-built board from Go's `TestGetIndividualBoardStateForSnake` /
/// `TestSettingsRequestSerialization`: 11x11, turn 0, snakes one/two with
/// single-segment bodies and health 0 (Go's NewBoardState leaves it 0).
fn go_fixture_game(settings: StandardSettings, royale: RoyaleSettings) -> EngineGame {
    let snake = |id: &str, x: i32| rules::Snake {
        id: id.to_string(),
        body: vec![Point::new(x, 3)],
        health: 0,
        eliminated_cause: EliminationCause::NotEliminated,
        eliminated_by: String::new(),
        eliminated_on_turn: 0,
    };
    EngineGame {
        board: rules::BoardState {
            turn: 0,
            width: 11,
            height: 11,
            food: vec![],
            snakes: vec![snake("one", 3), snake("two", 4)],
            hazards: vec![],
        },
        meta: GameMeta {
            game_id: "GAME_ID".into(),
            ruleset_name: "standard".into(),
            timeout: 500,
            settings,
            royale: Some(royale),
            source: GameSource::Custom,
        },
        snake_names: [("one".into(), "ONE".into()), ("two".into(), "TWO".into())].into(),
    }
}

fn go_fixture_customizations() -> HashMap<String, SnakeCustomizations> {
    [
        (
            "one".to_string(),
            SnakeCustomizations {
                color: "#123456".into(),
                head: "safe".into(),
                tail: "curled".into(),
                author: String::new(),
            },
        ),
        (
            "two".to_string(),
            SnakeCustomizations {
                color: "#654321".into(),
                head: "silly".into(),
                tail: "bolt".into(),
                author: String::new(),
            },
        ),
    ]
    .into()
}

fn fixture_json(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/play/testdata/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}")))
        .unwrap()
}

/// Compare against a Go fixture with exactly one normalization: `game.source`
/// (`""` in the fixtures, `"custom"` from this CLI).
fn assert_matches_go_fixture(actual: serde_json::Value, fixture_name: &str) {
    let mut expected = fixture_json(fixture_name);
    if let Some(source) = expected.get_mut("game").and_then(|g| g.get_mut("source")) {
        *source = serde_json::json!("custom");
    } else if let Some(source) = expected.get_mut("source") {
        *source = serde_json::json!("custom");
    }
    assert_eq!(actual, expected, "{fixture_name}");
}

#[test]
fn snake_request_body_matches_go_fixture() {
    let game = go_fixture_game(
        StandardSettings {
            food_spawn_chance: 15,
            minimum_food: 1,
            hazard_damage_per_turn: 14,
        },
        RoyaleSettings {
            shrink_every_n_turns: 25,
            seed: 0,
        },
    );
    let resolved = ResolvedGame {
        mode: GameMode::Standard,
        wire_ruleset: "standard".into(),
        wire_map: "standard".into(),
    };
    let body = request_body(
        &game,
        "one",
        &HashMap::new(),
        &go_fixture_customizations(),
        &resolved,
    );
    assert_matches_go_fixture(
        serde_json::to_value(&body).unwrap(),
        "snake_request_body.json",
    );
}

/// Go's settings variants (flags 11/7/19/17). The fixtures all carry
/// map "standard" (Go's default map) — including royale and constrictor —
/// pairs our validation table would never produce, which is why
/// `request_body` is pair-parameterized rather than mode-driven.
#[test]
fn snake_request_body_settings_variants_match_go_fixtures() {
    let settings = StandardSettings {
        food_spawn_chance: 11,
        minimum_food: 7,
        hazard_damage_per_turn: 19,
    };
    let royale = RoyaleSettings {
        shrink_every_n_turns: 17,
        seed: 0,
    };
    for (gametype, fixture) in [
        ("standard", "snake_request_body_standard.json"),
        ("solo", "snake_request_body_solo.json"),
        ("royale", "snake_request_body_royale.json"),
        ("constrictor", "snake_request_body_constrictor.json"),
    ] {
        let game = go_fixture_game(settings.clone(), royale.clone());
        let resolved = ResolvedGame {
            mode: GameMode::Standard, // unused by request_body; the wire pair rules
            wire_ruleset: gametype.into(),
            wire_map: "standard".into(),
        };
        let body = request_body(
            &game,
            "one",
            &HashMap::new(),
            &go_fixture_customizations(),
            &resolved,
        );
        assert_matches_go_fixture(serde_json::to_value(&body).unwrap(), fixture);
    }
}

/// The jsonl_* set from Go's TestOutputFile: stub ruleset settings 1/2/3/4,
/// snk_0 "example snake" stacked at (1,5), food (0,4) and (5,5).
fn output_file_game() -> EngineGame {
    let snake = rules::Snake {
        id: "snk_0".to_string(),
        body: vec![Point::new(1, 5); 3],
        health: 100,
        eliminated_cause: EliminationCause::NotEliminated,
        eliminated_by: String::new(),
        eliminated_on_turn: 0,
    };
    EngineGame {
        board: rules::BoardState {
            turn: 0,
            width: 11,
            height: 11,
            food: vec![Point::new(0, 4), Point::new(5, 5)],
            snakes: vec![snake],
            hazards: vec![],
        },
        meta: GameMeta {
            game_id: "GAME_ID".into(),
            ruleset_name: "standard".into(),
            timeout: 500,
            settings: StandardSettings {
                food_spawn_chance: 1,
                minimum_food: 2,
                hazard_damage_per_turn: 3,
            },
            royale: Some(RoyaleSettings {
                shrink_every_n_turns: 4,
                seed: 0,
            }),
            source: GameSource::Custom,
        },
        snake_names: [("snk_0".into(), "example snake".into())].into(),
    }
}

fn output_file_customizations() -> HashMap<String, SnakeCustomizations> {
    [(
        "snk_0".to_string(),
        SnakeCustomizations {
            color: "#123456".into(),
            head: "safe".into(),
            tail: "curled".into(),
            author: String::new(),
        },
    )]
    .into()
}

#[test]
fn jsonl_game_line_matches_go_fixture() {
    let game = output_file_game();
    let resolved = ResolvedGame {
        mode: GameMode::Standard,
        wire_ruleset: "standard".into(),
        wire_map: "standard".into(),
    };
    let body = request_body(
        &game,
        "snk_0",
        &HashMap::new(),
        &output_file_customizations(),
        &resolved,
    );
    assert_matches_go_fixture(serde_json::to_value(&body.game).unwrap(), "jsonl_game.json");
}

#[test]
fn jsonl_turn_frames_match_go_fixtures() {
    let game = output_file_game();
    let resolved = ResolvedGame {
        mode: GameMode::Standard,
        wire_ruleset: "standard".into(),
        wire_map: "standard".into(),
    };
    // Turn 0: no measurement -> latency "0".
    let turn0 = request_body(
        &game,
        "snk_0",
        &HashMap::new(),
        &output_file_customizations(),
        &resolved,
    );
    assert_matches_go_fixture(serde_json::to_value(&turn0).unwrap(), "jsonl_turn_0.json");

    // Turn 1: latency 42ms measured.
    let mut game1 = output_file_game();
    game1.board.turn = 1;
    let contexts: HashMap<String, SnakeContext> = [(
        "snk_0".to_string(),
        SnakeContext {
            latency_ms: Some(42),
            shout: None,
        },
    )]
    .into();
    let turn1 = request_body(
        &game1,
        "snk_0",
        &contexts,
        &output_file_customizations(),
        &resolved,
    );
    assert_matches_go_fixture(serde_json::to_value(&turn1).unwrap(), "jsonl_turn_1.json");
}

// ---------------------------------------------------------------------------
// Mock snake + run_game behavior
// ---------------------------------------------------------------------------

mod run_game_tests {
    use super::*;
    use crate::play::http::SnakeHttpClient;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const METADATA: &str = r##"{"apiversion":"1","color":"#888800","head":"safe","tail":"bolt"}"##;
    /// 2x2 square walk keyed off the request's turn (Up,Right,Down,Left).
    const CYCLE: [&str; 4] = ["up", "right", "down", "left"];

    async fn square_walk_server() -> MockServer {
        let server = MockServer::start().await;
        mount_common(&server, None).await;
        for turn in 0..40 {
            let move_json = json!({"move": CYCLE[turn % 4]});
            Mock::given(method("POST"))
                .and(path("/move"))
                .and(body_partial_json(json!({"turn": turn})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(serde_json::to_string(&move_json).unwrap()),
                )
                .mount(&server)
                .await;
        }
        server
    }

    async fn always_up_server() -> MockServer {
        let server = MockServer::start().await;
        mount_common(&server, Some(json!({"move": "up"}))).await;
        server
    }

    async fn mount_common(server: &MockServer, move_response: Option<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(METADATA))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(server)
            .await;
        if let Some(response) = move_response {
            Mock::given(method("POST"))
                .and(path("/move"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(serde_json::to_string(&response).unwrap()),
                )
                .mount(server)
                .await;
        }
    }

    fn config_for_mode(
        mode_args: (&str, Option<&str>),
        urls: &[String],
        names: &[&str],
    ) -> PlayConfig {
        let args = PlayArgs {
            urls: urls.to_vec(),
            names: names.iter().map(|s| s.to_string()).collect(),
            gametype: mode_args.0.into(),
            map: mode_args.1.map(String::from),
            seed: Some(42),
            ..base_args()
        };
        let mut cfg = validate(&args, false).expect("valid config");
        cfg.output_path = Some("out.jsonl".into());
        cfg
    }

    async fn run_with_output(cfg: &PlayConfig) -> (Vec<u8>, Vec<u8>, super::super::GameOutcome) {
        let client = SnakeHttpClient::new(cfg.timeout_ms, false);
        let mut out = Vec::new();
        let mut log = Vec::new();
        let mut board = Vec::new();
        let outcome = run_game(cfg, &client, Some(&mut out), &mut log, &mut board)
            .await
            .expect("game runs");
        (out, log, outcome)
    }

    /// Running twice with the same seed produces identical boards per turn
    /// (per-mode; the latency field is NOT compared — it is measured).
    #[tokio::test]
    async fn same_seed_replays_identically_in_every_mode() {
        for (mode_args, snake_count, extra) in [
            (("standard", None), 2, 25),
            (("royale", None), 2, 3), // hazards visible before wall deaths
            (("constrictor", None), 2, 25),
            (("snail_mode", None), 2, 25),
            (("solo", None), 1, 25),
        ] {
            let server = always_up_server().await;
            let urls: Vec<String> = (0..snake_count).map(|_| server.uri()).collect();
            let names: Vec<&str> = (0..snake_count)
                .map(|i| match i {
                    0 => "A",
                    _ => "B",
                })
                .collect();
            let mut cfg = config_for_mode(mode_args, &urls, &names);
            cfg.royale_shrink = extra;

            let (out1, _log1, o1) = run_with_output(&cfg).await;
            let (out2, _log2, o2) = run_with_output(&cfg).await;

            assert_eq!(o1.final_turn, o2.final_turn, "{mode_args:?}");
            let strip = |out: &[u8]| -> Vec<serde_json::Value> {
                String::from_utf8(out.to_vec())
                    .unwrap()
                    .lines()
                    .skip(1) // game line (contains the seed-derived game id)
                    .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                    .map(|mut v| {
                        // Latency is measured wall-clock, not a function of
                        // the seed: mask it in you AND board.snakes.
                        if let Some(you) = v.get_mut("you") {
                            you["latency"] = json!("x");
                        }
                        if let Some(snakes) =
                            v["board"].get_mut("snakes").and_then(|s| s.as_array_mut())
                        {
                            for snake in snakes {
                                snake["latency"] = json!("x");
                            }
                        }
                        v
                    })
                    .collect()
            };
            assert_eq!(strip(&out1), strip(&out2), "{mode_args:?}: replay diverged");
        }
    }

    /// Royale on its own has hazards (arena always does; Go's didn't because
    /// its hazards come from the map).
    #[tokio::test]
    async fn royale_alone_has_hazards() {
        let server = square_walk_server().await;
        let urls = vec![server.uri(), server.uri()];
        let mut cfg = config_for_mode(("royale", None), &urls, &["A", "B"]);
        cfg.royale_shrink = 3;
        let (out, _log, _outcome) = run_with_output(&cfg).await;
        let frames = String::from_utf8(out).unwrap();
        let any_hazards = frames
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("board").is_some())
            .any(|v| !v["board"]["hazards"].as_array().unwrap().is_empty());
        assert!(any_hazards, "no Royale frame carried hazards");
    }

    /// Constrictor never spawns food whatever the flags say, while the
    /// settings block still shows the flag values (Go's wire behaviour).
    #[tokio::test]
    async fn constrictor_has_no_food_but_advertises_the_flags() {
        let server = square_walk_server().await;
        let urls = vec![server.uri(), server.uri()];
        let mut cfg = config_for_mode(("constrictor", None), &urls, &["A", "B"]);
        cfg.settings = StandardSettings {
            food_spawn_chance: 100,
            minimum_food: 5,
            hazard_damage_per_turn: 14,
        };
        let (out, _log, _outcome) = run_with_output(&cfg).await;
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.lines();
        let game_line: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        // Line 1 is the bare NestedGame: settings live directly under
        // `ruleset`, with no `game` wrapper.
        let settings = &game_line["ruleset"]["settings"];
        assert_eq!(settings["foodSpawnChance"], 100);
        assert_eq!(settings["minimumFood"], 5);
        for line in lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(board) = v.get("board") {
                assert!(
                    board["food"].as_array().unwrap().is_empty(),
                    "constrictor frame carried food: {board}"
                );
            }
        }
    }

    /// A single-snake `-g standard` game runs past turn 0 (the solo
    /// game-over check applies by snake count, like Go's WithSolo).
    #[tokio::test]
    async fn single_snake_standard_runs_past_turn_zero() {
        let server = always_up_server().await;
        let cfg = config_for_mode(("standard", None), &[server.uri()], &["A"]);
        let (_out, log, outcome) = run_with_output(&cfg).await;
        assert!(outcome.final_turn > 0, "ended at turn 0");
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains("Turn: 1,"), "no turn-1 state line:\n{log}");
    }

    /// Turn limit with 2+ alive is a draw; the final log line is the
    /// turn-limit line.
    #[tokio::test]
    async fn turn_limit_draw_and_final_log_line() {
        let server = square_walk_server().await;
        let urls = vec![server.uri(), server.uri()];
        let args = PlayArgs {
            urls: urls.clone(),
            names: vec!["A".into(), "B".into()],
            gametype: "standard".into(),
            map: None,
            seed: Some(42),
            max_turns: Some(3), // the limit arrives the way users set it now
            ..base_args()
        };
        let mut cfg = validate(&args, false).expect("valid config");
        cfg.output_path = Some("out.jsonl".into());
        // No food: bodies stay length 3 (or 4 after initial food), so the
        // square walk never dies.
        cfg.settings = StandardSettings {
            food_spawn_chance: 0,
            minimum_food: 0,
            hazard_damage_per_turn: 14,
        };

        let (out, log, outcome) = run_with_output(&cfg).await;
        assert!(outcome.hit_turn_limit);
        assert!(outcome.is_draw);
        assert_eq!(outcome.final_turn, 3);
        assert_eq!(outcome.winner_id, None);

        let log = String::from_utf8(log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        let completed = lines
            .iter()
            .position(|l| *l == "Game completed after 3 turns. It was a draw.")
            .expect("draw line");
        let wrote = lines
            .iter()
            .position(|l| l.starts_with("Wrote ") && l.ends_with("out.jsonl"))
            .expect("wrote line");
        assert_eq!(wrote, completed + 1, "Wrote follows the completed line");
        assert_eq!(
            lines.last().copied(),
            Some("Game stopped at the turn limit of 3 turns."),
            "turn-limit line is the final log line"
        );

        // Line count = final turn + 3; result line is a draw with empty winners.
        let text = String::from_utf8(out).unwrap();
        let count = text.lines().count();
        assert_eq!(count, 3 + 3, "game + (turn+1) frames + result");
        let result: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(result["isDraw"], true);
        assert_eq!(result["winnerId"], "");
        assert_eq!(result["winnerName"], "");
    }

    /// A survivor wins: snake A always moves up (wall death), snake B walks
    /// the square (never dies) — B wins regardless of spawn placement.
    #[tokio::test]
    async fn winner_line_and_result() {
        let square = square_walk_server().await;
        let up = always_up_server().await;
        // First URL = A (dies), second = B (survives).
        let urls = vec![up.uri(), square.uri()];
        let cfg = config_for_mode(("standard", None), &urls, &["Doomed", "Immortal"]);
        let (out, log, outcome) = run_with_output(&cfg).await;

        assert_eq!(outcome.winner_name.as_deref(), Some("Immortal"));
        assert!(!outcome.is_draw);
        let log = String::from_utf8(log).unwrap();
        let n = outcome.final_turn;
        assert!(
            log.contains(&format!(
                "Game completed after {n} turns. Immortal was the winner."
            )),
            "winner line missing:\n{log}"
        );
        let result: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!(result["isDraw"], false);
        assert_eq!(result["winnerName"], "Immortal");
    }

    /// Solo death: the unique placement-1 snake is dead, so no winner and no
    /// draw — Go's plain completion line.
    #[tokio::test]
    async fn solo_death_prints_the_plain_line() {
        let server = always_up_server().await;
        let cfg = config_for_mode(("solo", None), &[server.uri()], &["A"]);
        let (out, log, outcome) = run_with_output(&cfg).await;

        assert_eq!(outcome.winner_id, None);
        assert!(!outcome.is_draw);
        let log = String::from_utf8(log).unwrap();
        let n = outcome.final_turn;
        assert!(
            log.contains(&format!("Game completed after {n} turns.\n")),
            "plain completion line missing:\n{log}"
        );
        assert!(!log.contains("was the winner"));
        assert!(!log.contains("draw"));
        let result: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!(result["isDraw"], false);
        assert_eq!(result["winnerId"], "");
    }

    /// End-to-end export shape per mode: version "cli", source "custom",
    /// settings block = the flags, map/ruleset pair = the resolved pair,
    /// `you` = the first snake in every frame.
    #[tokio::test]
    async fn export_shape_matches_flags_in_every_mode() {
        for mode_args in [
            ("standard", None),
            ("solo", None),
            ("royale", None),
            ("constrictor", None),
            ("snail_mode", None),
        ] {
            let server = always_up_server().await;
            let cfg = config_for_mode(mode_args, &[server.uri()], &["A"]);
            let (out, _log, outcome) = run_with_output(&cfg).await;

            let text = String::from_utf8(out).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(
                lines.len(),
                outcome.final_turn as usize + 3,
                "{mode_args:?}: game + (turn+1) frames + result"
            );

            let game: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(game["ruleset"]["version"], "cli", "{mode_args:?}");
            assert_eq!(game["source"], "custom", "{mode_args:?}");
            assert_eq!(
                game["ruleset"]["settings"]["foodSpawnChance"], 15,
                "{mode_args:?}"
            );
            assert_eq!(
                game["ruleset"]["settings"]["minimumFood"], 1,
                "{mode_args:?}"
            );
            assert_eq!(
                game["ruleset"]["settings"]["hazardDamagePerTurn"], 14,
                "{mode_args:?}"
            );
            assert_eq!(
                game["ruleset"]["settings"]["royale"]["shrinkEveryNTurns"], 25,
                "{mode_args:?}"
            );

            let (want_ruleset, want_map) = match mode_args {
                ("solo", _) => ("solo", "standard"),
                ("royale", _) => ("royale", "royale"),
                ("constrictor", _) => ("constrictor", "empty"),
                ("snail_mode", _) => ("standard", "snail_mode"),
                _ => ("standard", "standard"),
            };
            assert_eq!(game["ruleset"]["name"], want_ruleset, "{mode_args:?}");
            assert_eq!(game["map"], want_map, "{mode_args:?}");

            for line in &lines[1..lines.len() - 1] {
                let frame: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_eq!(frame["game"]["ruleset"]["version"], "cli", "{mode_args:?}");
                // `you` is the first (only) snake in every frame.
                assert_eq!(frame["you"]["name"], "A", "{mode_args:?}");
            }
        }
    }

    /// The output writer is written from scratch, not appended: a second run
    /// with a shorter game leaves only the new lines.
    #[tokio::test]
    async fn output_is_rewritten_not_appended() {
        let up = always_up_server().await;
        let cfg = config_for_mode(("solo", None), &[up.uri()], &["A"]);
        let (out1, _log1, o1) = run_with_output(&cfg).await;

        let mut cfg2 = cfg.clone();
        cfg2.max_turns = Some(2);
        let (out2, _log2, o2) = run_with_output(&cfg2).await;

        assert_eq!(
            String::from_utf8(out2.clone()).unwrap().lines().count(),
            o2.final_turn as usize + 3
        );
        assert!(o2.final_turn <= o1.final_turn);
        assert_ne!(
            String::from_utf8(out1.clone()).unwrap(),
            String::from_utf8(out2.clone()).unwrap(),
            "different games must write different bytes"
        );
    }

    /// Sequential mode (-s) runs the same game: /move one at a time, same
    /// outcome shape as concurrent.
    #[tokio::test]
    async fn sequential_mode_runs() {
        let server = always_up_server().await;
        let urls = vec![server.uri(), server.uri()];
        let mut cfg = config_for_mode(("standard", None), &urls, &["A", "B"]);
        cfg.sequential = true;
        let (out, _log, outcome) = run_with_output(&cfg).await;
        assert!(outcome.final_turn > 0);
        assert_eq!(
            String::from_utf8(out).unwrap().lines().count(),
            outcome.final_turn as usize + 3
        );
    }

    /// `GET /` is called exactly once per snake before the game, and every
    /// snake — including one that died mid-game — receives exactly one
    /// `/end`. Each server backs exactly one snake, so exact counts are
    /// assertable (wiremock `.expect(n)` + `verify`).
    #[tokio::test]
    async fn get_root_once_per_snake_and_end_for_every_snake() {
        // Doomed always moves up and dies; Immortal walks the square and
        // wins (the same setup as `winner_line_and_result`).
        let doomed = MockServer::start().await;
        let immortal = MockServer::start().await;
        for server in [&doomed, &immortal] {
            Mock::given(method("GET"))
                .and(path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_string(METADATA))
                .expect(1)
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/start"))
                .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/end"))
                .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
                .expect(1)
                .mount(server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&doomed)
            .await;
        for turn in 0..40 {
            let move_json = json!({"move": CYCLE[turn % 4]});
            Mock::given(method("POST"))
                .and(path("/move"))
                .and(body_partial_json(json!({"turn": turn})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(serde_json::to_string(&move_json).unwrap()),
                )
                .mount(&immortal)
                .await;
        }

        let urls = vec![doomed.uri(), immortal.uri()];
        let cfg = config_for_mode(("standard", None), &urls, &["Doomed", "Immortal"]);
        let (_out, _log, outcome) = run_with_output(&cfg).await;

        assert_eq!(outcome.winner_name.as_deref(), Some("Immortal"));
        doomed.verify().await;
        immortal.verify().await;
    }

    /// `-s` actually serializes the `/move` round (not just "runs"): each
    /// snake gets its own server whose /move answers after a 600 ms delay,
    /// so a max-turns-1 game's single move round costs >= 2x the delay
    /// sequential and ~1x concurrent. Server-side delays dominate, so slow
    /// runners inflate both sides roughly equally.
    async fn delayed_snake(delay_ms: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(METADATA))
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
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"move":"up"}"#)
                    .set_delay(std::time::Duration::from_millis(delay_ms)),
            )
            .mount(&server)
            .await;
        server
    }

    async fn run_one_turn_game(sequential: bool) -> std::time::Duration {
        let a = delayed_snake(600).await;
        let b = delayed_snake(600).await;
        let urls = vec![a.uri(), b.uri()];
        let mut cfg = config_for_mode(("standard", None), &urls, &["A", "B"]);
        cfg.sequential = sequential;
        cfg.max_turns = Some(1);
        // The 600 ms delays must land inside the snake timeout, or every
        // /move times out at the default 500 ms and the assert measures
        // timeouts, not serialization.
        cfg.timeout_ms = 5000;
        let started = std::time::Instant::now();
        let _ = run_with_output(&cfg).await;
        started.elapsed()
    }

    #[tokio::test]
    async fn sequential_serializes_the_move_round() {
        let concurrent = run_one_turn_game(false).await;
        let sequential = run_one_turn_game(true).await;
        assert!(
            sequential.as_millis() as f64 > 1.5 * concurrent.as_millis() as f64,
            "-s must serialize the move round: seq={sequential:?} conc={concurrent:?}"
        );
        assert!(
            sequential.as_millis() >= 1200,
            "two serialized 600 ms moves cost at least 1200 ms: {sequential:?}"
        );
        assert!(
            concurrent.as_millis() < 1200,
            "concurrent moves overlap instead of serializing: {concurrent:?}"
        );
    }

    /// Verbose mode logs every POST with its URL and body.
    #[tokio::test]
    async fn verbose_logs_post_lines() {
        let server = always_up_server().await;
        let mut cfg = config_for_mode(("solo", None), &[server.uri()], &["A"]);
        cfg.verbose = true;
        let client = SnakeHttpClient::new(cfg.timeout_ms, true);
        let mut out = Vec::new();
        let mut log = Vec::new();
        let mut board = Vec::new();
        run_game(&cfg, &client, Some(&mut out), &mut log, &mut board)
            .await
            .expect("runs");
        let log = String::from_utf8(log).unwrap();
        assert!(
            log.contains(&format!("POST {}/start: {{", server.uri())),
            "start POST line missing:\n{log}"
        );
        assert!(
            log.contains(&format!("POST {}/move: {{", server.uri())),
            "move POST line missing:\n{log}"
        );
        assert!(
            log.contains(&format!("POST {}/end: {{", server.uri())),
            "end POST line missing:\n{log}"
        );
    }

    /// A ping failure names the URL and wraps Go's error text.
    #[tokio::test]
    async fn ping_failure_wraps_go_error() {
        // Port 1 on 127.0.0.1: nothing listens there.
        let cfg = config_for_mode(("solo", None), &["http://127.0.0.1:1".into()], &["A"]);
        let client = SnakeHttpClient::new(300, false);
        let mut out = Vec::new();
        let mut log = Vec::new();
        let mut board = Vec::new();
        let err = run_game(&cfg, &client, Some(&mut out), &mut log, &mut board)
            .await
            .expect_err("ping must fail");
        let text = format!("{err:#}");
        assert!(text.contains("Error getting snake metadata"), "{text}");
        assert!(
            text.contains("Snake metadata request to http://127.0.0.1:1"),
            "{text}"
        );
    }

    /// Metadata precedence: nested customizations win, then top-level, then
    /// empty strings.
    #[test]
    fn display_fields_precedence() {
        let parse = |body: &str| serde_json::from_str::<wire::SnakeInfoResponse>(body).unwrap();
        let nested = parse(
            r##"{"color":"#111111","customizations":{"color":"#222222","head":"h","tail":"t"}}"##,
        );
        assert_eq!(
            super::display_fields(&nested),
            ("#222222".into(), "h".into(), "t".into())
        );
        let top = parse(r##"{"color":"#111111","head":"hh","tail":"tt"}"##);
        assert_eq!(
            super::display_fields(&top),
            ("#111111".into(), "hh".into(), "tt".into())
        );
        let empty = parse("{}");
        assert_eq!(
            super::display_fields(&empty),
            (String::new(), String::new(), String::new())
        );
    }
}
