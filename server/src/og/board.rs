//! Battlesnake boards and head tiles for social cards, painted from the same
//! geometry as the Studio's SVG board (`components::snake_board`).
//!
//! Heads and tails come from the design kit's reference table, which holds the
//! Standard set; any other catalog cosmetic is drawn as the default shape.

use arena::design_kit::{
    AssetKind, FillRule,
    refs::{REFS, RefShape},
};
use svgtypes::{SimplePathSegment, SimplifyingPathParser};
use tiny_skia::{
    Color, FillRule as SkiaFillRule, LineCap, LineJoin, Path, PathBuilder, Stroke, Transform,
};

use super::canvas::{Canvas, palette, round_rect, shaded, solid, vertical_gradient, with_alpha};
use crate::components::snake_board::{Board, CELL, Cell, Direction, Placement};

/// A head or tail cosmetic by slug, falling back to the default shape.
pub fn shape(kind: AssetKind, slug: &str) -> Option<&'static RefShape> {
    let of_kind = || REFS.iter().filter(move |r| r.kind == kind);
    of_kind()
        .find(|r| r.slug == slug)
        .or_else(|| of_kind().find(|r| r.slug == crate::customizations::DEFAULT_SLUG))
}

/// A reference shape's path, in its 100×100 box.
fn shape_path(shape: &RefShape) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for segment in SimplifyingPathParser::from(shape.d) {
        match segment.ok()? {
            SimplePathSegment::MoveTo { x, y } => pb.move_to(x as f32, y as f32),
            SimplePathSegment::LineTo { x, y } => pb.line_to(x as f32, y as f32),
            SimplePathSegment::Quadratic { x1, y1, x, y } => {
                pb.quad_to(x1 as f32, y1 as f32, x as f32, y as f32);
            }
            SimplePathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => pb.cubic_to(
                x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
            ),
            SimplePathSegment::ClosePath => pb.close(),
        }
    }
    pb.finish()
}

fn fill_rule(rule: FillRule) -> SkiaFillRule {
    match rule {
        FillRule::NonZero => SkiaFillRule::Winding,
        FillRule::EvenOdd => SkiaFillRule::EvenOdd,
    }
}

/// `Direction::head_transform` as a matrix: head art faces right.
fn head_transform(dir: Direction) -> Transform {
    match dir {
        Direction::Right => Transform::identity(),
        Direction::Left => mirror(),
        Direction::Up => Transform::from_rotate_at(-90.0, 50.0, 50.0),
        Direction::Down => Transform::from_rotate_at(90.0, 50.0, 50.0),
    }
}

/// `Direction::tail_transform` as a matrix: tail art joins the body on its
/// left edge and points right.
fn tail_transform(dir: Direction) -> Transform {
    match dir {
        Direction::Right => Transform::identity(),
        Direction::Left => mirror(),
        Direction::Down => mirror().pre_concat(Transform::from_rotate_at(90.0, 50.0, 50.0)),
        Direction::Up => mirror().pre_concat(Transform::from_rotate_at(-90.0, 50.0, 50.0)),
    }
}

/// Board pixels a body polyline reaches past its end into the head or tail cell.
const SEAM_OVERLAP: f64 = 1.5;

/// Lengthen a polyline's first (`start`) or last segment by [`SEAM_OVERLAP`].
fn extend_end(line: &mut [(f64, f64)], start: bool) {
    let n = line.len();
    if n < 2 {
        return;
    }
    let (end, prev) = if start { (0, 1) } else { (n - 1, n - 2) };
    let (dx, dy) = (line[end].0 - line[prev].0, line[end].1 - line[prev].1);
    let len = dx.hypot(dy);
    if len > 0.0 {
        line[end].0 += dx / len * SEAM_OVERLAP;
        line[end].1 += dy / len * SEAM_OVERLAP;
    }
}

/// `scale(-1,1) translate(-100,0)`: flip the 100×100 box left to right.
fn mirror() -> Transform {
    Transform::from_row(-1.0, 0.0, 0.0, 1.0, 100.0, 0.0)
}

/// The body polylines as paths, each pushed a little further under the head
/// and tail art: where two anti-aliased edges meet exactly, the background
/// shows through as a seam.
fn body_paths(polylines: &[Vec<(f64, f64)>]) -> Vec<Path> {
    let last = polylines.len().saturating_sub(1);
    polylines
        .iter()
        .enumerate()
        .filter_map(|(n, line)| {
            let mut line = line.clone();
            if n == 0 {
                extend_end(&mut line, true);
            }
            if n == last {
                extend_end(&mut line, false);
            }
            let mut pb = PathBuilder::new();
            for (i, &(px, py)) in line.iter().enumerate() {
                if i == 0 {
                    pb.move_to(px as f32, py as f32);
                } else {
                    pb.line_to(px as f32, py as f32);
                }
            }
            pb.finish()
        })
        .collect()
}

/// Snakes darker than this (relative luminance, 0..=1) get a faint light
/// outline on the board and a light roster tile: a near-black snake would
/// otherwise vanish into the dark board.
const DARK_LUMINANCE: f32 = 0.2;
/// Outline width outside the snake, in board pixels.
const HALO: f32 = 1.5;
const HALO_ALPHA: f32 = 0.4;

pub fn is_dark(color: Color) -> bool {
    0.2126 * color.red() + 0.7152 * color.green() + 0.0722 * color.blue() < DARK_LUMINANCE
}

#[derive(Debug, Clone)]
pub struct BoardSnake {
    /// Head first, as in the game API.
    pub body: Vec<Cell>,
    pub color: Color,
    pub head: Option<&'static RefShape>,
    pub tail: Option<&'static RefShape>,
    /// Drawn first and faded, as the board draws eliminated snakes.
    pub eliminated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BoardArt {
    pub width: i32,
    pub height: i32,
    pub snakes: Vec<BoardSnake>,
    pub food: Vec<Cell>,
    pub hazards: Vec<Cell>,
}

/// Opacity of eliminated snakes. The live board uses 10%, which disappears
/// at card size.
const ELIMINATED_ALPHA: f32 = 0.22;

impl Canvas {
    /// Paint `art` in a framed panel filling the square at (`x`, `y`) of side
    /// `side`: the home page's dark board, the grid, hazards, snakes, food.
    pub fn board(&mut self, art: &BoardArt, x: f32, y: f32, side: f32) {
        self.glow(
            x + side / 2.0,
            y + side / 2.0,
            side * 0.62,
            with_alpha(palette::pink(), 0.16),
        );
        let panel = round_rect(x, y, side, side, 28.0);
        if let Some(panel) = &panel {
            if let Some(shader) =
                vertical_gradient(y, y + side, palette::board_top(), palette::board_bottom())
            {
                self.fill_path(
                    panel,
                    &shaded(shader),
                    SkiaFillRule::Winding,
                    Transform::identity(),
                );
            }
            let stroke = Stroke {
                width: 1.5,
                ..Stroke::default()
            };
            self.stroke_path(
                panel,
                &solid(palette::board_border()),
                &stroke,
                Transform::identity(),
            );
        }

        let (width, height) = (art.width.max(1), art.height.max(1));
        let board = Board::new(width, height);
        let inset = side * 0.035;
        let room = side - 2.0 * inset;
        let scale = (room / board.px_width as f32).min(room / board.px_height as f32);
        let to_card = Transform::from_translate(
            x + (side - board.px_width as f32 * scale) / 2.0,
            y + (side - board.px_height as f32 * scale) / 2.0,
        )
        .pre_scale(scale, scale);

        let cell = CELL as f32;
        let radius = cell * 0.16;
        let grid = solid(palette::cell());
        let hazard = solid(palette::hazard());
        for cx in 0..width {
            for cy in 0..height {
                let (px, py) = board.top_left((cx, cy));
                let paint = if art.hazards.contains(&(cx, cy)) {
                    &hazard
                } else {
                    &grid
                };
                if let Some(path) = round_rect(px as f32, py as f32, cell, cell, radius) {
                    self.fill_path(&path, paint, SkiaFillRule::Winding, to_card);
                }
            }
        }

        // A snake that died off the edge has its head outside the grid; keep
        // it inside the panel.
        if let Some(panel) = &panel {
            self.clip_to(panel);
        }
        let eliminated = art.snakes.iter().filter(|s| s.eliminated);
        let alive = art.snakes.iter().filter(|s| !s.eliminated);
        for snake in eliminated.chain(alive) {
            self.snake(&board, snake, to_card);
        }

        let food = solid(palette::food());
        for &f in &art.food {
            let (cx, cy) = board.center(f);
            if let Some(path) = PathBuilder::from_circle(cx as f32, cy as f32, cell / 3.25) {
                self.fill_path(&path, &food, SkiaFillRule::Winding, to_card);
            }
        }
        self.unclip();
    }

    /// One snake, in the board's order: tail, body polyline(s), head.
    fn snake(&mut self, board: &Board, snake: &BoardSnake, to_card: Transform) {
        let Some(geometry) = board.snake(&snake.body) else {
            return;
        };
        let alpha = if snake.eliminated {
            ELIMINATED_ALPHA
        } else {
            1.0
        };
        let placed = |shape: Option<&'static RefShape>, at: Placement, dir: Transform| {
            let shape = shape?;
            let scale = CELL as f32 / 100.0;
            let transform = to_card
                .pre_translate(at.x as f32, at.y as f32)
                .pre_scale(scale, scale)
                .pre_concat(dir);
            Some((shape_path(shape)?, fill_rule(shape.fill_rule), transform))
        };
        let tail = geometry
            .tail
            .and_then(|at| placed(snake.tail, at, tail_transform(at.dir)));
        let head = placed(snake.head, geometry.head, head_transform(geometry.head.dir));
        let body = body_paths(&geometry.polylines);
        let body_stroke = |width: f32| Stroke {
            width,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };

        if is_dark(snake.color) {
            // Outline first; the fill below covers its inner half.
            let halo = solid(with_alpha(palette::ink(), HALO_ALPHA * alpha));
            let cell = CELL as f32;
            let shape_outline = Stroke {
                // Shapes are in their 100×100 box: convert board pixels.
                width: 2.0 * HALO * 100.0 / cell,
                line_join: LineJoin::Round,
                ..Stroke::default()
            };
            for (path, _, transform) in tail.iter().chain(head.iter()) {
                self.stroke_path(path, &halo, &shape_outline, *transform);
            }
            for path in &body {
                self.stroke_path(path, &halo, &body_stroke(cell + 2.0 * HALO), to_card);
            }
        }

        let paint = solid(with_alpha(snake.color, alpha));
        if let Some((path, rule, transform)) = &tail {
            self.fill_path(path, &paint, *rule, *transform);
        }
        for path in &body {
            self.stroke_path(path, &paint, &body_stroke(CELL as f32), to_card);
        }
        if let Some((path, rule, transform)) = &head {
            self.fill_path(path, &paint, *rule, *transform);
        }
    }

    /// A snake's head, facing right, on a rounded tile at (`x`, `y`) of side
    /// `side`: the roster's avatar.
    pub fn head_tile(&mut self, head: Option<&RefShape>, color: Color, x: f32, y: f32, side: f32) {
        let tile = if is_dark(color) {
            palette::ink()
        } else {
            palette::card()
        };
        self.round_rect(x, y, side, side, side * 0.24, tile);
        if let Some(border) = round_rect(x, y, side, side, side * 0.24) {
            let stroke = Stroke {
                width: 1.5,
                ..Stroke::default()
            };
            self.stroke_path(
                &border,
                &solid(palette::hairline()),
                &stroke,
                Transform::identity(),
            );
        }
        let Some(shape) = head else {
            return;
        };
        let Some(path) = shape_path(shape) else {
            return;
        };
        let inner = side * 0.64;
        let transform =
            Transform::from_translate(x + (side - inner) / 2.0, y + (side - inner) / 2.0)
                .pre_scale(inner / 100.0, inner / 100.0);
        self.fill_path(&path, &solid(color), fill_rule(shape.fill_rule), transform);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Point;

    fn map(t: Transform, x: f32, y: f32) -> (f32, f32) {
        let mut p = [Point::from_xy(x, y)];
        t.map_points(&mut p);
        (
            (p[0].x * 100.0).round() / 100.0,
            (p[0].y * 100.0).round() / 100.0,
        )
    }

    #[test]
    fn head_transforms_point_the_face_the_right_way() {
        // Head art faces right: its right-edge midpoint is the snout.
        let snout = |dir| map(head_transform(dir), 100.0, 50.0);
        assert_eq!(snout(Direction::Right), (100.0, 50.0));
        assert_eq!(snout(Direction::Left), (0.0, 50.0));
        assert_eq!(snout(Direction::Up), (50.0, 0.0));
        assert_eq!(snout(Direction::Down), (50.0, 100.0));
        // The eye (top-left in the art) stays on the snake's left when it turns up.
        assert_eq!(map(head_transform(Direction::Up), 12.5, 28.5), (28.5, 87.5));
    }

    #[test]
    fn tail_transforms_point_the_tip_the_right_way() {
        let tip = |dir| map(tail_transform(dir), 100.0, 50.0);
        assert_eq!(tip(Direction::Right), (100.0, 50.0));
        assert_eq!(tip(Direction::Left), (0.0, 50.0));
        assert_eq!(tip(Direction::Up), (50.0, 0.0));
        assert_eq!(tip(Direction::Down), (50.0, 100.0));
    }

    #[test]
    fn body_ends_reach_further_into_head_and_tail() {
        let mut line = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        extend_end(&mut line, true);
        extend_end(&mut line, false);
        assert_eq!(
            line,
            vec![
                (-SEAM_OVERLAP, 0.0),
                (10.0, 0.0),
                (10.0, 10.0 + SEAM_OVERLAP)
            ]
        );
        let mut point = vec![(3.0, 3.0)];
        extend_end(&mut point, true);
        assert_eq!(point, vec![(3.0, 3.0)]);
    }

    #[test]
    fn a_head_off_the_board_stays_inside_the_panel() {
        let green = crate::og::canvas::rgb(0, 255, 0);
        let art = BoardArt {
            width: 11,
            height: 11,
            // Died moving left off the board from (0, 5).
            snakes: vec![BoardSnake {
                body: vec![(-1, 5), (0, 5), (1, 5)],
                color: green,
                head: shape(AssetKind::Head, "default"),
                tail: shape(AssetKind::Tail, "default"),
                eliminated: false,
            }],
            ..BoardArt::default()
        };
        let mut c = Canvas::new().unwrap();
        c.board(&art, 100.0, 100.0, 200.0);
        // Cell row y = 5 spans card y ~193-207 and the panel starts at x = 100;
        // unclipped, the head would reach x = 98. Inside the panel the body is
        // drawn; left of the panel edge, no snake green at all.
        assert!(
            (110..200).any(|x| c.pixel(x, 200) == [0, 255, 0]),
            "body missing"
        );
        for y in 194..206 {
            for x in 90..99 {
                assert!(
                    c.pixel(x, y)[1] < 120,
                    "head drawn outside the panel at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn dark_snakes_are_the_ones_the_board_would_swallow() {
        use crate::og::canvas::{parse_hex, rgb};
        assert!(is_dark(parse_hex("#25272b").unwrap()));
        assert!(is_dark(palette::cell()));
        assert!(is_dark(rgb(0, 0, 0)));
        for bright in ["#ff3d8a", "#3ddba0", "#888888", "#ff6600", "#0066ff"] {
            assert!(!is_dark(parse_hex(bright).unwrap()), "{bright}");
        }
    }

    #[test]
    fn every_reference_shape_parses() {
        for r in REFS {
            let path = shape_path(r).unwrap_or_else(|| panic!("{} failed to parse", r.slug));
            // Tight bounds: `bounds()` counts Bézier control points.
            let b = path.compute_tight_bounds().unwrap();
            assert!(b.left() >= -1.0 && b.right() <= 101.0, "{}: {b:?}", r.slug);
            assert!(b.top() >= -1.0 && b.bottom() <= 101.0, "{}: {b:?}", r.slug);
        }
    }

    #[test]
    fn unknown_cosmetics_fall_back_to_the_default_shape() {
        assert_eq!(shape(AssetKind::Head, "bendr").unwrap().slug, "bendr");
        assert_eq!(
            shape(AssetKind::Head, "bendr").unwrap().kind,
            AssetKind::Head
        );
        assert_eq!(
            shape(AssetKind::Tail, "pixel").unwrap().kind,
            AssetKind::Tail
        );
        let fallback = shape(AssetKind::Head, "not-a-real-head").unwrap();
        assert_eq!((fallback.slug, fallback.kind), ("default", AssetKind::Head));
        let fallback = shape(AssetKind::Tail, "").unwrap();
        assert_eq!((fallback.slug, fallback.kind), ("default", AssetKind::Tail));
    }
}
