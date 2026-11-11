//! `battlesnake play`: run a complete Battlesnake game locally against
//! snake URLs over HTTP — no arena account, no config file, no network
//! beyond the snake URLs (ported from the Go CLI's `cli/commands/play.go`,
//! running arena's five game modes through `engine-core`).

pub mod board_server;
pub mod board_view;
pub mod export;
pub mod http;
pub mod mode;
pub mod names;

use std::collections::HashMap;
use std::io::Write;

use color_eyre::eyre::Context as _;
use color_eyre::eyre::{Result, eyre};
use futures::future::join_all;
use rules::{Direction, RoyaleSettings, StandardSettings};

use engine_core::wire::{self, SnakeContext, SnakeCustomizations};
use engine_core::{EngineGame, GameMode, InitialSnake};

pub use mode::ResolvedGame;

/// One snake entering the game, paired by position from `-n`/`-u`.
#[derive(Debug, Clone)]
pub struct SnakeSpec {
    pub name: String,
    pub url: String,
}

/// The flag surface, byte-for-byte Go's long/short names (including
/// camelCase). Every field whose long name is not the kebab-case of its Rust
/// name has an explicit `long`. `--max-turns` is the single arena-added
/// flag; every other long/short name is byte-for-byte Go's, including
/// `--browser`/`--board-url` (DEV-2146). `--config`/`$HOME/.battlesnake.yaml`
/// are deliberately absent (dropped: nothing read them).
#[derive(clap::Args, Debug)]
pub struct PlayArgs {
    /// Width of Board
    #[arg(short = 'W', long, default_value_t = 11)]
    pub width: i32,
    /// Height of Board
    #[arg(short = 'H', long, default_value_t = 11)]
    pub height: i32,
    /// Name of Snake
    #[arg(short = 'n', long = "name", value_name = "NAME")]
    pub names: Vec<String>,
    /// URL of Snake
    #[arg(short = 'u', long = "url", value_name = "URL")]
    pub urls: Vec<String>,
    /// Request Timeout
    // 0 means 500, like the Go CLI (normalized in `validate`).
    #[arg(
        short = 't',
        long,
        default_value_t = 500,
        allow_negative_numbers = true
    )]
    pub timeout: i64,
    /// Use Sequential Processing
    #[arg(short = 's', long, default_value_t = false)]
    pub sequential: bool,
    /// Type of Game Rules
    #[arg(short = 'g', long, default_value = "standard")]
    pub gametype: String,
    /// Game map to use to populate the board
    // No clap default: "omitted" and "standard" resolve differently
    // (e.g. `-g royale` alone is Royale; `-g royale -m standard` is an
    // error). Go defaulted `-m` to standard — the source of its
    // royale-without-hazards behaviour, which this CLI deliberately drops.
    #[arg(short = 'm', long = "map", value_name = "MAP")]
    pub map: Option<String>,
    /// View the Map Each Turn
    #[arg(short = 'v', long = "viewmap", default_value_t = false)]
    pub view_map: bool,
    /// Use color to draw the map
    #[arg(short = 'c', long, default_value_t = false)]
    pub color: bool,
    /// Random Seed
    // Go's -r is an int64 flag and accepts negatives.
    #[arg(short = 'r', long, allow_negative_numbers = true)]
    pub seed: Option<i64>,
    /// Turn Delay in Milliseconds
    #[arg(short = 'd', long = "delay", default_value_t = 0)]
    pub delay_ms: u64,
    /// Minimum Turn Duration in Milliseconds
    #[arg(short = 'D', long = "duration", default_value_t = 0)]
    pub duration_ms: u64,
    /// File path to output game state to. Existing files will be overwritten
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<std::path::PathBuf>,
    /// Percentage chance of spawning a new food every round
    #[arg(long = "foodSpawnChance", default_value_t = 15)]
    pub food_spawn_chance: i32,
    /// Minimum food to keep on the board every turn
    #[arg(long = "minimumFood", default_value_t = 1)]
    pub minimum_food: i32,
    /// Health damage a snake will take when ending its turn in a hazard
    #[arg(long = "hazardDamagePerTurn", default_value_t = 14)]
    pub hazard_damage_per_turn: i32,
    /// In Royale mode, the number of turns between generating new hazards
    #[arg(long = "shrinkEveryNTurns", default_value_t = 25)]
    pub shrink_every_n_turns: i32,
    /// Stop the game at this turn. Omitted: no limit, the game runs until
    /// it's over.
    // An arena addition, not a Go flag — Go's play had no cap at all — so
    // it gets a normal kebab-case long name (DEV-2164).
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    pub max_turns: Option<i32>,
    /// View the game in the browser using the Battlesnake game board
    #[arg(long, default_value_t = false)]
    pub browser: bool,
    /// Base URL for the game board when using --browser
    #[arg(
        long = "board-url",
        default_value = "https://board.battlesnake.com",
        value_name = "URL"
    )]
    pub board_url: String,
}

/// Fully-validated, engine-ready inputs. Built from `PlayArgs` by pure
/// validation code so tests construct configs directly.
#[derive(Debug, Clone)]
pub struct PlayConfig {
    pub width: i32,
    pub height: i32,
    pub resolved: ResolvedGame,
    /// Raw `-g` value, for the `Ruleset: ..., Seed: ...` log line.
    pub gametype_label: String,
    /// Normalized (0 => 500), never negative.
    pub timeout_ms: i64,
    pub sequential: bool,
    pub view_map: bool,
    pub color: bool,
    pub verbose: bool,
    /// As given/defaulted; printed as-is; seeds the RNG via `as u64`.
    pub seed: i64,
    pub delay_ms: u64,
    pub duration_ms: u64,
    pub output_path: Option<std::path::PathBuf>,
    /// Flag-derived settings, sent verbatim in every mode (Constrictor's
    /// no-food behaviour comes from the shared engine, not zeroed flags).
    pub settings: StandardSettings,
    /// Validated >= 1 when the mode is Royale (populate_hazards errors
    /// mid-game otherwise, after /start already went out).
    pub royale_shrink: i32,
    pub snakes: Vec<SnakeSpec>,
    /// `Some(n)` from `--max-turns n`: stop at turn n. `None` (default):
    /// no limit — the game runs until it's over, like Go's CLI.
    /// `engine_core::MAX_TURNS` (5,000) stays server/prod-only.
    pub max_turns: Option<i32>,
    /// `Some(opts)` when `--browser`: the board server's launch options
    /// (base URL, browser spawn, shutdown timeouts). Whether browser mode
    /// is on lives ONLY here.
    pub browser: Option<board_server::BrowserOptions>,
}

/// Validate the parsed flags into a `PlayConfig`. Pure: no I/O, no clock
/// (except the default seed), injectable namer — everything here is
/// unit-testable.
pub fn validate(args: &PlayArgs, verbose: bool) -> Result<PlayConfig> {
    if args.timeout < 0 {
        return Err(eyre!("timeout must not be negative"));
    }
    let resolved = mode::resolve_mode(&args.gametype, args.map.as_deref())?;
    mode::validate_board_size(args.width, args.height)?;
    let snakes = mode::pair_snakes(&args.names, &args.urls, names::generate)?;
    mode::validate_snake_count(&resolved, snakes.len())?;
    if resolved.mode == GameMode::Royale && args.shrink_every_n_turns < 1 {
        return Err(eyre!(
            "shrinkEveryNTurns must be at least 1 in royale games (got {})",
            args.shrink_every_n_turns
        ));
    }
    if let Some(max_turns) = args.max_turns
        && max_turns < 1
    {
        return Err(eyre!("--max-turns must be at least 1 (got {max_turns})"));
    }
    let seed = args.seed.unwrap_or_else(default_seed);
    Ok(PlayConfig {
        width: args.width,
        height: args.height,
        resolved,
        gametype_label: args.gametype.clone(),
        timeout_ms: if args.timeout == 0 { 500 } else { args.timeout },
        sequential: args.sequential,
        view_map: args.view_map,
        color: args.color,
        verbose,
        seed,
        delay_ms: args.delay_ms,
        duration_ms: args.duration_ms,
        output_path: args.output.clone(),
        settings: StandardSettings {
            food_spawn_chance: args.food_spawn_chance,
            minimum_food: args.minimum_food,
            hazard_damage_per_turn: args.hazard_damage_per_turn,
        },
        royale_shrink: args.shrink_every_n_turns,
        snakes,
        max_turns: args.max_turns,
        browser: if args.browser {
            Some(board_server::BrowserOptions {
                board_url: args.board_url.clone(),
                // Test/CI affordance for the spawned binary; in-process
                // tests build PlayConfig directly with open_browser:
                // false instead.
                open_browser: std::env::var_os("BATTLESNAKE_DONT_OPEN_BROWSER").is_none(),
                ..Default::default()
            })
        } else {
            None
        },
    })
}

/// Go defaults the seed to `time.Now().UTC().UnixNano()`; nanosecond
/// resolution makes consecutive runs differ.
fn default_seed() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Build the snake request body: the shared conversion, then the three
/// public fields `play` owns patched on — `ruleset.name`/`map` follow the
/// user's flags (see `resolve_mode`), `ruleset.version` is `cli`.
///
/// The single place bodies are built: `/start`, `/move`, `/end` and the
/// `--output` frames all go through here, so `you` (including for dead
/// snakes on `/end`) is always correct.
pub fn request_body(
    game: &EngineGame,
    you_snake_id: &str,
    snake_contexts: &HashMap<String, SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
    resolved: &ResolvedGame,
) -> wire::Game {
    let mut body = wire::Game::from_engine_game(game, you_snake_id, snake_contexts, customizations);
    body.game.ruleset.name = resolved.wire_ruleset.clone();
    body.game.ruleset.version = "cli".to_string();
    body.game.map = resolved.wire_map.clone();
    body
}

/// What happened in a completed game.
#[derive(Debug, Clone)]
pub struct GameOutcome {
    pub final_turn: i32,
    /// `None` on a draw or when no snake won (solo death).
    pub winner_id: Option<String>,
    pub winner_name: Option<String>,
    pub is_draw: bool,
    /// The loop stopped at `--max-turns` rather than by game over.
    pub hit_turn_limit: bool,
    /// Lines in the `--output` file (0 when not exporting).
    pub lines_written: usize,
}

/// `battlesnake play` entry point: validate, open the output file (truncate,
/// like Go's Initialize) and run the game. Failures print a plain
/// `Error: ...` line (no color-eyre report) and exit 1; clap usage errors
/// keep clap's exit 2.
pub async fn run_play(args: PlayArgs, verbose: bool) {
    if let Err(error) = run_play_inner(args, verbose).await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run_play_inner(args: PlayArgs, verbose: bool) -> Result<()> {
    let cfg = validate(&args, verbose)?;
    let mut output = match &cfg.output_path {
        Some(path) => Some(
            std::fs::File::create(path)
                .wrap_err_with(|| format!("Failed to open output file: {}", path.display()))?,
        ),
        None => None,
    };
    let client = http::SnakeHttpClient::new(cfg.timeout_ms, cfg.verbose);
    let mut log = std::io::stderr();
    let mut board = std::io::stdout();
    let output_dyn: Option<&mut dyn Write> = output.as_mut().map(|f| f as &mut dyn Write);
    run_game(&cfg, &client, output_dyn, &mut log, &mut board)
        .await
        .map(|_| ())
}

/// Metadata precedence for color/head/tail (the same precedence
/// `game_runner.rs` applies to `SnakeInfoResponse`): nested
/// `customizations.{...}` first, then top-level, then "". Values are used
/// raw — no color normalization, no entitlement resolution.
fn display_fields(info: &wire::SnakeInfoResponse) -> (String, String, String) {
    let nested = |field: fn(&wire::InfoCustomizations) -> String, top: &Option<String>| -> String {
        match &info.customizations {
            Some(c) => field(c),
            None => top.clone().unwrap_or_default(),
        }
    };
    (
        nested(|c| c.color.clone(), &info.color),
        nested(|c| c.head.clone(), &info.head),
        nested(|c| c.tail.clone(), &info.tail),
    )
}

/// Serialize a snake request body where a `Result` context exists: the
/// HTTP client takes pre-serialized bodies (`&str`), because its
/// `MoveResult`/warn-text returns cannot propagate a serialization
/// failure.
fn serialize_body(body: &wire::Game) -> Result<String> {
    serde_json::to_string(body).wrap_err("serializing snake request body")
}

/// Go's DEBUG line for every POST (`--verbose`): the endpoint URL and the
/// request body.
fn log_verbose_post(log: &mut dyn Write, url: &str, endpoint: &str, json: &str) {
    let _ = writeln!(
        log,
        "POST {}: {}",
        engine_core::http_util::build_endpoint_url(url, endpoint),
        json
    );
}

/// Per-snake request state for one `/move` round, all keyed by snake id
/// (grouping keeps `collect_moves` under clippy's argument budget and
/// names the bundle: everything `request_body` needs beyond the board).
struct SnakeTurnState<'a> {
    contexts: &'a HashMap<String, SnakeContext>,
    customizations: &'a HashMap<String, SnakeCustomizations>,
    last_directions: &'a HashMap<String, Direction>,
}

/// One `/move` round for every alive snake, in board order: build and
/// serialize the request bodies, collect the moves (concurrent by default,
/// one at a time with `-s`), and log the failure warnings Go would print
/// (`Got non-ok status code from <url>/move`, `Request to <url>/move
/// failed`, … — URL, status, body and the real error, not just an id).
/// Returns only the move results; the warning texts are logged here.
async fn collect_moves(
    game: &EngineGame,
    cfg: &PlayConfig,
    client: &http::SnakeHttpClient,
    snake_ids: &[String],
    state: SnakeTurnState<'_>,
    log: &mut dyn Write,
) -> Result<Vec<engine_core::move_response::MoveResult>> {
    // Alive snakes in board order (create_initial_game preserves the
    // snake_ids order in board.snakes).
    let alive: Vec<String> = snake_ids
        .iter()
        .filter(|id| {
            game.board
                .snakes
                .iter()
                .any(|s| &s.id == *id && !s.eliminated_cause.is_eliminated())
        })
        .cloned()
        .collect();

    let mut requests = Vec::new();
    for (spec, id) in cfg.snakes.iter().zip(snake_ids) {
        if !alive.contains(id) {
            continue;
        }
        let body = request_body(
            game,
            id,
            state.contexts,
            state.customizations,
            &cfg.resolved,
        );
        let json = serialize_body(&body)?;
        if client.verbose() {
            log_verbose_post(log, &spec.url, "move", &json);
        }
        requests.push((
            spec.url.clone(),
            id.clone(),
            json,
            state.last_directions.get(id).copied(),
        ));
    }

    let collected: Vec<(engine_core::move_response::MoveResult, Option<String>)> = if cfg.sequential
    {
        let mut out = Vec::new();
        for (url, id, json, last) in requests {
            out.push(client.request_move(&url, &json, &id, last).await);
        }
        out
    } else {
        // Own the per-snake request state: join_all holds these
        // futures across awaits, and borrowed locals would not
        // outlive the loop.
        join_all(
            requests
                .into_iter()
                .map(|(url, id, json, last)| async move {
                    client.request_move(&url, &json, &id, last).await
                }),
        )
        .await
    };

    // Warnings in board order (the client cannot log: it has no writer).
    for (_result, warning) in &collected {
        if let Some(warning) = warning {
            let _ = writeln!(log, "{warning}");
        }
    }

    Ok(collected.into_iter().map(|(result, _)| result).collect())
}

/// Run the full game. Logs go to `log`, the `--viewmap` board to `board`,
/// and the JSONL export (if any) to `output`. Test-callable with `Vec<u8>`
/// writers.
pub async fn run_game(
    cfg: &PlayConfig,
    client: &http::SnakeHttpClient,
    mut output: Option<&mut dyn Write>,
    log: &mut dyn Write,
    board: &mut dyn Write,
) -> Result<GameOutcome> {
    let write_log = |log: &mut dyn Write, line: &str| {
        let _ = writeln!(log, "{line}");
    };

    // 1. Seeded RNG + game ID; snake IDs are drawn from the same stream
    //    BEFORE create_initial_game consumes it (draw order is
    //    load-bearing: the whole game is a function of the seed).
    let (mut rng, game_id) = engine_core::seeded_game(cfg.seed as u64);
    use rand::RngCore as _;
    let snake_ids: Vec<String> = (0..cfg.snakes.len())
        .map(|_| {
            let mut bytes = [0u8; 16];
            rng.fill_bytes(&mut bytes);
            uuid::Builder::from_random_bytes(bytes)
                .into_uuid()
                .to_string()
        })
        .collect();

    let initial_snakes: Vec<InitialSnake> = cfg
        .snakes
        .iter()
        .zip(&snake_ids)
        .map(|(spec, id)| InitialSnake {
            id: id.clone(),
            name: spec.name.clone(),
        })
        .collect();
    let mut game = engine_core::create_initial_game(
        &mut rng,
        game_id,
        cfg.width,
        cfg.height,
        cfg.resolved.mode,
        &initial_snakes,
    )?;

    // 2. Flag-driven metadata. meta.source is already Custom (the default)
    //    -> wire game.source = "custom".
    game.meta.timeout = cfg.timeout_ms;
    game.meta.settings = cfg.settings.clone();
    game.meta.royale = Some(match game.meta.royale.take() {
        // Royale: create_initial_game seeded the hazard RNG from the game
        // ID; keep that seed, override only the shrink cadence.
        Some(mut royale) => {
            royale.shrink_every_n_turns = cfg.royale_shrink;
            royale
        }
        // Other modes carry the flag for the wire `settings.royale` block
        // (Go shows the flag value even on standard games). The seed is
        // only consumed by rules::royale::populate_hazards, which never
        // runs outside the royale ruleset.
        None => RoyaleSettings {
            shrink_every_n_turns: cfg.royale_shrink,
            seed: 0,
        },
    });

    // #147 / Go parity: a single-snake game runs the solo game-over check
    // (Go's `WithSolo(len(urls) < 2)`), whatever the -g label says.
    // One-directional on purpose: multi-snake games keep whatever rule
    // create_initial_game chose for their mode.
    if cfg.snakes.len() < 2 {
        game.meta.game_over_rule = engine_core::GameOverRule::AllSnakesDead;
    }

    // 3. GET / per snake, in order (sequential, like Go). Failures are
    //    fatal with Go's wrapper.
    let mut customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
    let mut board_runtime: HashMap<String, board_server::SnakeRuntime> = HashMap::new();
    for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
        let http::PingInfo { info, author } = client
            .ping(&spec.url)
            .await
            .wrap_err("Error getting snake metadata")?;
        let (color, head, tail) = display_fields(&info);
        customizations.insert(
            id.clone(),
            SnakeCustomizations {
                color,
                head,
                tail,
                author,
            },
        );
        board_runtime.insert(id.clone(), board_server::SnakeRuntime::from_ping());
        write_log(
            log,
            &format!("Snake ID: {id} URL: {}, Name: \"{}\"", spec.url, spec.name),
        );
    }

    // 3.5. Launch the board server (Go's order: ping -> open board ->
    //     play; a dead snake URL exits 1 before a tab opens).
    //     `board_srv`, not `board_server`: the module path stays unshadowed
    //     for the `board_server::…` references below (and `board` is the
    //     Write param).
    let mut board_srv = match &cfg.browser {
        Some(opts) => Some(
            board_server::BoardServer::launch(
                board_server::board_game(cfg, &game_id.to_string()),
                opts,
                log,
            )
            .await?,
        ),
        None => None,
    };

    // 4. Turn-0 requests; POST /start sequentially, warn-only.
    let mut snake_contexts: HashMap<String, SnakeContext> = HashMap::new();
    let first_snake = snake_ids
        .first()
        .cloned()
        .ok_or_else(|| eyre!("no snakes"))?;
    for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
        let body = request_body(&game, id, &snake_contexts, &customizations, &cfg.resolved);
        let json = serialize_body(&body)?;
        if client.verbose() {
            log_verbose_post(log, &spec.url, "start", &json);
        }
        if let Some(failure) = client.start(&spec.url, &json).await {
            write_log(log, &failure);
        }
    }

    // 5. Header log line + initial board.
    write_log(
        log,
        &format!("Ruleset: {}, Seed: {}", cfg.gametype_label, cfg.seed),
    );
    print_board(&game, cfg, &customizations, log, board);
    if let Some(srv) = board_srv.as_mut() {
        srv.send_frame(&game, &customizations, &board_runtime)?;
    }

    // 6. Export the turn-0 frame (`you` = the first snake, Go's choice).
    let mut exporter = output.as_mut().map(|_| {
        export::GameExporter::new(
            request_body(
                &game,
                &first_snake,
                &snake_contexts,
                &customizations,
                &cfg.resolved,
            )
            .game,
        )
    });
    if let Some(exporter) = &mut exporter {
        exporter.add_frame(request_body(
            &game,
            &first_snake,
            &snake_contexts,
            &customizations,
            &cfg.resolved,
        ));
    }

    // 7. The game loop. Never issues /move after the game is over or the
    //    turn cap is hit (stop_reason is checked before each iteration);
    //    the reported turn equals the final board's turn.
    let mut last_directions: HashMap<String, Direction> = HashMap::new();
    let names_by_id: HashMap<String, String> = snake_ids
        .iter()
        .cloned()
        .zip(cfg.snakes.iter().map(|s| s.name.clone()))
        .collect();

    let stop = loop {
        if let Some(reason) = engine_core::stop_reason(&game, cfg.max_turns) {
            break reason;
        }

        let turn_deadline = (cfg.duration_ms > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_millis(cfg.duration_ms));

        let results = collect_moves(
            &game,
            cfg,
            client,
            &snake_ids,
            SnakeTurnState {
                contexts: &snake_contexts,
                customizations: &customizations,
                last_directions: &last_directions,
            },
            log,
        )
        .await?;

        let moves: Vec<(String, Direction)> = results
            .iter()
            .map(|r| (r.snake_id.clone(), r.direction))
            .collect();
        for result in &results {
            last_directions.insert(result.snake_id.clone(), result.direction);
        }
        snake_contexts.clear();
        for result in &results {
            snake_contexts.insert(
                result.snake_id.clone(),
                SnakeContext {
                    latency_ms: wire::reported_latency_ms(
                        result.latency_ms,
                        result.outcome.timed_out(),
                        game.meta.timeout,
                    ),
                    shout: result.shout.clone(),
                },
            );
            board_runtime.insert(
                result.snake_id.clone(),
                board_server::SnakeRuntime::from_move_result(result, cfg.timeout_ms),
            );
        }

        engine_core::advance_turn(&mut game, &moves, &mut rng)?;

        print_board(&game, cfg, &customizations, log, board);
        if let Some(srv) = board_srv.as_mut() {
            srv.send_frame(&game, &customizations, &board_runtime)?;
        }

        if cfg.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(cfg.delay_ms)).await;
        }
        if let Some(deadline) = turn_deadline {
            let now = std::time::Instant::now();
            if deadline > now {
                tokio::time::sleep(deadline - now).await;
            }
        }

        if let Some(exporter) = &mut exporter {
            exporter.add_frame(request_body(
                &game,
                &first_snake,
                &snake_contexts,
                &customizations,
                &cfg.resolved,
            ));
        }
    };

    // 7.5. `game_end` goes out immediately after the game loop — before
    //     the /end requests — so a connected browser gets it while those
    //     are still in flight. Also stamps `game_ended_at`, anchoring
    //     `finish`'s no-browser grace window at the true game end. The
    //     last in-loop `send_frame` already carries the final board
    //     state, so this is safe.
    if let Some(srv) = board_srv.as_mut() {
        srv.send_game_end()?;
    }

    // 8. Winner/draw from arena's placement code; /end for every snake
    //    (you = that snake), sequential, warn-only.
    let placements = engine_core::placement::from_final_snakes(&game.board.snakes);
    let winner = engine_core::placement::outright_winner(placements, |(_, place)| Some(*place))
        .filter(|(id, _)| {
            game.board
                .snakes
                .iter()
                .any(|s| &s.id == id && !s.eliminated_cause.is_eliminated())
        });
    let is_draw = cfg.snakes.len() > 1 && winner.is_none();

    for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
        let body = request_body(&game, id, &snake_contexts, &customizations, &cfg.resolved);
        let json = serialize_body(&body)?;
        if client.verbose() {
            log_verbose_post(log, &spec.url, "end", &json);
        }
        if let Some(failure) = client.end(&spec.url, &json).await {
            write_log(log, &failure);
        }
    }

    // 9. Final log block. The turn-limit line is last, so it is the final
    //    log line when it applies.
    let final_turn = game.board.turn;
    match (&winner, is_draw) {
        (_, true) => write_log(
            log,
            &format!("Game completed after {final_turn} turns. It was a draw."),
        ),
        (Some((id, _)), _) => {
            let name = names_by_id.get(id).cloned().unwrap_or_else(|| id.clone());
            write_log(
                log,
                &format!("Game completed after {final_turn} turns. {name} was the winner."),
            );
        }
        (None, false) => write_log(log, &format!("Game completed after {final_turn} turns.")),
    }

    let hit_turn_limit = stop == engine_core::StopReason::TurnCap;

    let mut lines_written = 0usize;
    if let (Some(exporter), Some(out)) = (&mut exporter, output.as_mut()) {
        let (winner_id, winner_name) = match &winner {
            Some((id, _)) => (id.clone(), names_by_id.get(id).cloned().unwrap_or_default()),
            None => (String::new(), String::new()),
        };
        exporter.set_result(&winner_id, &winner_name, is_draw);
        lines_written = exporter.write_to(out)?;
        if let Some(path) = &cfg.output_path {
            write_log(
                log,
                &format!(
                    "Wrote {lines_written} lines to output file: {}",
                    path.display()
                ),
            );
        }
    }

    if let Some(max_turns) = cfg.max_turns
        && hit_turn_limit
    {
        write_log(
            log,
            &format!("Game stopped at the turn limit of {max_turns} turns."),
        );
    }

    // 10. Board server shutdown: the LAST step of the success path. Any
    //     `?` above (engine error, export write failure) skips it — the
    //     CLI exits 1 and process teardown kills the serve task, so a
    //     failed game never waits on a browser. `/end` requests are
    //     warn-only and don't take that path.
    if let Some(srv) = board_srv.as_mut() {
        srv.finish(log).await;
    }

    Ok(GameOutcome {
        final_turn,
        winner_id: winner.as_ref().map(|(id, _)| id.clone()),
        winner_name: winner
            .as_ref()
            .and_then(|(id, _)| names_by_id.get(id).cloned()),
        is_draw,
        hit_turn_limit,
        lines_written,
    })
}

fn print_board(
    game: &EngineGame,
    cfg: &PlayConfig,
    customizations: &HashMap<String, SnakeCustomizations>,
    log: &mut dyn Write,
    board: &mut dyn Write,
) {
    if cfg.view_map {
        let snake_chars: HashMap<String, char> = game
            .board
            .snakes
            .iter()
            .map(|s| s.id.clone())
            .enumerate()
            .map(|(i, id)| (id, board_view::BODY_CHARS[i % 8]))
            .collect();
        let colors: HashMap<String, String> = customizations
            .iter()
            .map(|(k, v)| (k.clone(), v.color.clone()))
            .collect();
        let rendered =
            board_view::render_map(game, &game.snake_names, &snake_chars, &colors, cfg.color);
        let _ = board.write_all(rendered.as_bytes());
    } else {
        board_view::print_state(game, &game.snake_names, log);
    }
}

#[cfg(test)]
mod tests;
