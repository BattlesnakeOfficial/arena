//! Untrusted SVG -> one clean path in 0..100 space.
//!
//! 1. UTF-8, a byte-level depth scan (before roxmltree's recursive tokenizer runs), only
//!    short plain-text entity declarations (Illustrator's namespace block; no XXE, no
//!    billion laughs), roxmltree with a node limit.
//! 2. [`svg_scan::scan`]: counts, lint facts, the CSS cost, and the reference graphs
//!    (cycles, nesting along reference chains, expansion).
//! 3. usvg converts shapes, transforms, CSS, `<use>` and the viewBox into plain paths.
//!    Its image resolvers are replaced with no-ops: the defaults read local files.
//! 4. [`paint::plan`]: the template filter (`draw-here`, guides, references, template
//!    colours), and the segment, dash and clip budgets.
//! 5. The truth raster ([`paint::paint`], which also bounds the outline travel) at
//!    512 px, and vector candidates built from every painted fill and stroke outline
//!    (see [`exact_candidate`]). The one that renders closest to the truth is emitted as
//!    is (`VectorExact`) when it differs only along edges (anti-aliasing) and stays
//!    inside the square. Otherwise (layered white and dark details, small details no
//!    fill rule reproduces, clips that cut through shapes, opacity, shapes reaching off
//!    the square...), or when its `d` would be too long, the truth is painted again at
//!    the trace grid size and traced (`Retraced`).

use tiny_skia::{PathBuilder, PathSegment, Point};
use usvg::roxmltree;

use super::lints::Lint;
use super::paint::{self, Painted, Shape};
use super::palette::{self, Rgb};
use super::raster::render_alpha;
use super::{FillRule, Limits, ProcessError, SPECK_UNITS, Strategy, emit, svg_scan, trace};

/// Most pixels (as a share of the comparison raster) on which the vector candidate may
/// differ from the truth along edges (anti-aliasing) and still be emitted as is.
const EXACT_TOLERANCE: f64 = 0.001;
/// Side of the raster the candidate is compared on.
const COMPARE_SIDE: u32 = 512;

/// Entity declarations: Illustrator's "Save As SVG" declares its namespace URIs as
/// entities in the DOCTYPE (`<!ENTITY ns_ai "http://ns.adobe.com/...">`). Only short
/// plain-text values are accepted: no `&` (so no nesting: no billion laughs), no
/// `SYSTEM`/`PUBLIC` (no XXE), no `%` (no parameter entities), no `<` (no markup), and
/// every reference to them may add at most this much text in total.
const MAX_ENTITIES: usize = 32;
const MAX_ENTITY_VALUE_BYTES: usize = 256;
const MAX_ENTITY_EXPANSION_BYTES: usize = 256 * 1024;

/// The `InvalidXml` detail for a rejected entity declaration (its message names
/// Illustrator's export).
pub(crate) const ENTITIES_REJECTED: &str =
    "entity declarations other than short plain text are not allowed";

/// The start of the `InvalidSvg` detail for bytes that aren't UTF-8 (its message says how
/// to save as UTF-8).
pub(crate) const NOT_UTF8: &str = "the file is not UTF-8 text";

/// A converted SVG, before fixes, metrics and lints.
pub(crate) struct SvgShape {
    pub path: tiny_skia::Path,
    pub fill_rule: FillRule,
    pub strategy: Strategy,
    pub info: Vec<Lint>,
}

pub(crate) fn process(bytes: &[u8], limits: &Limits) -> Result<SvgShape, ProcessError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ProcessError::InvalidSvg(format!("{NOT_UTF8} ({e})")))?;
    // roxmltree's tokenizer recurses once per nesting level: ~5000 nested tags overflow
    // a 2 MiB stack (~200 in a debug build) and abort the process. Bound the depth from
    // the bytes first.
    let depth = svg_scan::depth_upper_bound(bytes).ok_or_else(|| {
        ProcessError::InvalidXml("a DOCTYPE or declaration is malformed or unterminated".into())
    })?;
    if depth > limits.max_svg_depth {
        return Err(ProcessError::TooComplex("elements are nested too deeply"));
    }
    // Illustrator emits `<!DOCTYPE svg PUBLIC ...>`, so a DTD is allowed, but entity
    // declarations only as Illustrator writes them.
    check_entities(text)?;
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
    let canvas = tiny_skia::Transform::from_row(
        s,
        0.0,
        0.0,
        s,
        (100.0 - w * s) / 2.0,
        (100.0 - h * s) / 2.0,
    );

    let plan = paint::plan(&tree, limits)?;
    let facts = &plan.facts;
    let mut info = Vec::new();
    if facts.guides_dropped {
        info.push(Lint::GuidesVisible);
    }
    if facts.outside_draw_here {
        info.push(Lint::OutsideDrawHereIgnored);
    }
    if facts.multi_colour()
        || facts
            .colours
            .iter()
            .any(|&[r, g, b]| palette::coloured(Rgb::new(r, g, b)))
    {
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
    if facts.clipped || facts.masked {
        info.push(Lint::ClipOrMask {
            clipped: facts.clipped,
            masked: facts.masked,
        });
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

    let Painted {
        ink,
        shapes,
        outside,
    } = paint::paint(&tree, &plan, canvas, COMPARE_SIDE, limits.max_svg_travel)?;
    if !ink.iter().any(|&b| b) {
        return Err(ProcessError::Empty { info });
    }
    if outside {
        info.push(Lint::OutsideCanvas);
    }

    // The exact path must stay inside the square. Contours wholly outside it are
    // dropped (they change nothing inside it). One that crosses the edge sends the
    // drawing to the trace: the board hides the part outside, but Fit would scale it
    // into view, and the artist's own clip (which the candidates don't carry) may have
    // cut it already.
    let exact = exact_candidate(&shapes, &ink)?
        .and_then(|(path, rule)| Some((without_outside_contours(&path)?, rule)))
        .filter(|(path, _)| {
            !paint::reaches_outside(path) && emit::path_to_d(path).len() <= limits.max_path_d_bytes
        });
    if let Some((path, fill_rule)) = exact {
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

/// Accept only Illustrator-style entity declarations (see [`MAX_ENTITIES`]). Every
/// `<!ENTITY` in the text counts, wherever it is (even in a comment): rejecting a
/// harmless one is fine, missing a harmful one is not.
fn check_entities(text: &str) -> Result<(), ProcessError> {
    let reject = || ProcessError::InvalidXml(ENTITIES_REJECTED.into());
    let mut declared: Vec<(&str, &str)> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<!ENTITY") {
        let (name, value, tail) = plain_entity(&rest[i + "<!ENTITY".len()..]).ok_or_else(reject)?;
        declared.push((name, value));
        if declared.len() > MAX_ENTITIES {
            return Err(reject());
        }
        rest = tail;
    }
    let mut expansion = 0usize;
    for (name, value) in declared {
        let refs = text.matches(&format!("&{name};")).count();
        expansion = expansion.saturating_add(refs.saturating_mul(value.len()));
    }
    if expansion > MAX_ENTITY_EXPANSION_BYTES {
        return Err(reject());
    }
    Ok(())
}

/// After `<!ENTITY`: `S Name S ("value" | 'value') S? '>'`, with an ASCII name and a short
/// value free of `&`, `%` and `<`. Returns the name, the value and the text after `>`.
fn plain_entity(s: &str) -> Option<(&str, &str, &str)> {
    let space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let s0 = s.trim_start_matches(space);
    if s0.len() == s.len() || !s0.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    let name_len = s0
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(s0.len());
    let (name, s1) = s0.split_at(name_len);
    let s2 = s1.trim_start_matches(space);
    if s2.len() == s1.len() {
        return None;
    }
    let quote = s2.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let body = &s2[1..];
    let end = body.find(quote)?;
    let value = &body[..end];
    if value.len() > MAX_ENTITY_VALUE_BYTES || value.contains(['&', '%', '<']) {
        return None;
    }
    let tail = body[end + 1..]
        .trim_start_matches(space)
        .strip_prefix('>')?;
    Some((name, value, tail))
}

/// Most differing pixels away from every edge (of the truth or of the candidate): half
/// the area of the smallest detail the trace keeps ([`SPECK_UNITS`]), so losing or adding
/// a detail that a retrace would keep always rules the candidate out.
fn max_hard_pixels() -> usize {
    let speck_px = COMPARE_SIDE as f32 / 100.0 * SPECK_UNITS;
    (speck_px * speck_px / 2.0) as usize
}

/// How a candidate's render differs from the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Diff {
    /// Differing pixels with no edge of the truth and of the candidate next to them: a
    /// detail one has and the other doesn't (a filled-in hole, an extra speck).
    hard: usize,
    /// All differing pixels; the rest are anti-aliasing along shared edges.
    total: usize,
}

fn compare(cand: &[bool], truth: &[bool], side: usize) -> Diff {
    let mut d = Diff { hard: 0, total: 0 };
    for (i, (&c, &t)) in cand.iter().zip(truth).enumerate() {
        if c == t {
            continue;
        }
        d.total += 1;
        let (x, y) = (i % side, i / side);
        if !(has_edge(truth, side, x, y) && has_edge(cand, side, x, y)) {
            d.hard += 1;
        }
    }
    d
}

/// Does the 3x3 neighbourhood of (x, y) hold both values?
fn has_edge(m: &[bool], side: usize, x: usize, y: usize) -> bool {
    let v = m[y * side + x];
    let (x0, x1) = (x.saturating_sub(1), (x + 1).min(side - 1));
    let (y0, y1) = (y.saturating_sub(1), (y + 1).min(side - 1));
    (y0..=y1).any(|yy| (x0..=x1).any(|xx| m[yy * side + xx] != v))
}

/// The vector candidate and fill rule that reproduce the truth best, if one does: it
/// may differ only along edges (at most [`EXACT_TOLERANCE`] of the pixels, and at most
/// [`max_hard_pixels`] away from any edge). The first that differs only along edges is
/// taken; otherwise the one with the fewest differences.
///
/// Candidates:
/// 1. every paint concatenated, nonzero and even-odd. Even-odd also covers white details
///    drawn over a dark shape: both outlines are there, so the detail is a hole.
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
    let max_hard = max_hard_pixels();
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
    let mut best: Option<(Diff, tiny_skia::Path, FillRule)> = None;
    for (path, rules) in candidates {
        let Some(path) = path else {
            continue;
        };
        for &rule in rules {
            let cand: Vec<bool> = render_alpha(&path, rule, COMPARE_SIDE)?
                .iter()
                .map(|&a| a >= 128)
                .collect();
            let diff = compare(&cand, truth, side);
            if diff.hard > max_hard || diff.total > tolerance {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _, _)| diff < *b) {
                let edges_only = diff.hard == 0;
                best = Some((diff, path.clone(), rule));
                if edges_only {
                    return Ok(best.map(|(_, p, r)| (p, r)));
                }
            }
        }
    }
    Ok(best.map(|(_, p, r)| (p, r)))
}

/// `path` without the contours that lie wholly outside the square. They change nothing
/// inside it (a closed contour winds zero times around any point outside its bounding
/// box), the board never shows them, and Fit would otherwise measure them.
fn without_outside_contours(path: &tiny_skia::Path) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut contour: Vec<PathSegment> = Vec::new();
    let mut flush = |contour: &mut Vec<PathSegment>| {
        let mut b = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for seg in contour.iter() {
            let pts: &[Point] = match seg {
                PathSegment::MoveTo(p) | PathSegment::LineTo(p) => std::slice::from_ref(p),
                PathSegment::QuadTo(c, p) => &[*c, *p],
                PathSegment::CubicTo(c1, c2, p) => &[*c1, *c2, *p],
                PathSegment::Close => &[],
            };
            for p in pts {
                b = [b[0].min(p.x), b[1].min(p.y), b[2].max(p.x), b[3].max(p.y)];
            }
        }
        // The control points hold the curve, so this only ever keeps too much.
        if b[0] <= 100.0 && b[2] >= 0.0 && b[1] <= 100.0 && b[3] >= 0.0 {
            for seg in contour.iter() {
                match *seg {
                    PathSegment::MoveTo(p) => pb.move_to(p.x, p.y),
                    PathSegment::LineTo(p) => pb.line_to(p.x, p.y),
                    PathSegment::QuadTo(c, p) => pb.quad_to(c.x, c.y, p.x, p.y),
                    PathSegment::CubicTo(c1, c2, p) => {
                        pb.cubic_to(c1.x, c1.y, c2.x, c2.y, p.x, p.y)
                    }
                    PathSegment::Close => pb.close(),
                }
            }
        }
        contour.clear();
    };
    for seg in path.segments() {
        if matches!(seg, PathSegment::MoveTo(_)) {
            flush(&mut contour);
        }
        contour.push(seg);
    }
    flush(&mut contour);
    pb.finish()
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
