//! Untrusted SVG -> one clean path in 0..100 space.
//!
//! 1. UTF-8, a byte-level depth scan (before roxmltree's recursive tokenizer runs), no
//!    entity declarations (XXE, billion laughs), roxmltree with a node limit.
//! 2. [`svg_scan::scan`]: counts, lint facts, the CSS cost, and the reference graphs
//!    (cycles, nesting along reference chains, expansion).
//! 3. usvg converts shapes, transforms, CSS, `<use>` and the viewBox into plain paths.
//!    Its image resolvers are replaced with no-ops: the defaults read local files.
//! 4. [`paint::plan`]: the template filter (`draw-here`, guides, references, template
//!    colours), and the segment, dash and clip budgets.
//! 5. The truth raster ([`paint::paint`], which also bounds the outline travel) at
//!    512 px, and vector candidates built from every painted fill and stroke outline
//!    (see [`exact_candidate`]). One that renders the same as the truth (within 0.1% of
//!    the pixels) is emitted as is (`VectorExact`). Otherwise (layered white and dark
//!    details, clips that cut through shapes, opacity...), or when its `d` would be too
//!    long, the truth is painted again at the trace grid size and traced (`Retraced`).

use tiny_skia::{PathBuilder, PathSegment, Point, Transform};
use usvg::roxmltree;

use super::lints::Lint;
use super::paint::{self, Painted, Shape};
use super::raster::render_alpha;
use super::{FillRule, Limits, ProcessError, Strategy, emit, svg_scan, trace};

/// Most pixels (as a share of the comparison raster) on which the vector candidate may
/// differ from the truth and still be emitted as is.
const EXACT_TOLERANCE: f64 = 0.001;
/// Side of the raster the candidate is compared on.
const COMPARE_SIDE: u32 = 512;
/// How far (in units) the emitted path may reach outside the square before
/// `outside_canvas` is reported.
const OUTSIDE_SLACK: f32 = 0.5;

/// A converted SVG, before fixes, metrics and lints.
pub(crate) struct SvgShape {
    pub path: tiny_skia::Path,
    pub fill_rule: FillRule,
    pub strategy: Strategy,
    pub info: Vec<Lint>,
}

pub(crate) fn process(bytes: &[u8], limits: &Limits) -> Result<SvgShape, ProcessError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ProcessError::InvalidSvg(format!("the file is not UTF-8 text ({e})")))?;
    // roxmltree's tokenizer recurses once per nesting level: ~5000 nested tags overflow
    // a 2 MiB stack (~200 in a debug build) and abort the process. Bound the depth from
    // the bytes first.
    if svg_scan::depth_upper_bound(bytes) > limits.max_svg_depth {
        return Err(ProcessError::TooComplex("elements are nested too deeply"));
    }
    // Illustrator emits `<!DOCTYPE svg PUBLIC ...>`, so a DTD is allowed, but entity
    // declarations have no use in a drawing: refuse them (XXE, billion laughs).
    if text.contains("<!ENTITY") {
        return Err(ProcessError::InvalidXml(
            "entity declarations are not allowed".into(),
        ));
    }
    let xml_options = roxmltree::ParsingOptions {
        allow_dtd: true,
        nodes_limit: limits.max_svg_nodes,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, xml_options)
        .map_err(|e| ProcessError::InvalidXml(e.to_string()))?;
    let scanned = svg_scan::scan(&doc, limits)?;

    let options = usvg::Options {
        // No base directory, and no image loading at all: usvg's default resolvers read
        // local files named by `<image href>` (relative to the working directory when
        // there is no base directory) and decode embedded rasters.
        resources_dir: None,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_xmltree(&doc, &options)
        .map_err(|e| ProcessError::InvalidSvg(e.to_string()))?;
    drop(doc);

    // The canvas (after viewBox, width, height and preserveAspectRatio), centred in the
    // 100x100 square.
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    let s = 100.0 / w.max(h);
    let canvas = Transform::from_row(s, 0.0, 0.0, s, (100.0 - w * s) / 2.0, (100.0 - h * s) / 2.0);

    let plan = paint::plan(&tree, limits)?;
    let facts = &plan.facts;
    let mut info = Vec::new();
    if facts.guides_dropped {
        info.push(Lint::GuidesVisible);
    }
    if facts.multi_colour() || facts.colours.iter().any(|&c| coloured(c)) {
        info.push(Lint::ColoursFlattened);
    }
    if facts.strokes > 0 {
        info.push(Lint::StrokesConverted {
            count: facts.strokes,
        });
    }
    if facts.gradient {
        info.push(Lint::Gradient);
    }
    if facts.semi {
        info.push(Lint::SemiTransparent);
    }
    if facts.clip_or_mask {
        info.push(Lint::ClipOrMask);
    }
    if facts.filters || scanned.filters {
        info.push(Lint::FiltersIgnored);
    }
    if scanned.images > 0 {
        info.push(Lint::ImageIgnored {
            count: scanned.images,
        });
    }
    if scanned.text {
        info.push(Lint::TextIgnored);
    }
    if scanned.active {
        info.push(Lint::ActiveContentRemoved);
    }
    if (w - h).abs() > 0.01 * w.max(h) {
        info.push(Lint::NonSquare {
            width: w.round() as u32,
            height: h.round() as u32,
        });
    }

    let Painted { ink, shapes } =
        paint::paint(&tree, &plan, canvas, COMPARE_SIDE, limits.max_svg_travel)?;
    if !ink.iter().any(|&b| b) {
        return Err(ProcessError::Empty { info });
    }

    let exact = exact_candidate(&shapes, &ink)?
        .filter(|(path, _)| emit::path_to_d(path).len() <= limits.max_path_d_bytes);
    if let Some((path, fill_rule)) = exact {
        if let Some(b) = path.compute_tight_bounds()
            && (b.left() < -OUTSIDE_SLACK
                || b.top() < -OUTSIDE_SLACK
                || b.right() > 100.0 + OUTSIDE_SLACK
                || b.bottom() > 100.0 + OUTSIDE_SLACK)
        {
            info.push(Lint::OutsideCanvas);
        }
        return Ok(SvgShape {
            path,
            fill_rule,
            strategy: Strategy::VectorExact,
            info,
        });
    }

    let side = limits.trace_side;
    let truth = paint::paint(&tree, &plan, canvas, side, limits.max_svg_travel)?.ink;
    drop(tree);
    let traced = trace::trace_mask(truth, side as usize, limits, trace::Source::Vector)?;
    if traced.specks_removed > 0 {
        info.push(Lint::SpecksRemoved {
            count: traced.specks_removed,
        });
    }
    let Some(path) = traced.path else {
        return Err(ProcessError::Empty { info });
    };
    Ok(SvgShape {
        path,
        fill_rule: FillRule::EvenOdd,
        strategy: Strategy::Retraced,
        info,
    })
}

/// "Visibly coloured", as for rasters: chroma ≥ 64.
fn coloured(c: [u8; 3]) -> bool {
    let (max, min) = (c.iter().max(), c.iter().min());
    matches!((max, min), (Some(max), Some(min)) if max - min >= 64)
}

/// The vector candidate and fill rule that reproduce the truth, if any do.
///
/// Candidates, first match wins:
/// 1. every paint concatenated, nonzero then even-odd. Even-odd also covers white
///    details drawn over a dark shape: both outlines are there, so the detail is a hole.
/// 2. every paint with each shape turned to wind the same way (by its signed area),
///    nonzero: separate shapes that overlap with opposite winding would otherwise
///    cancel out where they overlap, but each shape's own holes still wind against it.
/// 3. multi-colour drawings: the same without the light paints (white drawn under
///    dark shapes and showing through their holes isn't ink).
fn exact_candidate(
    shapes: &[Shape],
    truth: &[bool],
) -> Result<Option<(tiny_skia::Path, FillRule)>, ProcessError> {
    let side = COMPARE_SIDE as usize;
    let tolerance = (EXACT_TOLERANCE * (side * side) as f64) as usize;
    let multi = shapes.iter().any(|s| s.light);
    let all = || shapes.iter();
    let dark = || shapes.iter().filter(|s| !s.light);
    let mut candidates: Vec<(Option<tiny_skia::Path>, &[FillRule])> = vec![
        (
            concat(all(), false),
            &[FillRule::NonZero, FillRule::EvenOdd],
        ),
        (concat(all(), true), &[FillRule::NonZero]),
    ];
    if multi {
        candidates.push((
            concat(dark(), false),
            &[FillRule::NonZero, FillRule::EvenOdd],
        ));
        candidates.push((concat(dark(), true), &[FillRule::NonZero]));
    }
    for (path, rules) in candidates {
        let Some(path) = path else {
            continue;
        };
        for &rule in rules {
            let alpha = render_alpha(&path, rule, COMPARE_SIDE)?;
            let diff = alpha
                .iter()
                .zip(truth)
                .filter(|(a, t)| (**a >= 128) != **t)
                .count();
            if diff <= tolerance {
                return Ok(Some((path, rule)));
            }
        }
    }
    Ok(None)
}

/// Concatenate shapes into one path, optionally turning each to positive winding.
fn concat<'a>(shapes: impl Iterator<Item = &'a Shape>, normalise: bool) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for s in shapes {
        if normalise && signed_area(&s.path) < 0.0 {
            if let Some(r) = reversed(&s.path) {
                pb.push_path(&r);
            }
        } else {
            pb.push_path(&s.path);
        }
    }
    pb.finish()
}

fn cross(a: Point, b: Point) -> f64 {
    a.x as f64 * b.y as f64 - b.x as f64 * a.y as f64
}

/// Twice the signed area of a path's control polygons (every contour closed). Its sign
/// is the winding direction of the outline that dominates: outer contours outweigh the
/// holes inside them.
fn signed_area(path: &tiny_skia::Path) -> f64 {
    let (mut area, mut start, mut cur) = (0.0, Point::zero(), Point::zero());
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                area += cross(cur, start);
                start = p;
                cur = p;
            }
            PathSegment::LineTo(p) => {
                area += cross(cur, p);
                cur = p;
            }
            PathSegment::QuadTo(c, p) => {
                area += cross(cur, c) + cross(c, p);
                cur = p;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                area += cross(cur, c1) + cross(c1, c2) + cross(c2, p);
                cur = p;
            }
            PathSegment::Close => {
                area += cross(cur, start);
                cur = start;
            }
        }
    }
    area + cross(cur, start)
}

/// The same outline traced the other way round, contour by contour.
fn reversed(path: &tiny_skia::Path) -> Option<tiny_skia::Path> {
    enum Seg {
        Line,
        Quad(Point),
        Cubic(Point, Point),
    }
    struct Contour {
        /// Start point, then the end point of every segment.
        points: Vec<Point>,
        segs: Vec<Seg>,
        closed: bool,
    }
    let mut contours: Vec<Contour> = Vec::new();
    for seg in path.segments() {
        if let PathSegment::MoveTo(p) = seg {
            contours.push(Contour {
                points: vec![p],
                segs: Vec::new(),
                closed: false,
            });
            continue;
        }
        let Some(c) = contours.last_mut() else {
            continue;
        };
        match seg {
            PathSegment::LineTo(p) => {
                c.segs.push(Seg::Line);
                c.points.push(p);
            }
            PathSegment::QuadTo(q, p) => {
                c.segs.push(Seg::Quad(q));
                c.points.push(p);
            }
            PathSegment::CubicTo(c1, c2, p) => {
                c.segs.push(Seg::Cubic(c1, c2));
                c.points.push(p);
            }
            PathSegment::Close => c.closed = true,
            PathSegment::MoveTo(_) => {}
        }
    }
    let mut pb = PathBuilder::new();
    for c in &contours {
        let Some(&last) = c.points.last() else {
            continue;
        };
        pb.move_to(last.x, last.y);
        for (i, seg) in c.segs.iter().enumerate().rev() {
            let to = c.points[i];
            match seg {
                Seg::Line => pb.line_to(to.x, to.y),
                Seg::Quad(q) => pb.quad_to(q.x, q.y, to.x, to.y),
                Seg::Cubic(c1, c2) => pb.cubic_to(c2.x, c2.y, c1.x, c1.y, to.x, to.y),
            }
        }
        if c.closed {
            pb.close();
        }
    }
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(cw: bool) -> tiny_skia::Path {
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        if cw {
            pb.line_to(10.0, 0.0);
            pb.line_to(10.0, 10.0);
            pb.line_to(0.0, 10.0);
        } else {
            pb.line_to(0.0, 10.0);
            pb.line_to(10.0, 10.0);
            pb.line_to(10.0, 0.0);
        }
        pb.close();
        pb.finish().expect("square")
    }

    #[test]
    fn reversing_flips_the_signed_area() {
        let cw = square(true);
        let ccw = square(false);
        assert!(signed_area(&cw) > 0.0 && signed_area(&ccw) < 0.0);
        let r = reversed(&ccw).expect("reversed");
        assert!((signed_area(&r) - signed_area(&cw)).abs() < 1e-6);
        assert_eq!(r.bounds(), ccw.bounds());
        // Curves keep their control points, swapped.
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.cubic_to(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
        let r = reversed(&pb.finish().expect("curve")).expect("reversed");
        let segs: Vec<PathSegment> = r.segments().collect();
        assert_eq!(
            segs,
            vec![
                PathSegment::MoveTo(Point::from_xy(5.0, 6.0)),
                PathSegment::CubicTo(
                    Point::from_xy(3.0, 4.0),
                    Point::from_xy(1.0, 2.0),
                    Point::from_xy(0.0, 0.0)
                ),
            ]
        );
    }
}
