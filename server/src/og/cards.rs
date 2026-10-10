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
        ring(c, PAD, top, spec.tile);
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

// --- Profiles: snakes, leaderboard entries, players --------------------------

/// A labelled number in a profile's stat row.
#[derive(Debug, Clone)]
pub struct Stat {
    pub label: String,
    pub value: String,
}

impl Stat {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProfileCard {
    pub kicker: String,
    pub title: String,
    pub subtitle: String,
    /// A head tile beside the title (snakes); players have none.
    pub avatar: Option<(Option<&'static RefShape>, Color)>,
    /// At most three are shown.
    pub stats: Vec<Stat>,
    /// A pink line above the stats, e.g. "#3 on Standard 11x11".
    pub highlight: Option<String>,
    pub board: BoardArt,
}

const STATS_LABEL_BASELINE: f32 = 432.0;

/// A snake's portrait: it winds across a small board toward a snack.
pub fn portrait_board(color: Color, head: &str, tail: &str) -> BoardArt {
    BoardArt {
        width: 7,
        height: 7,
        snakes: vec![BoardSnake {
            body: vec![
                (4, 5),
                (3, 5),
                (2, 5),
                (1, 5),
                (1, 4),
                (1, 3),
                (2, 3),
                (3, 3),
                (4, 3),
                (5, 3),
                (5, 2),
                (5, 1),
                (4, 1),
                (3, 1),
            ],
            color,
            head: shape(AssetKind::Head, head),
            tail: shape(AssetKind::Tail, tail),
            eliminated: false,
        }],
        food: vec![(5, 5)],
        hazards: vec![],
    }
}

/// A player's snakes (up to four), lined up across the board like a starting
/// grid, centred top to bottom, with a snack in front of the first.
pub fn lineup_board(snakes: &[(Color, &str, &str)]) -> BoardArt {
    const LENGTHS: [i32; 4] = [7, 5, 6, 4];
    let shown = snakes.len().min(LENGTHS.len()) as i32;
    // Rows two apart, centred on the middle row (4).
    let rows = (0..shown).map(|i| 4 + (shown - 1) - 2 * i);
    let lineup: Vec<BoardSnake> = snakes
        .iter()
        .zip(LENGTHS)
        .zip(rows)
        .map(|((&(color, head, tail), len), y)| BoardSnake {
            body: (1..=len).rev().map(|x| (x, y)).collect(),
            color,
            head: shape(AssetKind::Head, head),
            tail: shape(AssetKind::Tail, tail),
            eliminated: false,
        })
        .collect();
    let mut food = vec![(3, 0), (6, 8)];
    if let Some(first) = lineup.first().and_then(|s| s.body.first()) {
        food.push((first.0 + 1, first.1));
    }
    BoardArt {
        width: 9,
        height: 9,
        snakes: lineup,
        food,
        hazards: vec![],
    }
}

fn stats_row(c: &mut Canvas, stats: &[Stat]) {
    let shown = &stats[..stats.len().min(3)];
    if shown.is_empty() {
        return;
    }
    let column = COLUMN_WIDTH / shown.len() as f32;
    let label = TextStyle::new(Font::Mono, 15.0, palette::muted()).tracking(0.08);
    let value = TextStyle::new(Font::Display, 46.0, palette::ink()).tracking(-0.02);
    let value_baseline = STATS_LABEL_BASELINE + 12.0 + c.cap_height(Font::Display, 46.0);
    for (i, stat) in shown.iter().enumerate() {
        let x = PAD + i as f32 * column;
        let room = column - 18.0;
        let line = c.ellipsize(&stat.label.to_uppercase(), label, room);
        c.draw(&line, x, STATS_LABEL_BASELINE);
        let line = c.fit_line(&stat.value, value, room, 28.0);
        c.draw(&line, x - 2.0, value_baseline);
    }
}

pub fn profile_card(card: &ProfileCard) -> cja::Result<Vec<u8>> {
    let mut c = Canvas::new()?;
    backdrop(&mut c);
    c.board(&card.board, BOARD_X, PAD, BOARD_SIDE);
    kicker(&mut c, None, &card.kicker);

    let tile = 84.0;
    let x = if card.avatar.is_some() {
        PAD + tile + 24.0
    } else {
        PAD
    };
    let title = TextStyle::new(Font::Display, 66.0, palette::ink()).tracking(-0.03);
    let lines = c.fit_lines(&card.title, title, COLUMN_RIGHT - x, 2, 38.0);
    let size = lines.first().map_or(66.0, |l| l.style.size);
    let leading = size * 1.04;
    let subtitle = TextStyle::new(Font::Body, 24.0, palette::muted());
    let sub_cap = c.cap_height(Font::Body, 24.0);
    let title_cap = c.cap_height(Font::Display, size);
    let block = title_cap + leading * lines.len().saturating_sub(1) as f32 + 18.0 + sub_cap;

    // Centre the title block between the kicker and the highlight/stats.
    let bottom = if card.highlight.is_some() {
        330.0
    } else {
        380.0
    };
    let top = BODY_TOP + ((bottom - BODY_TOP - block) / 2.0).max(0.0);
    if let Some((head, color)) = card.avatar {
        c.head_tile(head, color, PAD, top + block / 2.0 - tile / 2.0, tile);
    }
    let last_baseline = draw_lines(&mut c, &lines, x - 2.0, top, leading);
    let sub = c.ellipsize(&card.subtitle, subtitle, COLUMN_RIGHT - x);
    c.draw(&sub, x, last_baseline + 18.0 + sub_cap);

    if let Some(highlight) = &card.highlight {
        let style = TextStyle::new(Font::Mono, 19.0, palette::pink()).tracking(0.04);
        let line = c.ellipsize(highlight, style, COLUMN_WIDTH);
        c.draw(&line, PAD, 372.0);
    }
    stats_row(&mut c, &card.stats);

    footer(&mut c);
    c.into_png()
}

// --- Leaderboards ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LadderRow {
    pub rank: i64,
    pub snake: RosterSnake,
    pub rating: String,
}

#[derive(Debug, Clone)]
pub struct LeaderboardCard {
    pub kicker: String,
    pub title: String,
    /// At most three are shown, under the title.
    pub stats: Vec<Stat>,
    /// The top of the ladder; at most five are shown.
    pub rows: Vec<LadderRow>,
}

/// Inner padding of a list panel.
const PANEL_PAD: f32 = 34.0;

/// A big title and a muted subtitle, from `top`; returns the last baseline.
fn title_block(c: &mut Canvas, title: &str, subtitle: &str, top: f32) -> f32 {
    let style = TextStyle::new(Font::Display, 66.0, palette::ink()).tracking(-0.03);
    let lines = c.fit_lines(title, style, COLUMN_WIDTH, 3, 40.0);
    let size = lines.first().map_or(66.0, |l| l.style.size);
    let last = draw_lines(c, &lines, PAD - 2.0, top, size * 1.04);
    let body = TextStyle::new(Font::Body, 24.0, palette::muted());
    let sub = c.fit_lines(subtitle, body, COLUMN_WIDTH, 2, 20.0);
    draw_lines(c, &sub, PAD, last + 26.0, 32.0)
}

/// Muted text centred in the right-hand panel, for an empty list.
fn panel_note(c: &mut Canvas, text: &str) {
    let style = TextStyle::new(Font::Strong, 24.0, palette::muted());
    let line = c.shape(text, style);
    let cap = c.cap_height(Font::Strong, 24.0);
    c.draw(
        &line,
        BOARD_X + (BOARD_SIDE - line.width) / 2.0,
        PAD + BOARD_SIDE / 2.0 + cap / 2.0,
    );
}

pub fn leaderboard_card(card: &LeaderboardCard) -> cja::Result<Vec<u8>> {
    let mut c = Canvas::new()?;
    backdrop(&mut c);
    kicker(&mut c, None, &card.kicker);
    let style = TextStyle::new(Font::Display, 72.0, palette::ink()).tracking(-0.03);
    let lines = c.fit_lines(&card.title, style, COLUMN_WIDTH, 3, 40.0);
    let size = lines.first().map_or(72.0, |l| l.style.size);
    let leading = size * 1.04;
    let block = c.cap_height(Font::Display, size) + leading * lines.len().saturating_sub(1) as f32;
    let top = BODY_TOP + ((370.0 - BODY_TOP - block) / 2.0).max(0.0);
    draw_lines(&mut c, &lines, PAD - 2.0, top, leading);
    stats_row(&mut c, &card.stats);

    c.panel(BOARD_X, PAD, BOARD_SIDE);
    let inner_x = BOARD_X + PANEL_PAD;
    let inner_right = BOARD_X + BOARD_SIDE - PANEL_PAD;
    let header = TextStyle::new(Font::Mono, 16.0, palette::muted()).tracking(0.1);
    let header_baseline = PAD + PANEL_PAD + c.cap_height(Font::Mono, 16.0);
    c.text("TOP OF THE LADDER", header, inner_x, header_baseline);

    let rows = &card.rows[..card.rows.len().min(5)];
    if rows.is_empty() {
        panel_note(&mut c, "No ranked snakes yet");
    }
    let row_height = 80.0;
    let tile = 50.0;
    let name_style = TextStyle::new(Font::Strong, 24.0, palette::ink());
    let owner_style = TextStyle::new(Font::Body, 17.0, palette::muted());
    let name_cap = c.cap_height(Font::Strong, 24.0);
    let owner_cap = c.cap_height(Font::Body, 17.0);
    let mut top = header_baseline + 26.0;
    for row in rows {
        let center = top + row_height / 2.0;
        let rank_color = if row.rank == 1 {
            palette::pink()
        } else {
            palette::muted()
        };
        let rank = TextStyle::new(Font::Mono, 22.0, rank_color);
        let cap = c.cap_height(Font::Mono, 22.0);
        c.text(&row.rank.to_string(), rank, inner_x, center + cap / 2.0);

        let tile_x = inner_x + 48.0;
        c.head_tile(
            row.snake.head,
            row.snake.color,
            tile_x,
            center - tile / 2.0,
            tile,
        );

        let rating = TextStyle::new(Font::Mono, 20.0, palette::ink());
        let rating_cap = c.cap_height(Font::Mono, 20.0);
        let rating_width =
            c.text_right(&row.rating, rating, inner_right, center + rating_cap / 2.0);

        let x = tile_x + tile + 18.0;
        let room = inner_right - rating_width - 16.0 - x;
        let name = c.ellipsize(&row.snake.name, name_style, room);
        let owner = c.ellipsize(&format!("by {}", row.snake.owner), owner_style, room);
        let block = name_cap + 10.0 + owner_cap;
        let baseline = center - block / 2.0 + name_cap;
        c.draw(&name, x, baseline);
        c.draw(&owner, x, baseline + 10.0 + owner_cap);
        top += row_height;
    }

    footer(&mut c);
    c.into_png()
}

// --- Tournaments -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Entrant {
    pub color: Color,
    pub head: Option<&'static RefShape>,
    pub champion: bool,
}

#[derive(Debug, Clone)]
pub struct TournamentCard {
    /// The status pill: its label, and whether it's the filled (live) style.
    pub status: (String, bool),
    pub kicker: String,
    pub title: String,
    pub subtitle: String,
    pub champion: Option<RosterSnake>,
    pub entrants: Vec<Entrant>,
}

/// Most head tiles the entrant grid shows (6×6); past that, the last tile
/// becomes "+N".
const MAX_ENTRANTS: usize = 36;

fn entrant_grid(c: &mut Canvas, entrants: &[Entrant]) {
    if entrants.is_empty() {
        panel_note(c, "No snakes registered yet");
        return;
    }
    let inner = BOARD_SIDE - 2.0 * PANEL_PAD;
    let count = entrants.len().min(MAX_ENTRANTS);
    let cols = (count as f32).sqrt().ceil();
    let rows = (count as f32 / cols).ceil();
    let gap = 16.0;
    let tile = ((inner - (cols - 1.0) * gap) / cols).min(120.0);
    let grid_w = cols * tile + (cols - 1.0) * gap;
    let grid_h = rows * tile + (rows - 1.0) * gap;
    let x0 = BOARD_X + (BOARD_SIDE - grid_w) / 2.0;
    let y0 = PAD + (BOARD_SIDE - grid_h) / 2.0;
    // Entrants past the grid, counting the tile the "+N" replaces.
    let overflow = entrants.len() + 1 - count;
    for (i, entrant) in entrants.iter().take(count).enumerate() {
        let x = x0 + (i as f32 % cols) * (tile + gap);
        let y = y0 + (i as f32 / cols).floor() * (tile + gap);
        if entrants.len() > MAX_ENTRANTS && i == count - 1 {
            c.round_rect(x, y, tile, tile, tile * 0.24, palette::card());
            let style = TextStyle::new(Font::Mono, (tile * 0.3).min(26.0), palette::muted());
            let line = c.shape(&format!("+{overflow}"), style);
            let cap = c.cap_height(Font::Mono, style.size);
            c.draw(
                &line,
                x + (tile - line.width) / 2.0,
                y + tile / 2.0 + cap / 2.0,
            );
            continue;
        }
        c.head_tile(entrant.head, entrant.color, x, y, tile);
        if entrant.champion {
            ring(c, x, y, tile);
        }
    }
}

/// The pink ring that marks a winner's tile.
fn ring(c: &mut Canvas, x: f32, y: f32, tile: f32) {
    if let Some(ring) = round_rect(x - 5.0, y - 5.0, tile + 10.0, tile + 10.0, tile * 0.3) {
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

pub fn tournament_card(card: &TournamentCard) -> cja::Result<Vec<u8>> {
    let mut c = Canvas::new()?;
    backdrop(&mut c);
    kicker(
        &mut c,
        Some((card.status.0.as_str(), card.status.1)),
        &card.kicker,
    );
    let top = if card.champion.is_some() {
        140.0
    } else {
        168.0
    };
    title_block(&mut c, &card.title, &card.subtitle, top);

    if let Some(champion) = &card.champion {
        let label = TextStyle::new(Font::Mono, 16.0, palette::pink()).tracking(0.1);
        c.text("CHAMPION", label, PAD, 404.0);
        let tile = 64.0;
        let tile_y = 424.0;
        c.head_tile(champion.head, champion.color, PAD, tile_y, tile);
        ring(&mut c, PAD, tile_y, tile);
        let x = PAD + tile + 20.0;
        let name_style = TextStyle::new(Font::Display, 34.0, palette::ink()).tracking(-0.02);
        let name = c.fit_line(&champion.name, name_style, COLUMN_RIGHT - x, 24.0);
        let owner_style = TextStyle::new(Font::Body, 19.0, palette::muted());
        let owner = c.ellipsize(
            &format!("by {}", champion.owner),
            owner_style,
            COLUMN_RIGHT - x,
        );
        let name_cap = c.cap_height(Font::Display, name.style.size);
        let owner_cap = c.cap_height(Font::Body, 19.0);
        let block = name_cap + 12.0 + owner_cap;
        let baseline = tile_y + tile / 2.0 - block / 2.0 + name_cap;
        c.draw(&name, x, baseline);
        c.draw(&owner, x, baseline + 12.0 + owner_cap);
    }

    c.panel(BOARD_X, PAD, BOARD_SIDE);
    entrant_grid(&mut c, &card.entrants);

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

    pub(crate) fn sample_profile(long: bool, avatar: bool) -> ProfileCard {
        let pink = rgb(0xFF, 0x3D, 0x8A);
        ProfileCard {
            kicker: "Battlesnake".to_string(),
            title: if long {
                "The Very Hungry Caterpillar Who Never Stops Eating Ever".to_string()
            } else {
                "Hovering Hobbs".to_string()
            },
            subtitle: "by coreyja".to_string(),
            avatar: avatar.then(|| (shape(AssetKind::Head, "bendr"), pink)),
            stats: vec![
                Stat::new("Games", "1,204"),
                Stat::new("Wins", "377"),
                Stat::new("Win rate", "31.3%"),
            ],
            highlight: Some("#3 on Standard 11x11".to_string()),
            board: portrait_board(pink, "bendr", "bolt"),
        }
    }

    pub(crate) fn sample_leaderboard(rows: usize) -> LeaderboardCard {
        LeaderboardCard {
            kicker: "Leaderboard · Standard · 11x11".to_string(),
            title: "Standard 11x11".to_string(),
            stats: vec![
                Stat::new("Ranked snakes", "142"),
                Stat::new("Games", "98,311"),
                Stat::new("Live now", "3"),
            ],
            rows: sample_game(8, true)
                .roster
                .into_iter()
                .take(rows)
                .enumerate()
                .map(|(i, snake)| LadderRow {
                    rank: i as i64 + 1,
                    snake,
                    rating: format!("{:.1}", 41.2 - i as f64 * 2.7),
                })
                .collect(),
        }
    }

    pub(crate) fn sample_tournament(entrants: usize, champion: bool) -> TournamentCard {
        let roster = sample_game(8, true).roster;
        TournamentCard {
            status: if champion {
                ("COMPLETE".to_string(), false)
            } else {
                ("ROUND 2 OF 4".to_string(), true)
            },
            kicker: "Tournament · Standard · 11×11".to_string(),
            title: "Battlesnake Fall League 2026".to_string(),
            subtitle: format!("{entrants} snakes · Best of 3"),
            champion: champion.then(|| roster[0].clone()),
            entrants: (0..entrants)
                .map(|i| Entrant {
                    color: roster[i % roster.len()].color,
                    head: shape(
                        AssetKind::Head,
                        ["default", "bendr", "smile", "pixel"][i % 4],
                    ),
                    champion: champion && i == 0,
                })
                .collect(),
        }
    }

    #[test]
    fn profile_leaderboard_and_tournament_cards_render() {
        for (long, avatar) in [(false, true), (true, true), (true, false)] {
            let png = profile_card(&sample_profile(long, avatar)).unwrap();
            assert_eq!(decode(&png), (WIDTH, HEIGHT));
        }
        let mut bare = sample_profile(false, false);
        bare.stats.clear();
        bare.highlight = None;
        bare.board = lineup_board(&[]);
        assert_eq!(decode(&profile_card(&bare).unwrap()), (WIDTH, HEIGHT));
        for rows in [0, 3, 8] {
            let png = leaderboard_card(&sample_leaderboard(rows)).unwrap();
            assert_eq!(decode(&png), (WIDTH, HEIGHT), "{rows} rows");
        }
        for entrants in [0, 1, 2, 7, 16, 36, 37, 100] {
            for champion in [false, true] {
                let png = tournament_card(&sample_tournament(
                    entrants.max(usize::from(champion)),
                    champion,
                ))
                .unwrap();
                assert_eq!(decode(&png), (WIDTH, HEIGHT), "{entrants} entrants");
            }
        }
    }

    #[test]
    fn lineup_shows_at_most_four_snakes_heads_right() {
        let snakes: Vec<(Color, &str, &str)> = (0..6)
            .map(|_| (rgb(1, 2, 3), "default", "default"))
            .collect();
        let board = lineup_board(&snakes);
        assert_eq!(board.snakes.len(), 4);
        for snake in &board.snakes {
            // Head first, at the right-hand end.
            let (head, neck) = (snake.body[0], snake.body[1]);
            assert_eq!((head.0 - neck.0, head.1), (1, neck.1));
        }
        assert!(lineup_board(&[]).snakes.is_empty());
        let rows = |n: usize| -> Vec<i32> {
            lineup_board(&snakes[..n])
                .snakes
                .iter()
                .map(|s| s.body[0].1)
                .collect()
        };
        assert_eq!(rows(1), [4]);
        assert_eq!(rows(2), [5, 3]);
        assert_eq!(rows(4), [7, 5, 3, 1]);
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
        write(
            "snake.png",
            profile_card(&sample_profile(false, true)).unwrap(),
        );
        write(
            "snake-long.png",
            profile_card(&sample_profile(true, true)).unwrap(),
        );
        let player = ProfileCard {
            kicker: "Player".to_string(),
            title: "Corey Alexander".to_string(),
            subtitle: "@coreyja".to_string(),
            avatar: None,
            stats: vec![
                Stat::new("Snakes", "3"),
                Stat::new("Rating", "71.4"),
                Stat::new("Ladders", "2"),
            ],
            highlight: None,
            board: lineup_board(&[
                (rgb(0xFF, 0x3D, 0x8A), "smile", "curled"),
                (rgb(0x3D, 0xDB, 0xA0), "bendr", "bolt"),
                (rgb(0xF5, 0xB7, 0x00), "pixel", "pixel"),
            ]),
        };
        write("player.png", profile_card(&player).unwrap());
        write(
            "leaderboard.png",
            leaderboard_card(&sample_leaderboard(5)).unwrap(),
        );
        write(
            "tournament-live.png",
            tournament_card(&sample_tournament(16, false)).unwrap(),
        );
        write(
            "tournament-done.png",
            tournament_card(&sample_tournament(8, true)).unwrap(),
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
