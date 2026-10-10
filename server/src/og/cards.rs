//! Card layouts. Each takes plain data (no database types) and returns PNG
//! bytes; `routes::og` maps models onto them.
//!
//! Every card shares one frame: the site's dark paper with a faint dot grid,
//! a board panel on the right, a text column on the left, and the wordmark in
//! the bottom-left corner.

use arena::design_kit::{AssetKind, refs::RefShape};
use tiny_skia::{Color, Stroke, Transform};

use super::{
    board::{BoardArt, BoardSnake, shape},
    canvas::{Canvas, HEIGHT, WIDTH, palette, round_rect, solid, with_alpha},
    text::{Font, Line, TextStyle},
};

const PAD: f32 = 64.0;
const W: f32 = WIDTH as f32;
const H: f32 = HEIGHT as f32;
const BOARD_SIDE: f32 = H - 2.0 * PAD;
const BOARD_X: f32 = W - PAD - BOARD_SIDE;
/// Right edge of the text column.
const COLUMN_RIGHT: f32 = BOARD_X - 56.0;
const COLUMN_WIDTH: f32 = COLUMN_RIGHT - PAD;
/// Text column content sits between the kicker row and the footer.
const BODY_TOP: f32 = 124.0;
const BODY_BOTTOM: f32 = 516.0;
const FOOTER_BASELINE: f32 = H - PAD + 2.0;

pub const SITE_HOST: &str = "arena.battlesnake.com";

fn backdrop(c: &mut Canvas) {
    let dot = with_alpha(palette::hairline(), 0.9);
    let step = 30.0;
    let mut y = step / 2.0;
    while y < H {
        let mut x = step / 2.0;
        while x < W {
            c.circle(x, y, 1.4, dot);
            x += step;
        }
        y += step;
    }
    c.glow(PAD + 80.0, 40.0, 560.0, with_alpha(palette::pink(), 0.09));
}

/// The four-tile logo from the site nav, `size` px square.
fn glyph(c: &mut Canvas, x: f32, y: f32, size: f32) {
    let u = size / 24.0;
    let tile = |c: &mut Canvas, tx: f32, ty: f32, color: Color| {
        c.round_rect(x + tx * u, y + ty * u, 9.0 * u, 9.0 * u, 2.5 * u, color);
    };
    tile(c, 2.0, 2.0, palette::pink());
    tile(c, 13.0, 2.0, with_alpha(palette::ink(), 0.2));
    tile(c, 2.0, 13.0, with_alpha(palette::ink(), 0.2));
    tile(c, 13.0, 13.0, with_alpha(palette::pink(), 0.45));
}

fn footer(c: &mut Canvas) {
    let style = TextStyle::new(Font::Display, 26.0, palette::ink()).tracking(-0.01);
    let cap = c.cap_height(Font::Display, 26.0);
    let size = 34.0;
    glyph(c, PAD - 3.0, FOOTER_BASELINE - cap / 2.0 - size / 2.0, size);
    c.text("Battlesnake Arena", style, PAD + 38.0, FOOTER_BASELINE);
    let url = TextStyle::new(Font::Mono, 17.0, palette::muted());
    c.text_right(SITE_HOST, url, COLUMN_RIGHT, FOOTER_BASELINE);
}

/// A small uppercase label in a pill; returns the pill's width.
fn pill(c: &mut Canvas, label: &str, x: f32, top: f32, filled: bool) -> f32 {
    let height = 34.0;
    let style = TextStyle::new(
        Font::Mono,
        15.0,
        if filled {
            palette::paper()
        } else {
            palette::ink()
        },
    )
    .tracking(0.1);
    let line = c.shape(label, style);
    let dot = if filled { 18.0 } else { 0.0 };
    let width = line.width + 32.0 + dot;
    let cap = c.cap_height(Font::Mono, 15.0);
    let baseline = top + height / 2.0 + cap / 2.0;
    if filled {
        c.round_rect(x, top, width, height, height / 2.0, palette::pink());
        c.circle(x + 20.0, top + height / 2.0, 4.5, palette::paper());
    } else {
        c.round_rect(x, top, width, height, height / 2.0, palette::hairline());
    }
    c.draw(&line, x + 16.0 + dot, baseline);
    width
}

/// The kicker row: an optional pill, then uppercase mono text.
fn kicker(c: &mut Canvas, pill_label: Option<(&str, bool)>, text: &str) {
    let mut x = PAD;
    if let Some((label, filled)) = pill_label {
        x += pill(c, label, x, PAD, filled) + 16.0;
    }
    let style = TextStyle::new(Font::Mono, 18.0, palette::muted()).tracking(0.06);
    let cap = c.cap_height(Font::Mono, 18.0);
    let line = c.ellipsize(&text.to_uppercase(), style, COLUMN_RIGHT - x);
    c.draw(&line, x, PAD + 17.0 + cap / 2.0);
}

/// Draw `lines` top-down from `top` with `leading` between baselines; returns
/// the y just below the last line's baseline.
fn draw_lines(c: &mut Canvas, lines: &[Line], x: f32, top: f32, leading: f32) -> f32 {
    let Some(first) = lines.first() else {
        return top;
    };
    let mut baseline = top + c.cap_height(first.style.font, first.style.size);
    for line in lines {
        c.draw(line, x, baseline);
        baseline += leading;
    }
    baseline - leading
}

// --- Default -----------------------------------------------------------------

/// The home page's decorative board, with real heads and tails.
fn home_board() -> BoardArt {
    BoardArt {
        width: 11,
        height: 11,
        snakes: vec![
            BoardSnake {
                body: vec![
                    (3, 8),
                    (3, 7),
                    (4, 7),
                    (4, 6),
                    (4, 5),
                    (5, 5),
                    (5, 4),
                    (5, 3),
                ],
                color: palette::pink(),
                head: shape(AssetKind::Head, "smile"),
                tail: shape(AssetKind::Tail, "curled"),
                eliminated: false,
            },
            BoardSnake {
                body: vec![(8, 3), (8, 4), (7, 4), (7, 5), (7, 6), (8, 6), (9, 6)],
                color: palette::ink(),
                head: shape(AssetKind::Head, "fang"),
                tail: shape(AssetKind::Tail, "sharp"),
                eliminated: false,
            },
        ],
        food: vec![(1, 9), (6, 8), (9, 1), (2, 2)],
        hazards: vec![],
    }
}

/// The site-wide card, for pages without one of their own.
pub fn default_card(tagline: &str) -> cja::Result<Vec<u8>> {
    let mut c = Canvas::new()?;
    backdrop(&mut c);
    c.board(&home_board(), BOARD_X, PAD, BOARD_SIDE);
    kicker(&mut c, None, "Battlesnake Arena");

    let headline = TextStyle::new(Font::Display, 84.0, palette::ink()).tracking(-0.03);
    let cap = c.cap_height(Font::Display, 84.0);
    let first = 196.0 + cap;
    c.text("Your code", headline, PAD - 4.0, first);
    c.text(
        "vs. everyone.",
        headline.color(palette::pink()),
        PAD - 4.0,
        first + 88.0,
    );

    let body = TextStyle::new(Font::Body, 26.0, palette::muted());
    let lines = c.fit_lines(tagline, body, COLUMN_WIDTH, 3, 22.0);
    draw_lines(&mut c, &lines, PAD, first + 140.0, 36.0);

    footer(&mut c);
    c.into_png()
}

// --- Game --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameStatus {
    Waiting,
    Live,
    Finished,
    Failed,
}

/// One snake in a card's roster.
#[derive(Debug, Clone)]
pub struct RosterSnake {
    pub name: String,
    pub owner: String,
    pub color: Color,
    pub head: Option<&'static RefShape>,
    /// Shown only on spoiler cards.
    pub placement: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct GameCard {
    pub status: GameStatus,
    /// "Standard", "Royale", …
    pub mode: String,
    pub width: i32,
    pub height: i32,
    /// Spoiler-free cards list snakes in join order; spoiler cards by placement.
    pub roster: Vec<RosterSnake>,
    pub board: BoardArt,
    /// Reveal the result: placements, the final board, and (solo) turns survived.
    pub spoilers: bool,
    /// Solo games: the final turn, shown on spoiler cards.
    pub turns: Option<i32>,
}

fn ordinal(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Roster row sizing by snake count: a fight poster for duels, a tighter
/// list for free-for-alls.
struct RowSpec {
    tile: f32,
    name: TextStyle,
    min_name: f32,
    /// Owner line under the name; compact rows have none.
    owner: Option<TextStyle>,
    gap: f32,
}

fn row_spec(count: usize) -> RowSpec {
    let display = |size| TextStyle::new(Font::Display, size, palette::ink()).tracking(-0.02);
    let owner = |size| Some(TextStyle::new(Font::Body, size, palette::muted()));
    match count {
        0..=1 => RowSpec {
            tile: 92.0,
            name: display(64.0),
            min_name: 34.0,
            owner: owner(24.0),
            gap: 0.0,
        },
        2 => RowSpec {
            tile: 76.0,
            name: display(52.0),
            min_name: 28.0,
            owner: owner(22.0),
            gap: 58.0,
        },
        3..=4 => RowSpec {
            tile: 58.0,
            name: display(38.0),
            min_name: 24.0,
            owner: owner(19.0),
            gap: 22.0,
        },
        _ => RowSpec {
            tile: 40.0,
            name: TextStyle::new(Font::Strong, 25.0, palette::ink()),
            min_name: 19.0,
            owner: None,
            gap: 14.0,
        },
    }
}

/// Most rows a roster shows; past that, the last row becomes "+N more".
const MAX_ROWS: usize = 6;

/// Height of the solo card's "survived" stat under its one roster row.
const SOLO_STAT_HEIGHT: f32 = 112.0;

/// The placement label at the right of a spoiler row ("1ST", "2ND", …).
fn placement_label(
    c: &Canvas,
    snake: &RosterSnake,
    spec: &RowSpec,
    spoilers: bool,
    solo: bool,
) -> Option<Line> {
    if !spoilers || solo {
        return None;
    }
    let winner = snake.placement == Some(1);
    let text = snake
        .placement
        .map_or_else(|| "—".to_string(), ordinal)
        .to_uppercase();
    let size = if spec.owner.is_some() { 20.0 } else { 18.0 };
    let color = if winner {
        palette::pink()
    } else {
        palette::muted()
    };
    Some(c.shape(
        &text,
        TextStyle::new(Font::Mono, size, color).tracking(0.06),
    ))
}

const NAME_X: f32 = 22.0;
const LABEL_GAP: f32 = 18.0;

/// Draw the roster centred in the text column, with `below` px of room kept
/// under it; returns the y where that room starts.
fn roster(c: &mut Canvas, snakes: &[RosterSnake], spoilers: bool, below: f32) -> f32 {
    let spec = row_spec(snakes.len());
    let solo = snakes.len() == 1;
    let (shown, hidden) = if snakes.len() > MAX_ROWS {
        (&snakes[..MAX_ROWS - 1], snakes.len() + 1 - MAX_ROWS)
    } else {
        (snakes, 0)
    };
    let rows = shown.len() + usize::from(hidden > 0);
    let height = rows as f32 * spec.tile + rows.saturating_sub(1) as f32 * spec.gap + below;
    let mut top = BODY_TOP + ((BODY_BOTTOM - BODY_TOP - height) / 2.0).max(0.0);

    // One name size for every row, so a duel reads as a matched pair: the
    // smallest size any name needs, but no smaller than 70% of full size
    // (longer names are cut short instead).
    let name_x = PAD + spec.tile + NAME_X;
    let floor = spec.min_name.max(spec.name.size * 0.7);
    let name_size = shown
        .iter()
        .map(|snake| {
            let label = placement_label(c, snake, &spec, spoilers, solo);
            let room = COLUMN_RIGHT - label.map_or(0.0, |l| l.width + LABEL_GAP) - name_x;
            c.fit_line(&snake.name, spec.name, room, floor).style.size
        })
        .fold(spec.name.size, f32::min);

    for (i, snake) in shown.iter().enumerate() {
        if i > 0 && snakes.len() == 2 {
            // Duel: "VS" centred under the first tile.
            let vs = TextStyle::new(Font::Mono, 20.0, palette::pink()).tracking(0.12);
            let line = c.shape("VS", vs);
            let cap = c.cap_height(Font::Mono, 20.0);
            c.draw(
                &line,
                PAD + (spec.tile - line.width) / 2.0,
                top - spec.gap / 2.0 + cap / 2.0,
            );
        }
        roster_row(c, snake, &spec, name_size, top, spoilers, solo);
        top += spec.tile + spec.gap;
    }
    if hidden > 0 {
        let more = TextStyle::new(Font::Strong, 22.0, palette::muted());
        let cap = c.cap_height(Font::Strong, 22.0);
        c.text(
            &format!("+ {hidden} more snakes"),
            more,
            name_x,
            top + spec.tile / 2.0 + cap / 2.0,
        );
        top += spec.tile + spec.gap;
    }
    top - spec.gap
}

fn roster_row(
    c: &mut Canvas,
    snake: &RosterSnake,
    spec: &RowSpec,
    name_size: f32,
    top: f32,
    spoilers: bool,
    solo: bool,
) {
    let winner = spoilers && !solo && snake.placement == Some(1);
    let dim = spoilers && !solo && !winner;
    c.head_tile(snake.head, snake.color, PAD, top, spec.tile);
    if winner {
        // A pink ring marks the winner's tile.
        let ring = round_rect(
            PAD - 5.0,
            top - 5.0,
            spec.tile + 10.0,
            spec.tile + 10.0,
            spec.tile * 0.3,
        );
        if let Some(ring) = ring {
            let stroke = Stroke {
                width: 3.0,
                ..Stroke::default()
            };
            c.stroke_path(
                &ring,
                &solid(palette::pink()),
                &stroke,
                Transform::identity(),
            );
        }
    }

    let mut right = COLUMN_RIGHT;
    let center = top + spec.tile / 2.0;
    if let Some(label) = placement_label(c, snake, spec, spoilers, solo) {
        let cap = c.cap_height(Font::Mono, label.style.size);
        c.draw(&label, right - label.width, center + cap / 2.0);
        right -= label.width + LABEL_GAP;
    }

    let x = PAD + spec.tile + NAME_X;
    let mut name_style = spec.name.size(name_size);
    if dim {
        name_style = name_style.color(with_alpha(palette::ink(), 0.62));
    }
    let name = c.ellipsize(&snake.name, name_style, right - x);
    let name_cap = c.cap_height(name.style.font, name.style.size);
    match spec.owner {
        Some(owner_style) => {
            let owner_cap = c.cap_height(owner_style.font, owner_style.size);
            let gap = name.style.size * 0.36;
            let block = name_cap + gap + owner_cap;
            let name_baseline = center - block / 2.0 + name_cap;
            c.draw(&name, x, name_baseline);
            let owner = c.ellipsize(&format!("by {}", snake.owner), owner_style, right - x);
            c.draw(&owner, x, name_baseline + gap + owner_cap);
        }
        None => c.draw(&name, x, center + name_cap / 2.0),
    }
}

/// Under a solo game's roster row: turns survived (spoilers) or a teaser.
fn solo_stat(c: &mut Canvas, top: f32, turns: Option<i32>) {
    let label = TextStyle::new(Font::Mono, 18.0, palette::muted()).tracking(0.08);
    let label_cap = c.cap_height(Font::Mono, 18.0);
    let label_baseline = top + 40.0 + label_cap;
    match turns {
        Some(turns) => {
            c.text("SURVIVED", label, PAD, label_baseline);
            let stat = TextStyle::new(Font::Display, 60.0, palette::pink()).tracking(-0.02);
            let unit = if turns == 1 { "turn" } else { "turns" };
            c.text(
                &format!("{turns} {unit}"),
                stat,
                PAD - 2.0,
                label_baseline + 14.0 + c.cap_height(Font::Display, 60.0),
            );
        }
        None => {
            c.text(
                "HOW LONG CAN IT SURVIVE?",
                label.color(palette::pink()),
                PAD,
                label_baseline,
            );
        }
    }
}

pub fn game_card(card: &GameCard) -> cja::Result<Vec<u8>> {
    let mut c = Canvas::new()?;
    backdrop(&mut c);
    c.board(&card.board, BOARD_X, PAD, BOARD_SIDE);

    let pill_label = match card.status {
        GameStatus::Live => ("LIVE", true),
        GameStatus::Waiting => ("UP NEXT", false),
        GameStatus::Finished => ("REPLAY", false),
        GameStatus::Failed => ("NO RESULT", false),
    };
    let kicker_text = if card.roster.len() > 2 {
        format!(
            "{} · {}×{} · {} snakes",
            card.mode,
            card.width,
            card.height,
            card.roster.len()
        )
    } else {
        format!("{} · {}×{}", card.mode, card.width, card.height)
    };
    kicker(&mut c, Some(pill_label), &kicker_text);

    match card.roster.len() {
        0 => {
            let style = TextStyle::new(Font::Display, 56.0, palette::ink()).tracking(-0.02);
            let lines = c.fit_lines("A Battlesnake game", style, COLUMN_WIDTH, 2, 40.0);
            draw_lines(&mut c, &lines, PAD, 240.0, 62.0);
        }
        1 => {
            let turns = card.turns.filter(|_| card.spoilers);
            let below = if turns.is_some() {
                SOLO_STAT_HEIGHT
            } else {
                SOLO_STAT_HEIGHT * 0.5
            };
            let bottom = roster(&mut c, &card.roster, card.spoilers, below);
            solo_stat(&mut c, bottom, turns);
        }
        _ => {
            roster(&mut c, &card.roster, card.spoilers, 0.0);
        }
    }

    footer(&mut c);
    c.into_png()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::og::canvas::rgb;

    fn decode(png: &[u8]) -> (u32, u32) {
        let reader = png::Decoder::new(std::io::Cursor::new(png.to_vec()))
            .read_info()
            .unwrap();
        (reader.info().width, reader.info().height)
    }

    fn snake(name: &str, color: Color, placement: Option<i32>) -> RosterSnake {
        RosterSnake {
            name: name.to_string(),
            owner: "coreyja".to_string(),
            color,
            head: shape(AssetKind::Head, "default"),
            placement,
        }
    }

    pub(crate) fn sample_game(count: usize, spoilers: bool) -> GameCard {
        let colors = [
            rgb(0xFF, 0x3D, 0x8A),
            rgb(0x3D, 0xDB, 0xA0),
            rgb(0xF5, 0xB7, 0x00),
            rgb(0x4F, 0x8B, 0xFF),
            rgb(0xB0, 0x6C, 0xFF),
            rgb(0xF4, 0xEF, 0xEA),
            rgb(0xFF, 0x6B, 0x6B),
            rgb(0x88, 0x88, 0x88),
        ];
        let names = [
            "Snek Alpha",
            "The Very Hungry Caterpillar Who Never Stops",
            "hissss",
            "Danger Noodle",
            "Nagini",
            "Kaa",
            "Ouroboros",
            "Jörmungandr",
        ];
        let roster = (0..count)
            .map(|i| snake(names[i % 8], colors[i % 8], Some(i as i32 + 1)))
            .collect();
        // Turn 2 of a standard start: each snake two moves out of its corner.
        let bodies: [Vec<(i32, i32)>; 4] = [
            vec![(1, 3), (1, 2), (1, 1)],
            vec![(9, 7), (9, 8), (9, 9)],
            vec![(3, 9), (2, 9), (1, 9)],
            vec![(7, 1), (8, 1), (9, 1)],
        ];
        let snakes = (0..count.min(4))
            .map(|i| BoardSnake {
                body: bodies[i].clone(),
                color: colors[i],
                head: shape(AssetKind::Head, ["default", "bendr", "smile", "pixel"][i]),
                tail: shape(AssetKind::Tail, ["default", "bolt", "curled", "pixel"][i]),
                eliminated: spoilers && i > 0,
            })
            .collect();
        GameCard {
            status: if spoilers {
                GameStatus::Finished
            } else {
                GameStatus::Live
            },
            mode: if count == 1 { "Solo" } else { "Standard" }.to_string(),
            width: 11,
            height: 11,
            roster,
            board: BoardArt {
                width: 11,
                height: 11,
                snakes,
                food: vec![(5, 5), (0, 4), (6, 10), (10, 6)],
                hazards: vec![],
            },
            spoilers,
            turns: Some(312),
        }
    }

    #[test]
    fn ordinals() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 101, 111]
            .map(ordinal)
            .into();
        assert_eq!(
            got,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd",
                "101st", "111th"
            ]
        );
    }

    #[test]
    fn default_card_renders() {
        let png = default_card("A competitive arena where your code battles other Battlesnakes.")
            .unwrap();
        assert_eq!(decode(&png), (WIDTH, HEIGHT));
    }

    #[test]
    fn game_cards_render_for_every_roster_size() {
        for count in 0..=9 {
            for spoilers in [false, true] {
                let png = game_card(&sample_game(count, spoilers)).unwrap();
                assert_eq!(decode(&png), (WIDTH, HEIGHT), "{count} snakes");
            }
        }
    }

    /// Writes every card variant to `$OG_PREVIEW_DIR` for eyeballing:
    /// `OG_PREVIEW_DIR=/tmp/og cargo test -p arena og::cards -- --ignored`
    #[test]
    #[ignore = "writes preview PNGs; run by hand"]
    fn write_previews() {
        let Ok(dir) = std::env::var("OG_PREVIEW_DIR") else {
            return;
        };
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        let write = |name: &str, png: Vec<u8>| std::fs::write(dir.join(name), png).unwrap();
        write(
            "default.png",
            default_card("A competitive arena where your code battles other Battlesnakes.")
                .unwrap(),
        );
        for count in [1, 2, 4, 8] {
            for spoilers in [false, true] {
                let suffix = if spoilers { "-spoilers" } else { "" };
                write(
                    &format!("game-{count}{suffix}.png"),
                    game_card(&sample_game(count, spoilers)).unwrap(),
                );
            }
        }
    }
}
