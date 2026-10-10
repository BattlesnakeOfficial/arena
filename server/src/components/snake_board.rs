//! Server-rendered Battlesnake board for the Head & Tail Studio.
//!
//! A 1:1 port of board.battlesnake.com's SVG renderer
//! (BattlesnakeOfficial/board@dac4e78: `Gameboard.svelte`, `SvgSnake*.svelte`,
//! `svg.ts`), by way of a reference port that matched the live board exactly:
//! identical polyline points and head/tail `x`/`y`/`transform` at six turns of a
//! real game, and 0 differing pixels in a screenshot diff. The unit tests replay
//! those goldens from `server/tests/fixtures/snake_board/goldens.json`.
//!
//! Deliberate differences from the real board:
//! - Colour comes from CSS, not `fill`/`stroke` attributes: a snake uses `--snake`
//!   (set per snake via [`SnakeSpec::color`]) and otherwise the page's
//!   `--studio-snake`. See the `page: studio` block in `static/arena.css`, which
//!   also holds the grid/background colours and the board's drop shadow.
//! - Heads and tails are a single `<path>` (the design kit's clean output) rather
//!   than arbitrary injected SVG.
//! - No hazards and no self-collision head shadow; the studio never shows them.
//! - Bodies the board would throw on (one cell, or the same cell twice in the
//!   middle) render without panicking.

use arena::design_kit::FillRule;
use maud::{Markup, html};

use crate::customizations::normalize_color;

/// A board cell `(x, y)`. As in the game API, `(0, 0)` is the bottom-left cell.
pub type Cell = (i32, i32);

/// Side of one cell, in board pixels.
pub const CELL: i32 = 20;
const HALF: f64 = 10.0;
const SPACING: i32 = 4;
const BORDER: i32 = 10;
/// How far the body pokes into the head and tail cells, so no seam shows.
const OVERLAP: f64 = 0.1;
/// `(CELL / 3.25).toFixed(2)` in the board source.
const FOOD_RADIUS: &str = "6.15";
const FOOD_FILL: &str = "#f43f5e";

/// A board to render: its size plus one frame (a still board) or several (a
/// loop the page's JS steps through).
#[derive(Debug, Clone)]
pub struct BoardSpec<'a> {
    pub width: i32,
    pub height: i32,
    /// With more than one frame, each renders as `<g class="studio-frame">` and
    /// every frame but the first also gets `hidden`.
    pub frames: Vec<Frame<'a>>,
    /// Extra classes for the root `<svg>`, after `studio-board` (e.g. `"light"`).
    pub class: &'a str,
    /// Accessible name. Non-empty gives `role="img"` + `aria-label`; empty marks
    /// the board `aria-hidden`.
    pub label: &'a str,
}

/// The snakes and food on the board at one moment.
#[derive(Debug, Clone, Default)]
pub struct Frame<'a> {
    pub snakes: Vec<SnakeSpec<'a>>,
    pub food: Vec<Cell>,
}

#[derive(Debug, Clone)]
pub struct SnakeSpec<'a> {
    /// Head first, tail last. Stacked tail cells (a fresh or just-fed snake)
    /// render like the real board does.
    pub body: Vec<Cell>,
    pub head: ShapeRef<'a>,
    pub tail: ShapeRef<'a>,
    /// Extra classes for the snake's `<g>`, after `snake`.
    pub class: &'a str,
    /// A per-snake colour, emitted as `style="--snake: #rrggbb"`. Anything that is
    /// not `#rrggbb` is dropped, so the snake falls back to `--studio-snake`.
    pub color: Option<&'a str>,
    /// Drawn first and at 10% opacity, as the board draws eliminated snakes.
    pub eliminated: bool,
}

/// A head or tail drawing: one path in the asset's 100×100 box.
#[derive(Debug, Clone, Copy)]
pub struct ShapeRef<'a> {
    pub d: &'a str,
    pub fill_rule: FillRule,
    /// Marks a placeholder `<path>` the page's JS fills in later, e.g.
    /// `"studio-head"`.
    pub class: Option<&'a str>,
}

impl<'a> SnakeSpec<'a> {
    /// A snake with no extra class or colour of its own.
    pub fn new(body: Vec<Cell>, head: ShapeRef<'a>, tail: ShapeRef<'a>) -> Self {
        Self {
            body,
            head,
            tail,
            class: "",
            color: None,
            eliminated: false,
        }
    }
}

/// Which way a head faces, or which way a tail's tip points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Right,
    Left,
    Up,
    Down,
}

impl Direction {
    pub const ALL: [Direction; 4] = [
        Direction::Right,
        Direction::Left,
        Direction::Up,
        Direction::Down,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Right => "right",
            Direction::Left => "left",
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }

    /// The head's `<g transform>`; head art faces right.
    pub fn head_transform(self) -> &'static str {
        match self {
            Direction::Right => "",
            Direction::Left => "scale(-1,1) translate(-100, 0)",
            Direction::Up => "rotate(-90, 50, 50)",
            Direction::Down => "rotate(90, 50, 50)",
        }
    }

    /// The tail's `<g transform>`; tail art joins the body on its left edge and
    /// points right.
    pub fn tail_transform(self) -> &'static str {
        match self {
            Direction::Right => "",
            Direction::Left => "scale(-1,1) translate(-100,0)",
            Direction::Down => "scale(-1,1) translate(-100,0) rotate(90, 50, 50)",
            Direction::Up => "scale(-1,1) translate(-100,0) rotate(-90, 50, 50)",
        }
    }
}

/// Where a head or tail goes: its cell's top-left corner in board pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub dir: Direction,
}

/// Everything about one snake that depends on the board, not on its art.
#[derive(Debug, Clone, PartialEq)]
pub struct SnakeGeometry {
    /// `None` when the tail sits on the head (a fresh snake).
    pub tail: Option<Placement>,
    /// Points (board pixels) of each body polyline: none for a fresh snake, several
    /// only on wrapped boards.
    pub polylines: Vec<Vec<(f64, f64)>>,
    pub head: Placement,
}

/// Render a board as an inline `<svg class="studio-board …">`.
///
/// Inside each `<g class="snake">` the order is tail, body polyline(s), head,
/// as on the real board; food is drawn last.
pub fn snake_board(spec: &BoardSpec) -> Markup {
    let board = Board::new(spec.width, spec.height);
    let class = join_class("studio-board", spec.class);
    let view_box = format!("0 0 {} {}", board.px_width, board.px_height);
    let label = (!spec.label.is_empty()).then_some(spec.label);
    html! {
        svg xmlns="http://www.w3.org/2000/svg" class=(class) viewBox=(view_box)
            role=[label.map(|_| "img")] aria-label=[label] aria-hidden=[label.is_none().then_some("true")] {
            g {
                @for x in 0..spec.width {
                    @for y in 0..spec.height {
                        @let (px, py) = board.top_left((x, y));
                        rect class="grid" x=(px) y=(py) width=(CELL) height=(CELL) {}
                    }
                }
            }
            @if let [frame] = spec.frames.as_slice() {
                (render_frame(&board, frame))
            } @else {
                @for (i, frame) in spec.frames.iter().enumerate() {
                    g class="studio-frame" hidden[i > 0] { (render_frame(&board, frame)) }
                }
            }
        }
    }
}

/// The board-dependent geometry of one snake, or `None` for an empty body.
pub fn snake_geometry(width: i32, height: i32, body: &[Cell]) -> Option<SnakeGeometry> {
    Board::new(width, height).snake(body)
}

fn render_frame(board: &Board, frame: &Frame) -> Markup {
    let eliminated = frame.snakes.iter().filter(|s| s.eliminated);
    let alive = frame.snakes.iter().filter(|s| !s.eliminated);
    html! {
        @for snake in eliminated.chain(alive) {
            (render_snake(board, snake))
        }
        @for &food in &frame.food {
            @let (cx, cy) = board.center(food);
            circle class="food" fill=(FOOD_FILL) r=(FOOD_RADIUS) cx=(num(cx)) cy=(num(cy)) {}
        }
    }
}

fn render_snake(board: &Board, snake: &SnakeSpec) -> Markup {
    let Some(geometry) = board.snake(&snake.body) else {
        return html! {};
    };
    let mut style = Vec::new();
    if let Some(color) = snake.color.map(normalize_color).filter(|c| !c.is_empty()) {
        style.push(format!("--snake: {color}"));
    }
    if snake.eliminated {
        style.push("opacity: 0.1".to_string());
    }
    let style = (!style.is_empty()).then(|| style.join("; "));
    let head = geometry.head;
    html! {
        g class=(join_class("snake", snake.class)) style=[style] {
            @if let Some(tail) = geometry.tail {
                svg class="tail" viewBox="0 0 100 100" x=(tail.x) y=(tail.y) width=(CELL) height=(CELL) {
                    g transform=(tail.dir.tail_transform()) { (shape(&snake.tail)) }
                }
            }
            @for points in &geometry.polylines {
                polyline fill="transparent" points=(points_attr(points)) stroke-width=(CELL)
                    stroke-linecap="butt" stroke-linejoin="round" {}
            }
            svg class={ "head " (head.dir.as_str()) } viewBox="0 0 100 100" x=(head.x) y=(head.y)
                width=(CELL) height=(CELL) {
                g transform=(head.dir.head_transform()) { (shape(&snake.head)) }
            }
        }
    }
}

fn shape(shape: &ShapeRef) -> Markup {
    html! {
        path class=[shape.class] d=(shape.d) fill-rule=(shape.fill_rule.as_svg()) {}
    }
}

fn join_class(base: &str, extra: &str) -> String {
    if extra.is_empty() {
        base.to_string()
    } else {
        format!("{base} {extra}")
    }
}

/// Format a coordinate exactly as JavaScript's `String(number)` does, so our
/// attributes are byte-identical to the real board's. Rust's `{}` prints the
/// same shortest round-trip digits for every value a board produces (checked
/// against node for centres -10000..=10000 and every joint offset); the one
/// difference is negative zero, which JS prints as "0". Adding `0.0` turns
/// `-0.0` into `0.0` and leaves every other value alone.
fn num(v: f64) -> String {
    format!("{}", v + 0.0)
}

fn points_attr(points: &[(f64, f64)]) -> String {
    points
        .iter()
        .map(|&(x, y)| format!("{},{}", num(x), num(y)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn adjacent(a: Cell, b: Cell) -> bool {
    (a.0 - b.0).abs() + (a.1 - b.1).abs() == 1
}

/// The cell just outside the board edge that `s` wraps across to reach `d`.
fn src_wrap(s: Cell, d: Cell) -> Cell {
    (s.0 - (d.0 - s.0).signum(), s.1 - (d.1 - s.1).signum())
}

/// The cell `s` would be in if it were adjacent to `d` on an unwrapped board.
fn dst_wrap(s: Cell, d: Cell) -> Cell {
    (d.0 + (d.0 - s.0).signum(), d.1 + (d.1 - s.1).signum())
}

/// A body point: the cell it belongs to, plus its pixel position (the cell's
/// centre, or a joint pushed toward a neighbour).
#[derive(Debug, Clone, Copy)]
struct Point {
    cell: Cell,
    cx: f64,
    cy: f64,
}

/// Move `s` from its cell centre to the edge facing `d`, plus `gap`. The board
/// throws when `d` is `s`'s own cell; we leave the point where it is.
fn joint(s: Point, d: Cell, gap: f64) -> Point {
    let mut j = s;
    if d.0 > s.cell.0 {
        j.cx = s.cx + HALF + gap;
    } else if d.0 < s.cell.0 {
        j.cx = s.cx - HALF - gap;
    } else if d.1 > s.cell.1 {
        j.cy = s.cy - HALF - gap;
    } else if d.1 < s.cell.1 {
        j.cy = s.cy + HALF + gap;
    }
    j
}

/// The joint on the board edge where the body wraps from `s` toward `d`.
fn border(s: Point, d: Cell) -> Point {
    joint(s, src_wrap(s.cell, d), 0.0)
}

fn head_direction(body: &[Cell]) -> Direction {
    let (Some(&head), Some(&neck)) = (body.first(), body.get(1)) else {
        return Direction::Right;
    };
    let neck = if adjacent(neck, head) {
        neck
    } else {
        dst_wrap(neck, head)
    };
    if head.0 < neck.0 {
        Direction::Left
    } else if head.1 > neck.1 {
        Direction::Up
    } else if head.1 < neck.1 {
        Direction::Down
    } else {
        Direction::Right
    }
}

/// Which way the tail tip points, judged from the last cell not stacked on it.
fn tail_direction(body: &[Cell]) -> Direction {
    let Some((&tail, rest)) = body.split_last() else {
        return Direction::Right;
    };
    let Some(&prev) = rest.iter().rev().find(|&&c| c != tail) else {
        return Direction::Right;
    };
    let prev = if adjacent(prev, tail) {
        prev
    } else {
        dst_wrap(prev, tail)
    };
    if prev.0 > tail.0 {
        Direction::Left
    } else if prev.1 > tail.1 {
        Direction::Down
    } else if prev.1 < tail.1 {
        Direction::Up
    } else {
        Direction::Right
    }
}

/// Board pixel geometry: the real board's constants and coordinate maths.
/// Social cards (`crate::og`) paint from the same numbers.
#[derive(Debug, Clone, Copy)]
pub struct Board {
    pub px_width: i32,
    pub px_height: i32,
}

impl Board {
    pub fn new(width: i32, height: i32) -> Self {
        let span = |n: i32| 2 * BORDER + n * CELL + (n - 1).max(0) * SPACING;
        Self {
            px_width: span(width),
            px_height: span(height),
        }
    }

    /// Top-left corner of a cell. Board `y` grows upward; SVG `y` grows down.
    pub fn top_left(&self, (x, y): Cell) -> (i32, i32) {
        (
            BORDER + x * (CELL + SPACING),
            self.px_height - (BORDER + y * (CELL + SPACING) + CELL),
        )
    }

    pub fn center(&self, cell: Cell) -> (f64, f64) {
        let (x, y) = self.top_left(cell);
        (f64::from(x) + HALF, f64::from(y) + HALF)
    }

    fn point(&self, cell: Cell) -> Point {
        let (cx, cy) = self.center(cell);
        Point { cell, cx, cy }
    }

    fn placement(&self, cell: Cell, dir: Direction) -> Placement {
        let (x, y) = self.top_left(cell);
        Placement { x, y, dir }
    }

    /// The board-dependent geometry of one snake, or `None` for an empty body.
    pub fn snake(&self, body: &[Cell]) -> Option<SnakeGeometry> {
        let (&head, &tail) = (body.first()?, body.last()?);
        Some(SnakeGeometry {
            tail: (head != tail).then(|| self.placement(tail, tail_direction(body))),
            polylines: self.polylines(body),
            head: self.placement(head, head_direction(body)),
        })
    }

    /// The body polyline(s): the centres of every cell between head and tail
    /// (stacked tail cells dropped), extended `HALF + SPACING + OVERLAP` into the
    /// head and tail cells. A wrapped body splits into one polyline per run.
    fn polylines(&self, body: &[Cell]) -> Vec<Vec<(f64, f64)>> {
        let Some((&head, rest)) = body.split_first() else {
            return Vec::new();
        };
        // A one-cell snake is its own tail.
        let (tail, mut middle) = match rest.split_last() {
            Some((&tail, middle)) => (tail, middle),
            None => (head, rest),
        };
        while let Some((&last, init)) = middle.split_last() {
            if last != tail {
                break;
            }
            middle = init;
        }
        if middle.is_empty() {
            return if head == tail {
                Vec::new()
            } else {
                vec![self.head_to_tail(head, tail)]
            };
        }

        let mut runs: Vec<Vec<Point>> = Vec::new();
        let mut prev: Option<Cell> = None;
        for &cell in middle {
            let point = self.point(cell);
            match runs.last_mut() {
                Some(run) if prev.is_some_and(|p| adjacent(p, cell)) => run.push(point),
                _ => runs.push(vec![point]),
            }
            prev = Some(cell);
        }
        // Where the body wraps, end each run on the board edge it leaves by and
        // start the next on the edge it enters by.
        for i in 0..runs.len() {
            if let (Some(&last), Some(next)) = (runs[i].last(), runs.get(i + 1))
                && let Some(next_first) = next.first()
            {
                let edge = border(last, next_first.cell);
                runs[i].push(edge);
            }
            if i > 0
                && let (Some(&first), Some(prev_last)) = (runs[i].first(), runs[i - 1].last())
            {
                let edge = border(first, prev_last.cell);
                runs[i].insert(0, edge);
            }
        }

        let gap = f64::from(SPACING) + OVERLAP;
        if let Some(run) = runs.first_mut()
            && let Some(&first) = run.first()
        {
            let into_head = if adjacent(head, first.cell) {
                joint(first, head, gap)
            } else {
                border(first, head)
            };
            run.insert(0, into_head);
        }
        if let Some(run) = runs.last_mut()
            && let Some(&last) = run.last()
        {
            let into_tail = if adjacent(last.cell, tail) {
                joint(last, tail, gap)
            } else {
                border(last, tail)
            };
            run.push(into_tail);
        }
        runs.into_iter()
            .map(|run| run.into_iter().map(|p| (p.cx, p.cy)).collect())
            .collect()
    }

    /// A two-cell snake (head plus tail, nothing between): a short stub that
    /// bridges the gap between the two cells.
    fn head_to_tail(&self, head: Cell, tail: Cell) -> Vec<(f64, f64)> {
        let (cx, cy) = self.center(head);
        let sp = f64::from(SPACING);
        if head.0 > tail.0 {
            vec![(cx - HALF + OVERLAP, cy), (cx - HALF - sp - OVERLAP, cy)]
        } else if head.0 < tail.0 {
            vec![(cx + HALF - OVERLAP, cy), (cx + HALF + sp + OVERLAP, cy)]
        } else if head.1 > tail.1 {
            vec![(cx, cy + HALF - OVERLAP), (cx, cy + HALF + sp + OVERLAP)]
        } else {
            vec![(cx, cy - HALF + OVERLAP), (cx, cy - HALF - sp - OVERLAP)]
        }
    }
}

// --- Studio layouts ---------------------------------------------------------

/// Size of the "All directions" board.
pub const FOUR_DIRECTIONS_SIZE: (i32, i32) = (7, 5);

/// Four snakes whose heads face right, left, up and down, and whose tails
/// point down, up, left and right: every head and tail transform on one board.
/// Verified against the real board's renderer.
pub const FOUR_DIRECTIONS_SNAKES: [[Cell; 4]; 4] = [
    [(2, 4), (1, 4), (0, 4), (0, 3)],
    [(4, 0), (5, 0), (6, 0), (6, 1)],
    [(5, 4), (5, 3), (5, 2), (4, 2)],
    [(1, 0), (1, 1), (1, 2), (2, 2)],
];

pub const FOUR_DIRECTIONS_FOOD: Cell = (3, 2);

/// Size of the "Live" board.
pub const LIVE_LOOP_SIZE: (i32, i32) = (7, 7);

/// The live loop's closed route in travel order: clockwise around a 5×5 ring
/// with a one-cell notch in its bottom-right corner. Every step is to an
/// adjacent cell, and the last cell is adjacent to the first. The ring gives
/// all four head directions and tail transforms; the notch adds a left turn
/// among the right turns.
pub const LIVE_LOOP_ROUTE: [Cell; 16] = [
    (1, 4),
    (1, 5),
    (2, 5),
    (3, 5),
    (4, 5),
    (5, 5),
    (5, 4),
    (5, 3),
    (4, 3),
    (4, 2),
    (4, 1),
    (3, 1),
    (2, 1),
    (1, 1),
    (1, 2),
    (1, 3),
];

/// Snake length in the live loop.
pub const LIVE_LOOP_SNAKE_LEN: usize = 5;

/// Food inside the ring, where the snake circles but never reaches it.
pub const LIVE_LOOP_FOOD: Cell = (3, 3);

/// The live loop's snake body (head first) in each of its frames. Frame `i`
/// has its tail on `LIVE_LOOP_ROUTE[i]`, so frame 0 heads right along the top
/// and the last frame steps back into frame 0.
pub fn live_loop_bodies() -> Vec<Vec<Cell>> {
    let n = LIVE_LOOP_ROUTE.len();
    (0..n)
        .map(|frame| {
            (0..LIVE_LOOP_SNAKE_LEN)
                .rev()
                .map(|k| LIVE_LOOP_ROUTE[(frame + k) % n])
                .collect()
        })
        .collect()
}

/// The "All directions" board with `head` and `tail` on all four snakes.
pub fn four_directions_board<'a>(
    head: ShapeRef<'a>,
    tail: ShapeRef<'a>,
    class: &'a str,
    label: &'a str,
) -> BoardSpec<'a> {
    let (width, height) = FOUR_DIRECTIONS_SIZE;
    BoardSpec {
        width,
        height,
        frames: vec![Frame {
            snakes: FOUR_DIRECTIONS_SNAKES
                .iter()
                .map(|body| SnakeSpec::new(body.to_vec(), head, tail))
                .collect(),
            food: vec![FOUR_DIRECTIONS_FOOD],
        }],
        class,
        label,
    }
}

/// The "Live" board: one frame per step of [`LIVE_LOOP_ROUTE`].
pub fn live_loop_board<'a>(
    head: ShapeRef<'a>,
    tail: ShapeRef<'a>,
    class: &'a str,
    label: &'a str,
) -> BoardSpec<'a> {
    let (width, height) = LIVE_LOOP_SIZE;
    BoardSpec {
        width,
        height,
        frames: live_loop_bodies()
            .into_iter()
            .map(|body| Frame {
                snakes: vec![SnakeSpec::new(body, head, tail)],
                food: vec![LIVE_LOOP_FOOD],
            })
            .collect(),
        class,
        label,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde::Deserialize;

    use super::*;

    const HEAD: ShapeRef<'static> = ShapeRef {
        d: "M0 100h100L56 55.39l44-39.89V.11L0 0zm12.52-80.71a9.26 9.26 0 1 1-9.26 9.26 9.26 9.26 0 0 1 9.26-9.26z",
        fill_rule: FillRule::NonZero,
        class: None,
    };
    const TAIL: ShapeRef<'static> = ShapeRef {
        d: "M50 0H0v100h50l50-50L50 0z",
        fill_rule: FillRule::NonZero,
        class: None,
    };

    // --- goldens --------------------------------------------------------------

    #[derive(Deserialize)]
    struct Golden {
        turn: Option<u32>,
        source: String,
        width: i32,
        height: i32,
        food: Vec<Cell>,
        snakes: Vec<GoldenSnake>,
        expected: Expected,
    }

    #[derive(Deserialize)]
    struct GoldenSnake {
        body: Vec<Cell>,
        eliminated: bool,
    }

    #[derive(Deserialize)]
    struct Expected {
        view_box: String,
        /// In draw order (eliminated snakes first).
        snakes: Vec<ExpectedSnake>,
        food: Vec<ExpectedFood>,
    }

    #[derive(Deserialize)]
    struct ExpectedSnake {
        eliminated: bool,
        tail: Option<ExpectedPlacement>,
        polylines: Vec<String>,
        head: ExpectedPlacement,
    }

    #[derive(Deserialize)]
    struct ExpectedPlacement {
        dir: Option<String>,
        x: String,
        y: String,
        transform: String,
    }

    #[derive(Deserialize)]
    struct ExpectedFood {
        cx: String,
        cy: String,
        r: String,
    }

    fn goldens() -> Vec<Golden> {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/snake_board/goldens.json"
        ))
        .unwrap()
    }

    fn golden_spec(g: &Golden) -> BoardSpec<'static> {
        BoardSpec {
            width: g.width,
            height: g.height,
            frames: vec![Frame {
                snakes: g
                    .snakes
                    .iter()
                    .map(|s| SnakeSpec {
                        eliminated: s.eliminated,
                        ..SnakeSpec::new(s.body.clone(), HEAD, TAIL)
                    })
                    .collect(),
                food: g.food.clone(),
            }],
            class: "",
            label: "",
        }
    }

    fn golden_name(g: &Golden) -> String {
        match g.turn {
            Some(turn) => format!("turn {turn} ({})", g.source),
            None => g.source.clone(),
        }
    }

    /// The board's draw order: eliminated snakes first, each group in input order.
    fn draw_order(g: &Golden) -> Vec<&GoldenSnake> {
        let (mut dead, alive): (Vec<_>, Vec<_>) = g.snakes.iter().partition(|s| s.eliminated);
        dead.extend(alive);
        dead
    }

    #[test]
    fn geometry_matches_goldens_exactly() {
        let goldens = goldens();
        assert_eq!(goldens.len(), 11);
        let mut snakes_checked = 0;
        for g in &goldens {
            let name = golden_name(g);
            let order = draw_order(g);
            assert_eq!(order.len(), g.expected.snakes.len(), "{name}");
            for (snake, want) in order.iter().zip(&g.expected.snakes) {
                assert_eq!(snake.eliminated, want.eliminated, "{name}");
                let got = snake_geometry(g.width, g.height, &snake.body).unwrap();
                let ctx = format!("{name}, body {:?}", snake.body);

                let got_polylines: Vec<String> =
                    got.polylines.iter().map(|line| points_attr(line)).collect();
                assert_eq!(got_polylines, want.polylines, "polylines: {ctx}");

                let head = &want.head;
                assert_eq!(got.head.x.to_string(), head.x, "head x: {ctx}");
                assert_eq!(got.head.y.to_string(), head.y, "head y: {ctx}");
                assert_eq!(Some(got.head.dir.as_str()), head.dir.as_deref(), "{ctx}");
                assert_eq!(got.head.dir.head_transform(), head.transform, "{ctx}");

                match (&got.tail, &want.tail) {
                    (None, None) => {}
                    (Some(got), Some(want)) => {
                        assert_eq!(got.x.to_string(), want.x, "tail x: {ctx}");
                        assert_eq!(got.y.to_string(), want.y, "tail y: {ctx}");
                        assert_eq!(got.dir.tail_transform(), want.transform, "{ctx}");
                    }
                    (got, want) => panic!(
                        "tail presence differs ({ctx}): got {got:?}, want {}",
                        want.is_some()
                    ),
                }
                snakes_checked += 1;
            }
        }
        assert_eq!(snakes_checked, 41);
    }

    /// Assert `needles` appear in `haystack` in this order.
    fn assert_in_order(haystack: &str, needles: &[String], ctx: &str) {
        let mut at = 0;
        for needle in needles {
            match haystack[at..].find(needle.as_str()) {
                Some(i) => at += i + needle.len(),
                None => panic!("{ctx}: missing (in order) {needle}\n--- markup ---\n{haystack}"),
            }
        }
    }

    #[test]
    fn markup_matches_goldens() {
        for g in &goldens() {
            let name = golden_name(g);
            let out = snake_board(&golden_spec(g)).into_string();

            assert!(
                out.contains(&format!("viewBox=\"{}\"", g.expected.view_box)),
                "{name}"
            );
            assert_eq!(
                out.matches("<rect class=\"grid\"").count(),
                (g.width * g.height) as usize,
                "{name}"
            );
            assert_eq!(
                out.matches("<g class=\"snake\"").count(),
                g.expected.snakes.len(),
                "{name}"
            );

            let mut needles = Vec::new();
            for snake in &g.expected.snakes {
                if snake.eliminated {
                    needles.push("<g class=\"snake\" style=\"opacity: 0.1\">".to_string());
                } else {
                    needles.push("<g class=\"snake\">".to_string());
                }
                if let Some(t) = &snake.tail {
                    needles.push(format!(
                        "<svg class=\"tail\" viewBox=\"0 0 100 100\" x=\"{}\" y=\"{}\" width=\"20\" height=\"20\"><g transform=\"{}\"><path ",
                        t.x, t.y, t.transform
                    ));
                }
                for points in &snake.polylines {
                    needles.push(format!(
                        "<polyline fill=\"transparent\" points=\"{points}\" stroke-width=\"20\" stroke-linecap=\"butt\" stroke-linejoin=\"round\"></polyline>"
                    ));
                }
                let h = &snake.head;
                needles.push(format!(
                    "<svg class=\"head {}\" viewBox=\"0 0 100 100\" x=\"{}\" y=\"{}\" width=\"20\" height=\"20\"><g transform=\"{}\"><path ",
                    h.dir.as_deref().unwrap_or_default(),
                    h.x,
                    h.y,
                    h.transform
                ));
                needles.push("</svg></g>".to_string());
            }
            for f in &g.expected.food {
                needles.push(format!(
                    "<circle class=\"food\" fill=\"#f43f5e\" r=\"{}\" cx=\"{}\" cy=\"{}\"></circle>",
                    f.r, f.cx, f.cy
                ));
            }
            needles.push("</svg>".to_string());
            assert_in_order(&out, &needles, &name);
            assert!(out.ends_with("</circle></svg>") || g.expected.food.is_empty());
        }
    }

    #[test]
    fn negative_zero_prints_like_javascript() {
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(92.0), "92");
        assert_eq!(num(82.0 - 10.0 - (4.0 + 0.1)), "67.9");
        assert_eq!(num(0.1 + 0.2), "0.30000000000000004");
    }

    // --- markup details ----------------------------------------------------------

    #[test]
    fn four_directions_snapshot() {
        let head = ShapeRef {
            class: Some("studio-head"),
            ..HEAD
        };
        let tail = ShapeRef {
            class: Some("studio-tail"),
            fill_rule: FillRule::EvenOdd,
            ..TAIL
        };
        let out = snake_board(&four_directions_board(
            head,
            tail,
            "light",
            "Your head facing right, left, up and down",
        ))
        .into_string();
        let want = include_str!("../../tests/fixtures/snake_board/four_directions.svg");
        assert_eq!(
            out,
            want.trim_end(),
            "four-direction board changed; if intended, update tests/fixtures/snake_board/four_directions.svg to:\n{out}"
        );
    }

    #[test]
    fn root_classes_and_label() {
        let spec = BoardSpec {
            width: 2,
            height: 1,
            frames: vec![Frame::default()],
            class: "dark studio-game-size",
            label: "Your head",
        };
        let out = snake_board(&spec).into_string();
        assert!(out.starts_with(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"studio-board dark studio-game-size\" viewBox=\"0 0 64 40\" role=\"img\" aria-label=\"Your head\">"
        ), "{out}");

        let unlabeled = snake_board(&BoardSpec {
            class: "",
            label: "",
            ..spec
        })
        .into_string();
        assert!(unlabeled.starts_with(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"studio-board\" viewBox=\"0 0 64 40\" aria-hidden=\"true\">"
        ), "{unlabeled}");
    }

    #[test]
    fn snake_colour_is_a_validated_css_variable() {
        let body = vec![(1, 0), (0, 0)];
        let render = |color: Option<&str>| {
            let spec = BoardSpec {
                width: 2,
                height: 1,
                frames: vec![Frame {
                    snakes: vec![SnakeSpec {
                        color,
                        class: "studio-pair",
                        ..SnakeSpec::new(body.clone(), HEAD, TAIL)
                    }],
                    food: vec![],
                }],
                class: "",
                label: "",
            };
            snake_board(&spec).into_string()
        };

        let out = render(Some("#FF4F86"));
        assert!(
            out.contains("<g class=\"snake studio-pair\" style=\"--snake: #ff4f86\">"),
            "{out}"
        );
        // No colour attributes anywhere: CSS owns snake colour.
        assert!(
            !out.contains("stroke=") && !out.contains(" fill=\"#"),
            "{out}"
        );

        for bad in [Some("red; background: url(x)"), Some("#ff4f8"), None] {
            let out = render(bad);
            assert!(
                out.contains("<g class=\"snake studio-pair\">"),
                "{bad:?}: {out}"
            );
            assert!(!out.contains("style="), "{bad:?}: {out}");
        }
    }

    #[test]
    fn eliminated_snakes_draw_first_and_faded() {
        let alive = SnakeSpec {
            color: Some("#00ff00"),
            ..SnakeSpec::new(vec![(0, 0), (0, 1)], HEAD, TAIL)
        };
        let dead = SnakeSpec {
            color: Some("#ff0000"),
            eliminated: true,
            ..SnakeSpec::new(vec![(2, 0), (2, 1)], HEAD, TAIL)
        };
        let out = snake_board(&BoardSpec {
            width: 3,
            height: 2,
            frames: vec![Frame {
                snakes: vec![alive, dead],
                food: vec![],
            }],
            class: "",
            label: "",
        })
        .into_string();
        let dead_at = out
            .find("style=\"--snake: #ff0000; opacity: 0.1\"")
            .unwrap();
        let alive_at = out.find("style=\"--snake: #00ff00\"").unwrap();
        assert!(dead_at < alive_at, "{out}");
    }

    #[test]
    fn shape_refs_render_placeholder_paths() {
        let head = ShapeRef {
            d: "M0 0H100V100H0Z",
            fill_rule: FillRule::EvenOdd,
            class: Some("studio-head"),
        };
        let out = snake_board(&four_directions_board(head, TAIL, "", "")).into_string();
        assert_eq!(
            out.matches(
                "<path class=\"studio-head\" d=\"M0 0H100V100H0Z\" fill-rule=\"evenodd\"></path>"
            )
            .count(),
            4
        );
        assert_eq!(
            out.matches(&format!(
                "<path d=\"{}\" fill-rule=\"nonzero\"></path>",
                TAIL.d
            ))
            .count(),
            4
        );
    }

    #[test]
    fn degenerate_bodies_do_not_panic() {
        assert_eq!(snake_geometry(3, 3, &[]), None);

        let one = snake_geometry(3, 3, &[(1, 1)]).unwrap();
        assert_eq!(one.tail, None);
        assert!(one.polylines.is_empty());
        assert_eq!(one.head.dir, Direction::Right);

        // Fresh snake: everything stacked on one cell.
        let fresh = snake_geometry(3, 3, &[(1, 1), (1, 1), (1, 1)]).unwrap();
        assert_eq!(fresh.tail, None);
        assert!(fresh.polylines.is_empty());

        // The board throws on a repeated middle cell; we render something sane.
        let odd = snake_geometry(5, 5, &[(0, 0), (1, 0), (1, 0), (2, 0), (3, 0)]).unwrap();
        assert_eq!(odd.head.dir, Direction::Left);
        assert_eq!(odd.polylines.len(), 2);

        let out = snake_board(&BoardSpec {
            width: 3,
            height: 3,
            frames: vec![Frame {
                snakes: vec![SnakeSpec::new(vec![], HEAD, TAIL)],
                food: vec![],
            }],
            class: "",
            label: "",
        })
        .into_string();
        assert!(!out.contains("class=\"snake"), "{out}");
    }

    // --- layouts ---------------------------------------------------------------

    fn in_bounds((width, height): (i32, i32), (x, y): Cell) -> bool {
        (0..width).contains(&x) && (0..height).contains(&y)
    }

    /// A valid snake on this board: adjacent consecutive cells and no cell used
    /// twice, except stacked cells at the tail.
    fn assert_valid_snake(size: (i32, i32), body: &[Cell]) {
        let mut unstacked = body.to_vec();
        while unstacked.len() > 1
            && unstacked[unstacked.len() - 1] == unstacked[unstacked.len() - 2]
        {
            unstacked.pop();
        }
        for pair in unstacked.windows(2) {
            assert!(adjacent(pair[0], pair[1]), "not adjacent: {body:?}");
        }
        let distinct: HashSet<_> = unstacked.iter().collect();
        assert_eq!(distinct.len(), unstacked.len(), "overlaps itself: {body:?}");
        assert!(
            body.iter().all(|&c| in_bounds(size, c)),
            "off board: {body:?}"
        );
    }

    #[test]
    fn four_directions_covers_every_direction_and_transform() {
        let heads: Vec<_> = FOUR_DIRECTIONS_SNAKES
            .iter()
            .map(|b| head_direction(b))
            .collect();
        assert_eq!(heads, Direction::ALL);
        let tails: HashSet<_> = FOUR_DIRECTIONS_SNAKES
            .iter()
            .map(|b| tail_direction(b))
            .collect();
        assert_eq!(tails, HashSet::from(Direction::ALL));

        let mut used = HashSet::new();
        for body in &FOUR_DIRECTIONS_SNAKES {
            assert_valid_snake(FOUR_DIRECTIONS_SIZE, body);
            assert!(body.iter().all(|c| used.insert(*c)), "snakes overlap");
        }
        assert!(!used.contains(&FOUR_DIRECTIONS_FOOD));
        assert!(in_bounds(FOUR_DIRECTIONS_SIZE, FOUR_DIRECTIONS_FOOD));
    }

    #[test]
    fn live_loop_is_a_valid_closed_loop() {
        let route = LIVE_LOOP_ROUTE;
        let distinct: HashSet<_> = route.iter().collect();
        assert_eq!(distinct.len(), route.len(), "route repeats a cell");
        for i in 0..route.len() {
            let next = route[(i + 1) % route.len()];
            assert!(adjacent(route[i], next), "route step {i} -> {next:?}");
        }

        let frames = live_loop_bodies();
        assert_eq!(frames.len(), 16);
        for (i, body) in frames.iter().enumerate() {
            assert_eq!(body.len(), LIVE_LOOP_SNAKE_LEN);
            assert_valid_snake(LIVE_LOOP_SIZE, body);
            assert!(!body.contains(&LIVE_LOOP_FOOD), "frame {i} covers the food");

            // Each frame is one move on from the one before, wrapping around.
            let next = &frames[(i + 1) % frames.len()];
            assert!(adjacent(body[0], next[0]), "frame {i}: head jumps");
            assert_eq!(&next[1..], &body[..body.len() - 1], "frame {i}: body jumps");
        }

        // The first frame heads right along the top.
        assert_eq!(head_direction(&frames[0]), Direction::Right);
        assert_eq!(frames[0][0], (4, 5));
    }

    #[test]
    fn live_loop_shows_every_head_direction_and_tail_transform() {
        let frames = live_loop_bodies();
        let heads: HashSet<_> = frames.iter().map(|b| head_direction(b)).collect();
        let tails: HashSet<_> = frames.iter().map(|b| tail_direction(b)).collect();
        assert_eq!(heads, HashSet::from(Direction::ALL));
        assert_eq!(tails, HashSet::from(Direction::ALL));

        // And so does the markup.
        let out = snake_board(&live_loop_board(HEAD, TAIL, "light", "")).into_string();
        for dir in Direction::ALL {
            assert!(out.contains(&format!("<svg class=\"head {}\"", dir.as_str())));
            assert!(out.contains(&format!("<g transform=\"{}\">", dir.tail_transform())));
        }
    }

    #[test]
    fn live_loop_shows_only_the_first_frame() {
        let out = snake_board(&live_loop_board(HEAD, TAIL, "", "")).into_string();
        assert_eq!(out.matches("<g class=\"studio-frame\"").count(), 16);
        assert_eq!(out.matches("<g class=\"studio-frame\" hidden>").count(), 15);
        let first = out.find("<g class=\"studio-frame\"").unwrap();
        assert!(out[first..].starts_with("<g class=\"studio-frame\"><g class=\"snake\">"));
        assert_eq!(out.matches("<circle class=\"food\"").count(), 16);
        assert_eq!(out.matches("<rect class=\"grid\"").count(), 49);
    }

    /// Writes a standalone sample board for eyeballing (`rsvg-convert` it). The
    /// studio CSS comes from arena.css; `rsvg` doesn't resolve custom
    /// properties, so the snake colour is substituted in.
    ///
    /// `SNAKE_BOARD_SAMPLE=/tmp/sample.svg cargo test --bin arena render_sample -- --ignored`
    #[test]
    #[ignore = "writes a file; run by hand"]
    fn render_sample() {
        let path = std::env::var("SNAKE_BOARD_SAMPLE").unwrap();
        let css = include_str!("../../static/arena.css");
        let start = css.find("/* --- page: studio").unwrap();
        let end = css[start + 1..]
            .find("/* ---")
            .map_or(css.len(), |i| start + 1 + i);
        let css = css[start..end].replace("var(--snake, var(--studio-snake, #ff4f86))", "#ff4f86");

        let board = |spec: BoardSpec| snake_board(&spec).into_string();
        // Nested <svg>s laid out on one sheet. rsvg ignores CSS `background` on
        // an <svg>, so paint the page colour behind each board.
        let place = |svg: &str, x: i32, y: i32, w: i32, h: i32| {
            let page = if svg.contains("studio-board dark") {
                "#0f0b19"
            } else {
                "#ffffff"
            };
            let board = svg.replacen(
                "<svg ",
                &format!("<svg x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" "),
                1,
            );
            format!(
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{page}\"/>{board}"
            )
        };

        // Row 1: the four-direction board, light and dark.
        let mut sheet = place(
            &board(four_directions_board(HEAD, TAIL, "light", "")),
            0,
            0,
            368,
            272,
        );
        sheet += &place(
            &board(four_directions_board(HEAD, TAIL, "dark", "")),
            384,
            0,
            368,
            272,
        );
        // Rows 2-3: every live-loop frame, in order.
        let live = live_loop_board(HEAD, TAIL, "light", "");
        for (i, frame) in live.frames.iter().enumerate() {
            let one = BoardSpec {
                frames: vec![frame.clone()],
                ..live.clone()
            };
            let (col, row) = (i as i32 % 8, i as i32 / 8);
            sheet += &place(&board(one), col * 128, 288 + row * 128, 120, 120);
        }
        let out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1024 544\" width=\"1024\" height=\"544\"><style><![CDATA[{css}]]></style>{sheet}</svg>"
        );
        std::fs::write(path, out).unwrap();
    }
}
