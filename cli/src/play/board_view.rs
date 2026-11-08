//! Board rendering for `play`: the per-turn state line (always logged) and
//! the `--viewmap` text/`--color` board. Ported byte-for-byte from the Go
//! CLI's `printState`/`printMap`/`parseSnakeColor` (`cli/commands/play.go`)
//! and `cli/commands/colors.go`.

use std::collections::HashMap;
use std::io::Write;

use engine_core::EngineGame;
use rules::EliminationCause;

/// ANSI escape codes (Go `cli/commands/colors.go`).
const TERM_RESET: &str = "\x1b[0m";
const TERM_BG_GRAY: &str = "\x1b[48;2;127;127;127m";
const TERM_BG_WHITE: &str = "\x1b[107m";
const TERM_FG_LIGHTGRAY: &str = "\x1b[38;2;200;200;200m";
const TERM_FG_FOOD: &str = "\x1b[38;2;255;92;117m";

/// Go's per-snake body glyphs, assigned positionally (`bodyChars[i%8]`).
pub(crate) const BODY_CHARS: [char; 8] = ['■', '⌀', '●', '☻', '◘', '☺', '□', '⍟'];

/// The per-turn log line, Go's `printState`:
/// `Turn: N, Snakes Alive: [names], Food: N, Hazards: N`.
///
/// Hazards count uses the on-board view so Snail Mode's off-board trail
/// bookkeeping is never counted (Go's `boardState.Hazards` has no
/// bookkeeping entries).
pub fn print_state(game: &EngineGame, names: &HashMap<String, String>, log: &mut dyn Write) {
    let alive: Vec<&str> = game
        .board
        .snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .map(|s| {
            names
                .get(&s.id)
                .map(String::as_str)
                .unwrap_or(s.id.as_str())
        })
        .collect();
    let line = format!(
        "Turn: {}, Snakes Alive: [{}], Food: {}, Hazards: {}\n",
        game.board.turn,
        alive.join(", "),
        game.board.food.len(),
        game.board.on_board_hazards().count(),
    );
    let _ = log.write_all(line.as_bytes());
}

/// Go's elimination-cause strings (`constants.go`), via
/// [`rules::EliminationCause::go_slug`], as printed on the per-snake
/// viewmap lines. CLI-local on purpose: arena's server frames use
/// [`rules::EliminationCause::arena_slug`] (`self-collision`), a separate
/// vocabulary that must stay correct separately.
fn go_cause(cause: &EliminationCause) -> &'static str {
    match cause {
        EliminationCause::NotEliminated => "not-eliminated",
        eliminated => eliminated.go_slug(),
    }
}

/// Go's `%v` of a `rules.Point`: `{x y ttl value}` — engine points always
/// carry ttl=0/value=0, so `{x y 0 0}`.
fn go_point(p: &rules::Point) -> String {
    format!("{{{} {} 0 0}}", p.x, p.y)
}

fn go_points(points: impl Iterator<Item = rules::Point>) -> String {
    let rendered: Vec<String> = points.map(|p| go_point(&p)).collect();
    format!("[{}]", rendered.join(" "))
}

/// Parses a color string like "#ef03d3" to rgb values from 0 to 255 or
/// returns the default gray if any errors occur (Go's `parseSnakeColor`).
pub(crate) fn parse_snake_color(color: &str) -> (i64, i64, i64) {
    if color.len() == 7 {
        let bytes = color.as_bytes();
        let parse = |s: &str| i64::from_str_radix(s, 16).ok();
        let parts = (
            std::str::from_utf8(&bytes[1..3]).ok().and_then(parse),
            std::str::from_utf8(&bytes[3..5]).ok().and_then(parse),
            std::str::from_utf8(&bytes[5..7]).ok().and_then(parse),
        );
        if let (Some(r), Some(g), Some(b)) = parts {
            return (r, g, b);
        }
    }
    // Default gray color from Battlesnake board
    (136, 136, 136)
}

/// Render the `--viewmap` board (Go's `printMap` into a String; the caller
/// writes it to the board stream). Layout and bytes match Go exactly:
/// Turn line, Hazards line, Food line, one line per snake (eliminated ones
/// included), then grid rows top-down, then the trailing blank line from
/// Go's `fmt.Println(o.String())` on a buffer that already ends in `\n`
/// (play.go:729) — every frame is followed by one blank line. Bounds-checks
/// every body point — a wall death leaves the head off the board.
pub fn render_map(
    game: &EngineGame,
    names: &HashMap<String, String>,
    snake_chars: &HashMap<String, char>,
    colors: &HashMap<String, String>,
    color: bool,
) -> String {
    use std::fmt::Write as _;

    let board = &game.board;
    let (width, height) = (board.width, board.height);
    let mut o = String::new();
    let _ = writeln!(o, "Turn: {}", board.turn);

    let mut cells = vec![vec![String::new(); height as usize]; width as usize];
    for y in 0..height {
        for x in 0..width {
            cells[x as usize][y as usize] = if color {
                format!("{TERM_FG_LIGHTGRAY}□")
            } else {
                "◦".to_string()
            };
        }
    }

    let hazards: Vec<rules::Point> = board.on_board_hazards().copied().collect();
    for h in &hazards {
        cells[h.x as usize][h.y as usize] = if color {
            format!("{TERM_BG_GRAY} {TERM_BG_WHITE}")
        } else {
            "░".to_string()
        };
    }
    if color {
        let _ = writeln!(
            o,
            "Hazards {TERM_BG_GRAY} {TERM_RESET}: {}",
            go_points(hazards.iter().copied())
        );
    } else {
        let _ = writeln!(o, "Hazards ░: {}", go_points(hazards.iter().copied()));
    }

    for f in &board.food {
        cells[f.x as usize][f.y as usize] = if color {
            format!("{TERM_FG_FOOD}●")
        } else {
            "⚕".to_string()
        };
    }
    if color {
        let _ = writeln!(
            o,
            "Food {TERM_FG_FOOD}{TERM_BG_WHITE}●{TERM_RESET}: {}",
            go_points(board.food.iter().copied())
        );
    } else {
        let _ = writeln!(o, "Food ⚕: {}", go_points(board.food.iter().copied()));
    }

    for s in &board.snakes {
        let name = names.get(&s.id).cloned().unwrap_or_else(|| s.id.clone());
        let character = snake_chars.get(&s.id).copied().unwrap_or('■');
        let (r, g, b) = parse_snake_color(colors.get(&s.id).map(String::as_str).unwrap_or(""));
        for body_point in &s.body {
            if body_point.x >= 0
                && body_point.x < width
                && body_point.y >= 0
                && body_point.y < height
            {
                cells[body_point.x as usize][body_point.y as usize] = if color {
                    format!("\x1b[38;2;{r};{g};{b}m■")
                } else {
                    character.to_string()
                };
            }
        }
        if color {
            let _ = write!(
                o,
                "{name} \x1b[38;2;{r};{g};{b}m{TERM_BG_WHITE}■■■{TERM_RESET}: "
            );
        } else {
            let _ = write!(o, "{name} {character}: ");
        }
        let _ = write!(o, "Health: {}", s.health);
        if s.eliminated_cause.is_eliminated() {
            let _ = write!(
                o,
                ", Eliminated: {}, Turn: {}",
                go_cause(&s.eliminated_cause),
                s.eliminated_on_turn
            );
        }
        let _ = writeln!(o);
    }

    for y in (0..height).rev() {
        if color {
            let _ = write!(o, "{TERM_BG_WHITE}");
        }
        for x in 0..width {
            let _ = write!(o, "{}", cells[x as usize][y as usize]);
        }
        if color {
            let _ = write!(o, "{TERM_RESET}");
        }
        let _ = writeln!(o);
    }
    // Go prints the buffer with fmt.Println, adding one newline on top of
    // the buffer's own trailing \n — i.e. a blank line after every frame.
    o.push('\n');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_snake_color_valid_hex() {
        assert_eq!(parse_snake_color("#12abef"), (18, 171, 239));
        assert_eq!(parse_snake_color("#000000"), (0, 0, 0));
        assert_eq!(parse_snake_color("#ffffff"), (255, 255, 255));
    }

    #[test]
    fn parse_snake_color_garbage_is_gray() {
        for bad in ["", "#12345", "#1234567", "123456", "#gg0000", "#12 456"] {
            assert_eq!(parse_snake_color(bad), (136, 136, 136), "{bad:?}");
        }
    }

    /// A wall death leaves the head off the board; rendering must not panic
    /// and must not draw anything for it.
    #[test]
    fn off_board_head_does_not_panic() {
        let game = test_game(vec![snake_at("s1", (-1, 7), true)], vec![], vec![]);
        let mut names = HashMap::new();
        names.insert("s1".to_string(), "S1".to_string());
        let mut chars = HashMap::new();
        chars.insert("s1".to_string(), '■');
        let colors = HashMap::new();
        let text = render_map(&game, &names, &chars, &colors, false);
        assert!(text.contains("Turn: 0"));
        assert!(text.contains("Eliminated: wall-collision, Turn: 2"));
    }

    /// The Go-generated golden board (see testdata/README.md): render_map's
    /// output — trailing blank line included — must equal the fixture
    /// byte-for-byte.
    fn fixture_game() -> EngineGame {
        let s1 = rules::Snake {
            id: "s1".to_string(),
            body: vec![rules::Point::new(3, 3); 3],
            health: 55,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        };
        let s2 = rules::Snake {
            id: "s2".to_string(),
            body: vec![
                rules::Point::new(-1, 7),
                rules::Point::new(0, 7),
                rules::Point::new(1, 7),
            ],
            health: 0,
            eliminated_cause: EliminationCause::OutOfBounds,
            eliminated_by: String::new(),
            eliminated_on_turn: 2,
        };
        let mut game = test_game(vec![s1, s2], vec![(5, 5)], vec![(2, 2)]);
        game.snake_names.insert("s1".into(), "ONE".into());
        game.snake_names.insert("s2".into(), "TWO".into());
        game
    }

    type FixtureInputs = (
        EngineGame,
        HashMap<String, String>,
        HashMap<String, char>,
        HashMap<String, String>,
    );

    fn fixture_inputs() -> FixtureInputs {
        let game = fixture_game();
        let names: HashMap<String, String> = game.snake_names.clone();
        let chars: HashMap<String, char> =
            [("s1".to_string(), '■'), ("s2".to_string(), '⌀')].into();
        let colors: HashMap<String, String> = [
            ("s1".to_string(), "#123456".to_string()),
            ("s2".to_string(), "not-a-color".to_string()),
        ]
        .into();
        (game, names, chars, colors)
    }

    fn expected_fixture_bytes(name: &str) -> String {
        let path = format!(
            "{}/tests/play/testdata/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        let raw = std::fs::read_to_string(path).expect("fixture exists");
        // Line 1 is the provenance header; everything after is Go's output.
        raw.split_once('\n').expect("header line").1.to_string()
    }

    #[test]
    fn text_viewmap_matches_go_fixture_byte_for_byte() {
        let (game, names, chars, colors) = fixture_inputs();
        let rendered = render_map(&game, &names, &chars, &colors, false);
        let expected = expected_fixture_bytes("viewmap_text.txt");
        assert_eq!(rendered, expected);
    }

    #[test]
    fn color_viewmap_matches_go_fixture_byte_for_byte() {
        let (game, names, chars, colors) = fixture_inputs();
        let rendered = render_map(&game, &names, &chars, &colors, true);
        let expected = expected_fixture_bytes("viewmap_color.txt");
        assert_eq!(rendered, expected);
    }

    // Shared fixture builder for viewmap tests (see
    // cli/tests/play/testdata/viewmap_*.txt for the Go-generated goldens).
    pub(crate) fn snake_at(id: &str, head: (i32, i32), eliminated: bool) -> rules::Snake {
        rules::Snake {
            id: id.to_string(),
            body: vec![
                rules::Point::new(head.0, head.1),
                rules::Point::new(head.0, head.1),
                rules::Point::new(head.0, head.1),
            ],
            health: 42,
            eliminated_cause: if eliminated {
                EliminationCause::OutOfBounds
            } else {
                EliminationCause::NotEliminated
            },
            eliminated_by: String::new(),
            eliminated_on_turn: if eliminated { 2 } else { 0 },
        }
    }

    pub(crate) fn test_game(
        snakes: Vec<rules::Snake>,
        food: Vec<(i32, i32)>,
        hazards: Vec<(i32, i32)>,
    ) -> EngineGame {
        EngineGame {
            board: rules::BoardState {
                turn: 0,
                width: 11,
                height: 11,
                food: food
                    .into_iter()
                    .map(|(x, y)| rules::Point::new(x, y))
                    .collect(),
                snakes,
                hazards: hazards
                    .into_iter()
                    .map(|(x, y)| rules::Point::new(x, y))
                    .collect(),
            },
            meta: engine_core::GameMeta {
                game_id: "g".into(),
                mode: engine_core::GameMode::Standard,
                game_over_rule: engine_core::GameMode::Standard.default_game_over_rule(),
                timeout: 500,
                settings: rules::StandardSettings {
                    food_spawn_chance: 15,
                    minimum_food: 1,
                    hazard_damage_per_turn: 14,
                },
                royale: None,
                source: engine_core::GameSource::Custom,
            },
            snake_names: HashMap::new(),
        }
    }
}
