//! `battlesnake play`: run a complete Battlesnake game locally against
//! snake URLs over HTTP — no arena account, no config file, no network
//! beyond the snake URLs (ported from the Go CLI's `cli/commands/play.go`,
//! running arena's five game modes through `engine-core`).

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
use engine_core::{EngineGame, GameMode, InitialSnake, MAX_TURNS};

pub use mode::ResolvedGame;

/// One snake entering the game, paired by position from `-n`/`-u`.
#[derive(Debug, Clone)]
pub struct SnakeSpec {
    pub name: String,
    pub url: String,
}

/// The flag surface, byte-for-byte Go's long/short names (including
/// camelCase). Every field whose long name is not the kebab-case of its Rust
/// name has an explicit `long`.
///
/// `--browser`/`--board-url` are DEV-2146 and deliberately absent, as are
/// `--config`/`$HOME/.battlesnake.yaml` (dropped: nothing read them).
#[derive(clap::Args, Debug)]
pub struct PlayArgs {
    #[arg(short = 'W', long, default_value_t = 11)]
    pub width: i32,
    #[arg(short = 'H', long, default_value_t = 11)]
    pub height: i32,
    #[arg(short = 'n', long = "name", value_name = "NAME")]
    pub names: Vec<String>,
    #[arg(short = 'u', long = "url", value_name = "URL")]
    pub urls: Vec<String>,
    /// 0 means 500, like the Go CLI.
    #[arg(
        short = 't',
        long,
        default_value_t = 500,
        allow_negative_numbers = true
    )]
    pub timeout: i64,
    #[arg(short = 's', long, default_value_t = false)]
    pub sequential: bool,
    #[arg(short = 'g', long, default_value = "standard")]
    pub gametype: String,
    /// No clap default: "omitted" and "standard" resolve differently
    /// (e.g. `-g royale` alone is Royale; `-g royale -m standard` is an
    /// error). Go defaulted `-m` to standard — the source of its
    /// royale-without-hazards behaviour, which this CLI deliberately drops.
    #[arg(short = 'm', long = "map", value_name = "MAP")]
    pub map: Option<String>,
    #[arg(short = 'v', long = "viewmap", default_value_t = false)]
    pub view_map: bool,
    #[arg(short = 'c', long, default_value_t = false)]
    pub color: bool,
    /// Go's -r is an int64 flag and accepts negatives.
    #[arg(short = 'r', long, allow_negative_numbers = true)]
    pub seed: Option<i64>,
    #[arg(short = 'd', long = "delay", default_value_t = 0)]
    pub delay_ms: u64,
    #[arg(short = 'D', long = "duration", default_value_t = 0)]
    pub duration_ms: u64,
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<std::path::PathBuf>,
    #[arg(long = "foodSpawnChance", default_value_t = 15)]
    pub food_spawn_chance: i32,
    #[arg(long = "minimumFood", default_value_t = 1)]
    pub minimum_food: i32,
    #[arg(long = "hazardDamagePerTurn", default_value_t = 14)]
    pub hazard_damage_per_turn: i32,
    #[arg(long = "shrinkEveryNTurns", default_value_t = 25)]
    pub shrink_every_n_turns: i32,
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
    /// `engine_core::MAX_TURNS`; a test override, not a CLI flag.
    pub max_turns: i32,
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
        max_turns: MAX_TURNS,
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
    /// The loop stopped at `max_turns` rather than by game over.
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

/// Run the full game. Logs go to `log`, the `--viewmap` board to `board`,
/// and the JSONL export (if any) to `output`. Test-callable with `Vec<u8>`
/// writers.
#[allow(clippy::too_many_lines)]
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

    // 3. GET / per snake, in order (sequential, like Go). Failures are
    //    fatal with Go's wrapper.
    let mut customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
    for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
        let info = client
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
                author: String::new(),
            },
        );
        write_log(
            log,
            &format!("Snake ID: {id} URL: {}, Name: \"{}\"", spec.url, spec.name),
        );
    }

    // 4. Turn-0 requests; POST /start sequentially, warn-only.
    let mut snake_contexts: HashMap<String, SnakeContext> = HashMap::new();
    let first_snake = snake_ids
        .first()
        .cloned()
        .ok_or_else(|| eyre!("no snakes"))?;
    for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
        let body = request_body(&game, id, &snake_contexts, &customizations, &cfg.resolved);
        if client.verbose() {
            let _ = writeln!(
                log,
                "POST {}: {}",
                engine_core::http_util::build_endpoint_url(&spec.url, "start"),
                serde_json::to_string(&body).unwrap_or_default()
            );
        }
        if let Some(failure) = client.start(&spec.url, &body).await {
            write_log(log, &failure);
        }
    }

    // 5. Header log line + initial board.
    write_log(
        log,
        &format!("Ruleset: {}, Seed: {}", cfg.gametype_label, cfg.seed),
    );
    print_board(&game, cfg, &customizations, log, board);

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

    // 7. The game loop. Never issues /move after is_game_over_for is true;
    //    the reported turn equals the final board's turn.
    let mut last_directions: HashMap<String, Direction> = HashMap::new();
    let names_by_id: HashMap<String, String> = snake_ids
        .iter()
        .cloned()
        .zip(cfg.snakes.iter().map(|s| s.name.clone()))
        .collect();

    while !engine_core::is_game_over_for(&game, cfg.snakes.len()) && game.board.turn < cfg.max_turns
    {
        let turn_deadline = (cfg.duration_ms > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_millis(cfg.duration_ms));

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

        // Collect moves: concurrent by default, sequential with -s.
        let mut results: Vec<engine_core::move_response::MoveResult> = Vec::new();
        if cfg.sequential {
            for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
                if !alive.contains(id) {
                    continue;
                }
                let body = request_body(&game, id, &snake_contexts, &customizations, &cfg.resolved);
                if client.verbose() {
                    let _ = writeln!(
                        log,
                        "POST {}: {}",
                        engine_core::http_util::build_endpoint_url(&spec.url, "move"),
                        serde_json::to_string(&body).unwrap_or_default()
                    );
                }
                results.push(
                    client
                        .request_move(&spec.url, &body, id, last_directions.get(id).copied())
                        .await,
                );
            }
        } else {
            let mut futures = Vec::new();
            for (spec, id) in cfg.snakes.iter().zip(&snake_ids) {
                if !alive.contains(id) {
                    continue;
                }
                let body = request_body(&game, id, &snake_contexts, &customizations, &cfg.resolved);
                if client.verbose() {
                    let _ = writeln!(
                        log,
                        "POST {}: {}",
                        engine_core::http_util::build_endpoint_url(&spec.url, "move"),
                        serde_json::to_string(&body).unwrap_or_default()
                    );
                }
                // Own the per-snake request state: join_all holds these
                // futures across awaits, and borrowed locals would not
                // outlive the loop.
                let url = spec.url.clone();
                let snake_id = id.clone();
                let last = last_directions.get(id).copied();
                futures
                    .push(async move { client.request_move(&url, &body, &snake_id, last).await });
            }
            results = join_all(futures).await;
        }

        // Warnings in board order (the client cannot log: it has no writer).
        for result in &results {
            if result.timed_out {
                write_log(
                    log,
                    &format!(
                        "Request to move endpoint for {} failed\n\tError: request timed out",
                        result.snake_id
                    ),
                );
            } else if result.errored {
                match result.status_code {
                    Some(status) if !(200..300).contains(&status) => write_log(
                        log,
                        &format!(
                            "Got non-ok status code from snake {}\n\tStatusCode: {} (expected 200)",
                            result.snake_id, status
                        ),
                    ),
                    _ => write_log(
                        log,
                        &format!("Failed to parse response from snake {}", result.snake_id),
                    ),
                }
            }
        }

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
                        result.timed_out,
                        game.meta.timeout,
                    ),
                    shout: result.shout.clone(),
                },
            );
        }

        engine_core::advance_turn(&mut game, &moves, &mut rng)?;

        print_board(&game, cfg, &customizations, log, board);

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
        if client.verbose() {
            let _ = writeln!(
                log,
                "POST {}: {}",
                engine_core::http_util::build_endpoint_url(&spec.url, "end"),
                serde_json::to_string(&body).unwrap_or_default()
            );
        }
        if let Some(failure) = client.end(&spec.url, &body).await {
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

    let hit_turn_limit = !engine_core::is_game_over_for(&game, cfg.snakes.len());

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

    if hit_turn_limit {
        write_log(
            log,
            &format!("Game stopped at the turn limit of {} turns.", cfg.max_turns),
        );
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
