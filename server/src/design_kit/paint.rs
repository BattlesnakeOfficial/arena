//! The bounded "truth" painter for SVG input, and the template-aware filter.
//!
//! The truth raster is what the drawing looks like in the artist's app, reduced to ink
//! or not. It is painted with tiny-skia from the usvg tree, deliberately *not* with
//! resvg: resvg renders filters and allocates a layer per mask and opacity group, and on
//! untrusted input that is a memory and CPU bomb (a 1.5 KB chain of nested masks reached
//! 949 MB; a 310-byte morphology filter ran for over 120 s). This painter:
//!
//! * paints fills and strokes in order, with group opacity folded into each paint (no
//!   layers), in black, or in white for the light colours of a multi-colour drawing;
//! * applies clip paths as coverage masks (at most `Limits::max_svg_clips` of them);
//! * ignores filters, paints masked content unmasked, and replaces gradients and
//!   patterns with one colour (their stops' average; black for patterns);
//! * bounds its own work: path segments, dashes and clip masks are counted first, and
//!   the vertical travel of every outline is charged before it is filled ([`travel`]).
//!
//! When the drawing uses more than one colour, near-white paints (luma ≥ 0.9) are
//! cut-outs, the way white eyes on a black head are drawn. Ink is a pixel that is at
//! least half opaque and at least half black.
//!
//! **Template filter** (the SVG template's contract, `palette.rs`): if a group with id
//! `draw-here` contains anything, only it is used. Otherwise everything is used except
//! groups with id `guides` or `reference-*` (so an artist who drew on a new layer and
//! left `draw-here` empty still gets her drawing). Either way, paints in exactly a
//! template colour are dropped: Figma and some exporters strip ids, but keep colours.
//! Hidden content (`display:none`, `visibility:hidden`) never reaches us: usvg drops it.

use std::collections::BTreeSet;

use tiny_skia::{Mask, Paint, PathSegment, Pixmap, Transform};

use super::palette::{Rgb, TEMPLATE};
use super::{Limits, ProcessError};

/// Luma (0..1) at or above which a colour is a cut-out in a multi-colour drawing.
pub(crate) const LIGHT_LUMA: f32 = 0.9;

/// Most clip-path renders (a chain of clip paths on clip paths is one render each). Only
/// two masks of a chain are alive at once; `Limits::max_svg_clips` bounds the masks
/// alive at once across nested clipped groups.
const MAX_CLIP_RENDERS: usize = 256;

/// What the painter noticed, for lints.
#[derive(Debug, Default, Clone)]
pub(crate) struct Facts {
    /// Distinct painted colours.
    pub colours: BTreeSet<[u8; 3]>,
    pub strokes: usize,
    /// Gradient or pattern paint (painted in one colour).
    pub gradient: bool,
    /// A paint less than fully opaque.
    pub semi: bool,
    pub clip_or_mask: bool,
    pub filters: bool,
    /// Template guides or references were present and left out.
    pub guides_dropped: bool,
}

impl Facts {
    pub fn multi_colour(&self) -> bool {
        self.colours.len() > 1
    }
}

/// Which part of the tree is the drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Only the content of the first `draw-here` group.
    DrawHere,
    /// Everything except template layers.
    Full,
}

/// A survey of the drawing: what to paint, and whether painting it is affordable.
pub(crate) struct Plan {
    mode: Mode,
    pub facts: Facts,
}

/// The truth raster, and the painted geometry for vector candidates.
pub(crate) struct Painted {
    /// `side`x`side`, row-major: ink or not.
    pub ink: Vec<bool>,
    /// Every painted fill and stroke outline, in paint order, in 0..100 space.
    pub shapes: Vec<Shape>,
}

/// One painted fill or stroke outline.
pub(crate) struct Shape {
    pub path: tiny_skia::Path,
    /// A light paint in a multi-colour drawing (a cut-out, not ink).
    pub light: bool,
}

fn norm_id(id: &str) -> String {
    id.trim()
        .chars()
        .map(|c| match c {
            '_' | ' ' => '-',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

fn id_is(id: &str, name: &str) -> bool {
    let id = norm_id(id);
    id == name
        || id
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// `draw-here` (also `Draw_here`, `draw-here-2`, ...).
fn is_draw_here(id: &str) -> bool {
    !id.is_empty() && id_is(id, "draw-here")
}

/// `guides` or `reference-*`: template layers that are never the drawing.
fn is_template_layer(id: &str) -> bool {
    !id.is_empty() && (id_is(id, "guides") || id_is(id, "reference"))
}

fn is_template_colour(paint: &usvg::Paint) -> bool {
    match paint {
        usvg::Paint::Color(c) => TEMPLATE.contains(&Rgb::new(c.red, c.green, c.blue)),
        _ => false,
    }
}

/// The fill and stroke paints of a path; none when it is hidden.
fn visible_paints(p: &usvg::Path) -> impl Iterator<Item = &usvg::Paint> {
    let visible = p.is_visible();
    let fill = p.fill().map(usvg::Fill::paint);
    let stroke = p.stroke().map(usvg::Stroke::paint);
    fill.into_iter().chain(stroke).filter(move |_| visible)
}

/// Anything visible in `g` that would be painted (in any colour).
fn has_paint(g: &usvg::Group) -> bool {
    g.children().iter().any(|c| match c {
        usvg::Node::Group(cg) => has_paint(cg),
        usvg::Node::Path(p) => visible_paints(p).next().is_some(),
        _ => false,
    })
}

/// Anything in `g` that is the artist's ink (not template colours or layers).
fn has_ink(g: &usvg::Group) -> bool {
    g.children().iter().any(|c| match c {
        usvg::Node::Group(cg) => !is_template_layer(cg.id()) && has_ink(cg),
        usvg::Node::Path(p) => visible_paints(p).any(|paint| !is_template_colour(paint)),
        _ => false,
    })
}

fn find_draw_here(g: &usvg::Group) -> Option<&usvg::Group> {
    g.children().iter().find_map(|c| match c {
        usvg::Node::Group(cg) if is_draw_here(cg.id()) => Some(cg.as_ref()),
        usvg::Node::Group(cg) => find_draw_here(cg),
        _ => None,
    })
}

/// Decide what the drawing is and check that painting it fits the budget.
pub(crate) fn plan(tree: &usvg::Tree, limits: &Limits) -> Result<Plan, ProcessError> {
    let mode = match find_draw_here(tree.root()) {
        Some(g) if has_ink(g) => Mode::DrawHere,
        _ => Mode::Full,
    };
    let mut survey = Survey {
        mode,
        facts: Facts::default(),
        segments: 0,
        clips: 0,
        clip_renders: 0,
        dashes: 0.0,
    };
    survey.group(tree.root(), 1.0, mode == Mode::Full);
    if survey.segments > limits.max_svg_segments {
        return Err(ProcessError::TooComplex("too many path segments"));
    }
    if survey.clips > limits.max_svg_clips || survey.clip_renders > MAX_CLIP_RENDERS {
        return Err(ProcessError::TooComplex("too many clipping paths"));
    }
    if survey.dashes > limits.max_svg_dashes as f64 {
        return Err(ProcessError::TooComplex("dashed outlines are too fine"));
    }
    Ok(Plan {
        mode,
        facts: survey.facts,
    })
}

/// The first pass: facts and costs, nothing rasterised.
struct Survey {
    mode: Mode,
    facts: Facts,
    /// Path verbs painted (stroke outlines weighted 5x: they are bigger) and in clips.
    segments: usize,
    /// Clipped groups: each holds a mask while its content is painted.
    clips: usize,
    /// Clip paths rendered (one per clipped group and per clip path on a clip path).
    clip_renders: usize,
    /// Estimated dash segments.
    dashes: f64,
}

impl Survey {
    /// `active`: content here is part of the drawing (inside `draw-here` in that mode).
    fn group(&mut self, g: &usvg::Group, parent_opacity: f32, active: bool) {
        let opacity = parent_opacity * g.opacity().get();
        if active {
            if !g.filters().is_empty() {
                self.facts.filters = true;
            }
            if g.mask().is_some() {
                self.facts.clip_or_mask = true;
            }
            if g.clip_path().is_some() {
                self.facts.clip_or_mask = true;
                self.clips += 1;
            }
            let mut clip = g.clip_path();
            while let Some(cp) = clip {
                self.clip_renders += 1;
                self.segments = self.segments.saturating_add(segments(cp.root()));
                clip = cp.clip_path();
                if self.clip_renders > MAX_CLIP_RENDERS {
                    break;
                }
            }
        }
        for child in g.children() {
            match child {
                usvg::Node::Group(cg) => {
                    if is_template_layer(cg.id()) {
                        if active && has_paint(cg) {
                            self.facts.guides_dropped = true;
                        }
                        continue;
                    }
                    let active = active || (self.mode == Mode::DrawHere && is_draw_here(cg.id()));
                    self.group(cg, opacity, active);
                }
                usvg::Node::Path(p) if active && p.is_visible() => self.path(p, opacity),
                _ => {}
            }
        }
    }

    fn path(&mut self, p: &usvg::Path, opacity: f32) {
        let verbs = p.data().len();
        let fill = p.fill().map(|f| (f.paint(), f.opacity().get(), 1));
        let stroke = p.stroke().map(|s| (s.paint(), s.opacity().get(), 5));
        for (paint, paint_opacity, weight) in fill.into_iter().chain(stroke) {
            if is_template_colour(paint) {
                self.facts.guides_dropped = true;
                continue;
            }
            // Stroke outlines are bigger than the path.
            self.segments = self.segments.saturating_add(verbs.saturating_mul(weight));
            let (rgb, a) = paint_colour(paint);
            if !matches!(paint, usvg::Paint::Color(_)) {
                self.facts.gradient = true;
            }
            if opacity * paint_opacity * a < 0.999 {
                self.facts.semi = true;
            }
            self.facts.colours.insert(rgb);
        }
        let Some(stroke) = p.stroke().filter(|s| !is_template_colour(s.paint())) else {
            return;
        };
        self.facts.strokes += 1;
        if let Some(dashes) = stroke.dasharray() {
            let period: f32 = dashes.iter().sum();
            if period > 0.0 {
                // In the path's own units; the control polygon is at least as long as
                // the path. Each period has `dashes.len() / 2` dashes.
                let pts = p.data().points();
                let len: f32 = pts.windows(2).map(|w| w[0].distance(w[1])).sum();
                let per_period = dashes.len().div_ceil(2) as f64;
                self.dashes += (len / period) as f64 * per_period + 1.0;
            }
        }
    }
}

/// Path verbs in a clip path's content.
fn segments(g: &usvg::Group) -> usize {
    g.children().iter().fold(0usize, |n, c| {
        n.saturating_add(match c {
            usvg::Node::Group(cg) => segments(cg),
            usvg::Node::Path(p) => p.data().len(),
            _ => 0,
        })
    })
}

/// Paint the planned drawing at `side`x`side` px. `canvas` maps the tree's user space
/// into 0..100.
pub(crate) fn paint(
    tree: &usvg::Tree,
    plan: &Plan,
    canvas: Transform,
    side: u32,
    max_travel: u32,
) -> Result<Painted, ProcessError> {
    let mut painter = Painter {
        mode: plan.mode,
        pm: Pixmap::new(side, side)
            .ok_or(ProcessError::Internal("could not allocate the SVG raster"))?,
        px: Transform::from_scale(side as f32 / 100.0, side as f32 / 100.0),
        side,
        multi: plan.facts.multi_colour(),
        shapes: Vec::new(),
        travel: 0.0,
        max_travel: f64::from(max_travel),
    };
    painter.group(tree.root(), canvas, 1.0, None, plan.mode == Mode::Full)?;
    // Paints are black (ink) or white (cut-out), so a pixel is ink when it is mostly
    // opaque and mostly black: an edge between a white detail and a dark shape is
    // resolved at 50%, like any other edge.
    let ink = painter
        .pm
        .pixels()
        .iter()
        .map(|p| p.alpha() >= 128 && p.demultiply().red() < 128)
        .collect();
    Ok(Painted {
        ink,
        shapes: painter.shapes,
    })
}

pub(crate) fn luma(c: [u8; 3]) -> f32 {
    (0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32) / 255.0
}

struct Painter {
    mode: Mode,
    pm: Pixmap,
    /// 0..100 space -> pixels.
    px: Transform,
    side: u32,
    multi: bool,
    shapes: Vec<Shape>,
    /// Vertical travel of the outlines painted so far, in units.
    travel: f64,
    max_travel: f64,
}

/// The rasteriser's work for `path` (0..100 space): the vertical distance its outline
/// travels, in units (a full-height edge is 100). Each unit is a fixed number of
/// scanlines at a given raster size, and every scanline an edge crosses is work, so
/// 5000 short segments are cheap and 5000 full-height ones are not. Measured on the
/// control polygon, which is at least as long as the curve.
pub(crate) fn travel(path: &tiny_skia::Path) -> f64 {
    let (mut total, mut start, mut cur) = (0f64, 0f32, 0f32);
    let step = |from: f32, to: f32| (to as f64 - from as f64).abs();
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                total += step(cur, start);
                start = p.y;
                cur = p.y;
            }
            PathSegment::LineTo(p) => {
                total += step(cur, p.y);
                cur = p.y;
            }
            PathSegment::QuadTo(c, p) => {
                total += step(cur, c.y) + step(c.y, p.y);
                cur = p.y;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                total += step(cur, c1.y) + step(c1.y, c2.y) + step(c2.y, p.y);
                cur = p.y;
            }
            PathSegment::Close => {
                total += step(cur, start);
                cur = start;
            }
        }
    }
    total + step(cur, start)
}

impl Painter {
    /// Count `path`'s rasterising work against the budget.
    fn charge(&mut self, path: &tiny_skia::Path) -> Result<(), ProcessError> {
        self.travel += travel(path);
        if self.travel > self.max_travel {
            return Err(ProcessError::TooComplex(
                "the outlines are too long to draw (lines crossing the whole drawing)",
            ));
        }
        Ok(())
    }

    fn group(
        &mut self,
        g: &usvg::Group,
        parent_ts: Transform,
        parent_opacity: f32,
        parent_clip: Option<&Mask>,
        active: bool,
    ) -> Result<(), ProcessError> {
        // `ts` maps this group's user space into 0..100.
        let ts = parent_ts.pre_concat(g.transform());
        let opacity = parent_opacity * g.opacity().get();
        let own_clip = match g.clip_path() {
            Some(cp) if active => Some(self.clip_mask(cp, ts, parent_clip)?),
            _ => None,
        };
        let clip = own_clip.as_ref().or(parent_clip);
        for child in g.children() {
            match child {
                usvg::Node::Group(cg) => {
                    if is_template_layer(cg.id()) {
                        continue;
                    }
                    let active = active || (self.mode == Mode::DrawHere && is_draw_here(cg.id()));
                    self.group(cg, ts, opacity, clip, active)?;
                }
                usvg::Node::Path(p) if active && p.is_visible() => {
                    self.path(p, ts, opacity, clip)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn path(
        &mut self,
        p: &usvg::Path,
        ts: Transform,
        opacity: f32,
        clip: Option<&Mask>,
    ) -> Result<(), ProcessError> {
        let stroke_first = p.paint_order() == usvg::PaintOrder::StrokeAndFill;
        if stroke_first {
            self.stroke(p, ts, opacity, clip)?;
        }
        if let Some(fill) = p.fill()
            && !is_template_colour(fill.paint())
            && let Some(path) = p.data().clone().transform(ts)
        {
            let (rgb, a) = paint_colour(fill.paint());
            let rule = match fill.rule() {
                usvg::FillRule::NonZero => tiny_skia::FillRule::Winding,
                usvg::FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
            };
            self.fill(&path, rgb, opacity * fill.opacity().get() * a, rule, clip)?;
            self.add_candidate(&path, rgb);
        }
        if !stroke_first {
            self.stroke(p, ts, opacity, clip)?;
        }
        Ok(())
    }

    fn stroke(
        &mut self,
        p: &usvg::Path,
        ts: Transform,
        opacity: f32,
        clip: Option<&Mask>,
    ) -> Result<(), ProcessError> {
        let Some(stroke) = p.stroke() else {
            return Ok(());
        };
        if is_template_colour(stroke.paint()) {
            return Ok(());
        }
        let (rgb, a) = paint_colour(stroke.paint());
        // Stroke in user space, then map (keeps non-uniform scales right), at the
        // resolution it will be rasterised at.
        let res = tiny_skia::PathStroker::compute_resolution_scale(&ts.post_concat(self.px));
        let Some(outline) = p
            .data()
            .stroke(&stroke.to_tiny_skia(), res.clamp(0.01, 1000.0))
            .and_then(|o| o.transform(ts))
        else {
            return Ok(());
        };
        self.fill(
            &outline,
            rgb,
            opacity * stroke.opacity().get() * a,
            tiny_skia::FillRule::Winding,
            clip,
        )?;
        self.add_candidate(&outline, rgb);
        Ok(())
    }

    fn add_candidate(&mut self, path: &tiny_skia::Path, rgb: [u8; 3]) {
        self.shapes.push(Shape {
            path: path.clone(),
            light: self.multi && luma(rgb) >= LIGHT_LUMA,
        });
    }

    /// Paint `path` in black, or in white when it is a light paint in a multi-colour
    /// drawing (a cut-out). Only that distinction matters for the truth.
    fn fill(
        &mut self,
        path: &tiny_skia::Path,
        rgb: [u8; 3],
        alpha: f32,
        rule: tiny_skia::FillRule,
        clip: Option<&Mask>,
    ) -> Result<(), ProcessError> {
        self.charge(path)?;
        let mut paint = Paint::default();
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        let shade = if self.multi && luma(rgb) >= LIGHT_LUMA {
            255
        } else {
            0
        };
        paint.set_color_rgba8(shade, shade, shade, a);
        paint.anti_alias = true;
        self.pm.fill_path(path, &paint, rule, self.px, clip);
        Ok(())
    }

    /// A clip path as a coverage mask, intersected with its own nested clip paths and
    /// with `parent`. Iterative over nested clips: at most two masks live at once.
    fn clip_mask(
        &mut self,
        cp: &usvg::ClipPath,
        ts: Transform,
        parent: Option<&Mask>,
    ) -> Result<Mask, ProcessError> {
        let side = self.side;
        let alloc = || {
            Mask::new(side, side).ok_or(ProcessError::Internal("could not allocate a clip mask"))
        };
        let mut acc: Option<Mask> = None;
        let mut current = Some((cp, ts));
        while let Some((cp, ts)) = current {
            let ts = ts.pre_concat(cp.transform());
            let mut mask = alloc()?;
            self.add_to_mask(cp.root(), ts, &mut mask)?;
            acc = Some(match acc {
                None => mask,
                Some(mut a) => {
                    intersect(&mut a, &mask);
                    a
                }
            });
            current = cp.clip_path().map(|inner| (inner, ts));
        }
        let mut mask = match acc {
            Some(m) => m,
            None => alloc()?,
        };
        if let Some(p) = parent {
            intersect(&mut mask, p);
        }
        Ok(mask)
    }

    fn add_to_mask(
        &mut self,
        g: &usvg::Group,
        parent_ts: Transform,
        mask: &mut Mask,
    ) -> Result<(), ProcessError> {
        let ts = parent_ts.pre_concat(g.transform());
        for c in g.children() {
            match c {
                usvg::Node::Group(cg) => self.add_to_mask(cg, ts, mask)?,
                usvg::Node::Path(p) if p.is_visible() => {
                    let rule = match p.fill().map(usvg::Fill::rule) {
                        Some(usvg::FillRule::EvenOdd) => tiny_skia::FillRule::EvenOdd,
                        _ => tiny_skia::FillRule::Winding,
                    };
                    if let Some(path) = p.data().clone().transform(ts) {
                        self.charge(&path)?;
                        mask.fill_path(&path, rule, true, self.px);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn intersect(acc: &mut Mask, other: &Mask) {
    for (a, b) in acc.data_mut().iter_mut().zip(other.data()) {
        *a = ((*a as u16 * *b as u16) / 255) as u8;
    }
}

/// A solid colour standing in for the paint, and extra alpha. Gradients become the
/// average of their stops; patterns black.
fn paint_colour(paint: &usvg::Paint) -> ([u8; 3], f32) {
    let avg = |stops: &[usvg::Stop]| -> ([u8; 3], f32) {
        if stops.is_empty() {
            return ([0, 0, 0], 1.0);
        }
        let n = stops.len() as f32;
        let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
        for s in stops {
            r += s.color().red as f32;
            g += s.color().green as f32;
            b += s.color().blue as f32;
            a += s.opacity().get();
        }
        ([(r / n) as u8, (g / n) as u8, (b / n) as u8], a / n)
    };
    match paint {
        usvg::Paint::Color(c) => ([c.red, c.green, c.blue], 1.0),
        usvg::Paint::LinearGradient(lg) => avg(lg.stops()),
        usvg::Paint::RadialGradient(rg) => avg(rg.stops()),
        usvg::Paint::Pattern(_) => ([0, 0, 0], 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_ids_are_recognised_loosely() {
        for id in [
            "draw-here",
            "Draw_here",
            "draw here",
            "draw-here-2",
            "DRAW-HERE",
        ] {
            assert!(is_draw_here(id), "{id}");
        }
        for id in ["", "draw", "draw-hereafter", "drawhere"] {
            assert!(!is_draw_here(id), "{id}");
        }
        for id in [
            "guides",
            "Guides_1_",
            "reference-default",
            "reference_smile",
            "reference",
        ] {
            assert!(is_template_layer(id), "{id}");
        }
        for id in ["", "guidesomething", "references", "my-guides"] {
            assert!(!is_template_layer(id), "{id}");
        }
    }
}
