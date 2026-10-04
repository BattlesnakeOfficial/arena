//! Bounds an untrusted SVG before usvg converts it.
//!
//! usvg (and the CSS engine it uses, simplecss) have no limits of their own on depth,
//! expansion or work, and a thread that overflows its stack aborts the whole process
//! (`catch_unwind` cannot catch that). Everything here is iterative and runs on the
//! parsed XML before usvg sees it:
//!
//! * [`depth_upper_bound`]: a byte scan that bounds element nesting before roxmltree's
//!   recursive tokenizer runs (about 5000 nested tags overflow a 2 MiB stack).
//! * [`scan`]: counts and lint facts, the CSS cost, and two **reference graphs**:
//!   - usvg's parser copies the target of every `<use>` into the tree (and a `<use>`
//!     inside a target is copied again with it);
//!   - its converter then follows references: `fill`/`stroke` paint servers
//!     (patterns), `clip-path`, `mask`, `filter` (and `feImage` hrefs), markers
//!     (instantiated at every vertex) and `<use>`, each converting the target's content
//!     nested inside the referencing element. Definitions (`<defs>`, patterns, masks,
//!     ...) are converted only through references, and so is anything inside a
//!     gradient, stop or filter primitive, or (through a `<use>`) inside an element
//!     usvg doesn't parse. Where an element sits never hides it: see `Role`.
//!
//!   From those:
//!   - a **cycle** of three or more references (pattern A fills with B, B with C, C
//!     with A), or a pattern whose content inherits a fill that points back at it,
//!     recurses forever: usvg only breaks one- and two-step cycles. Verified to abort the
//!     process even on a 64 MiB stack, with a 600-byte file. Rejected.
//!   - the **nesting** along a chain of references multiplies: 64 patterns, each with 60
//!     groups inside, is about 4,000 levels deep although no element is nested more than
//!     64 deep. Bounded by `Limits::max_svg_nesting`; deep enough to need a big stack
//!     (see `process_upload`).
//!   - the **expansion** multiplies too: every `<use>` copies its target, every path
//!     with an objectBoundingBox pattern gets its own copy of the pattern's content, and
//!     every vertex its own marker (on every shape, rects, circles and ellipses
//!     included). A `<use>` copy inherits paint and markers from the `<use>` and its
//!     ancestors, so those references count once per shape (or vertex) of the expanded
//!     target. Bounded by `Limits::max_svg_expansion`.
//!   - the **path segments** usvg makes, every copy included, bounded by
//!     `Limits::max_svg_expanded_segments` before usvg allocates them. Arcs become more
//!     cubics the bigger their radius ([`arc_cubics`]); one that would become more than
//!     [`MAX_ARC_CUBICS`] is rejected outright.
//!
//!   References can come from attributes, `style` attributes and `<style>` sheets, and
//!   `fill`, `stroke` and markers are inherited from every ancestor, gradients, stops
//!   and filter primitives included. Stylesheet rules are matched with the
//!   same CSS engine and element view as usvg's, and every candidate value counts (not
//!   just the cascade winner), so the graph is a superset of what usvg follows.
//! * `<tref>`: the text usvg copies into each one, and its scan for each one's target.
//! * The **CSS cost**, bounded by `Limits::max_css_work` before simplecss runs:
//!   - simplecss computes a line and column from the start of the text whenever a value
//!     ends, so parsing is quadratic: 16,000 declarations (144 KB) take 1.2 s. usvg
//!     parses every copy's `style` attribute, and we tokenise every non-inert
//!     original's (to read its references), even inside an element usvg never parses;
//!   - usvg matches every rule against every element (and `<use>` copy), and a
//!     descendant combinator backtracks through every ancestor, so
//!     `x g g g g g g g g g g` over 60 nested groups is about 10^12 steps.

use std::collections::HashMap;

use simplecss::{SelectorToken, SelectorTokenizer};
use usvg::roxmltree::{self, Node};

use super::{Limits, ProcessError};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Most own references (attribute, `style` and stylesheet values naming an element).
const MAX_REFS: usize = 20_000;
/// Most edges in a reference graph (children, own and inherited references).
const MAX_EDGES: usize = 400_000;
/// Most compound selectors in one CSS selector (simplecss matches them recursively).
const MAX_SELECTOR_COMPONENTS: usize = 32;

/// Most text all `<tref>`s may copy, in bytes.
const MAX_TREF_TEXT_BYTES: u64 = 1 << 20;
/// Most `<tref>`s times XML nodes: usvg scans the document for each one's target.
const MAX_TREF_WORK: u64 = 20_000_000;

const TOO_MANY_REFS: ProcessError = ProcessError::TooComplex("too many references");
const TREF_TOO_MUCH: ProcessError = ProcessError::TooComplex("too much text is copied (<tref>)");
const CSS_TOO_COMPLEX: ProcessError = ProcessError::TooComplex("the CSS is too complex");
const EXPANDS: ProcessError =
    ProcessError::TooComplex("references expand to too many shapes (<use>, patterns or markers)");

/// Input facts the prescan noticed, for lints.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Facts {
    /// `<text>`, `<tspan>` or `<textPath>`: not converted (no fonts).
    pub text: bool,
    /// `<image>` elements: never loaded.
    pub images: usize,
    /// Scripts, event handlers, embedded HTML, animations or external links.
    pub active: bool,
    /// `<filter>` definitions: ignored when painting.
    pub filters: bool,
}

/// Conservative upper bound on element nesting depth, computed without recursion.
/// Skips comments, CDATA, processing instructions and the DOCTYPE (with its internal
/// subset), and honours quotes inside tags, so `a="/>"` can't fake a self-closing tag.
/// Over-estimates are fine: it is only used to reject.
///
/// `None` when the markup can't be bounded: a DOCTYPE or `<!` declaration that is
/// malformed or unterminated (roxmltree rejects those too). Skipping more than roxmltree
/// does would hide elements from the count, so the DOCTYPE is read with roxmltree
/// 0.21.1's own grammar ([`doctype_end`]), and anything else is refused.
pub(crate) fn depth_upper_bound(b: &[u8]) -> Option<usize> {
    let n = b.len();
    let (mut i, mut depth, mut max) = (0usize, 0usize, 0usize);
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &b[i..];
        if rest.starts_with(b"<!--") {
            i = after(b, i + 4, b"-->").unwrap_or(n);
        } else if rest.starts_with(b"<![CDATA[") {
            i = after(b, i + 9, b"]]>").unwrap_or(n);
        } else if rest.starts_with(b"<?") {
            i = after(b, i + 2, b"?>").unwrap_or(n);
        } else if rest.starts_with(b"<!DOCTYPE") {
            i = doctype_end(b, i + 9)?;
        } else if rest.starts_with(b"<!") {
            // roxmltree accepts no other declaration outside the DOCTYPE.
            return None;
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            i = after(b, i + 2, b">").unwrap_or(n);
        } else {
            let mut j = i + 1;
            let mut quote = 0u8;
            while j < n {
                let c = b[j];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            let self_closing = j < n && b[j - 1] == b'/';
            if !self_closing {
                depth += 1;
                max = max.max(depth);
            }
            i = j + 1;
        }
    }
    Some(max)
}

/// The index just past the first `pat` at or after `from`.
fn after(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| from + p + pat.len())
}

/// The index just past a DOCTYPE whose `<!DOCTYPE` ends before `j`, read as roxmltree
/// 0.21.1 reads it (`tokenizer.rs`, `parse_doctype`):
///
/// * before the internal subset, quoted literals (the external ID) are skipped whole,
///   up to `[` or `>`;
/// * in the subset: `<!ENTITY ...>` with quoted values, comments, processing
///   instructions, and `<!ELEMENT`, `<!ATTLIST` and `<!NOTATION`, which roxmltree skips
///   to the first `>` **without** honouring quotes (so `<!ATTLIST a b CDATA ">">` ends
///   at the first `>`, and so must we); then `]`, spaces and `>`.
///
/// `None` for anything else, or at the end of the input: roxmltree fails there too.
fn doctype_end(b: &[u8], mut j: usize) -> Option<usize> {
    let n = b.len();
    let quoted = |j: usize| -> Option<usize> {
        let q = b[j];
        b.get(j + 1..)?
            .iter()
            .position(|&c| c == q)
            .map(|p| j + 1 + p + 1)
    };
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\r' | b'\n');
    loop {
        match *b.get(j)? {
            b'"' | b'\'' => j = quoted(j)?,
            b'>' => return Some(j + 1),
            b'[' => break,
            _ => j += 1,
        }
    }
    j += 1;
    loop {
        while j < n && space(b[j]) {
            j += 1;
        }
        let rest = b.get(j..)?;
        if rest.starts_with(b"<!ENTITY") {
            j += 8;
            loop {
                match *b.get(j)? {
                    b'"' | b'\'' => j = quoted(j)?,
                    b'>' => break,
                    _ => j += 1,
                }
            }
            j += 1;
        } else if rest.starts_with(b"<!--") {
            j = after(b, j + 4, b"-->")?;
        } else if rest.starts_with(b"<?") {
            j = after(b, j + 2, b"?>")?;
        } else if rest.starts_with(b"<!ELEMENT")
            || rest.starts_with(b"<!ATTLIST")
            || rest.starts_with(b"<!NOTATION")
        {
            j = after(b, j, b">")?;
        } else if rest.starts_with(b"]") {
            j += 1;
            while j < n && space(b[j]) {
                j += 1;
            }
            return (b.get(j) == Some(&b'>')).then_some(j + 1);
        } else {
            return None;
        }
    }
}

/// How a reference behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RefKind {
    /// `fill` / `stroke`: inherited by descendants.
    Paint,
    /// `marker-start/mid/end` (and the `marker` shorthand): inherited, and instantiated
    /// at every vertex of every shape (paths, lines, polylines, polygons, rects, circles
    /// and ellipses).
    Marker,
    /// `clip-path`, `mask`, `filter`, `href` and anything else: not inherited.
    Other,
}

impl RefKind {
    fn of_property(name: &str) -> RefKind {
        match name {
            "fill" | "stroke" => RefKind::Paint,
            "marker" | "marker-start" | "marker-mid" | "marker-end" => RefKind::Marker,
            _ => RefKind::Other,
        }
    }

    fn inherited(self) -> bool {
        matches!(self, RefKind::Paint | RefKind::Marker)
    }
}

/// How usvg treats an element, decided by its own tag only. Being inside a gradient, a
/// stop, a filter primitive or an element usvg never parses hides nothing: usvg parses
/// every child of a gradient, stop or primitive and resolves a reference to an id
/// anywhere in its tree, and a `<use>` copies any SVG element in the document, even one
/// inside a `<foreignObject>` or a foreign-namespace element. So a pattern nested in a
/// gradient is converted when something references it, and its content inherits paint
/// from the gradient (usvg looks for `fill`, `stroke` and markers on every ancestor,
/// whatever its tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Converted as graphics: children, references and inherited references count.
    Graphic,
    /// Gradients, stops and filter primitives: only their non-inherited references (an
    /// `href`) are followed, and their children are converted only through references.
    /// They aren't drawn, so their own paint and markers aren't followed, but their
    /// descendants inherit them.
    HrefOnly,
    /// Never parsed: non-SVG namespaces, `foreignObject`, `style`, `script`, metadata.
    /// Their children are reached only through a `<use>`.
    Inert,
}

struct El<'a, 'input> {
    node: Node<'a, 'input>,
    parent: Option<usize>,
    /// Root `<svg>` is 1.
    depth: usize,
    /// One past the last descendant, in document order.
    end: usize,
    role: Role,
    /// Only converted through references (`<defs>`, patterns, masks, ...).
    definition: bool,
    /// The `href` and `xlink:href` of a `<use>`.
    use_hrefs: [Option<&'a str>; 2],
    /// Own references: kind and target id.
    refs: Vec<(RefKind, &'a str)>,
    /// Nearest proper ancestor with inherited (paint or marker) references.
    inherit_from: Option<usize>,
    /// A shape usvg turns into a path (and so paints, and puts markers on).
    shape: bool,
    /// Upper bound on the segments of that path, which is also its marker vertices:
    /// [`shape_segments`].
    segments: u64,
    /// Upper bound on simplecss's work parsing the `style` attribute.
    style_cost: u64,
    /// Nodes the parser visits for this element: itself, and the comments and text
    /// before it (matching `:first-child` scans back over them).
    visit_cost: u64,
}

/// Element names that run code, embed other documents or animate attributes (SMIL can
/// set an `href` to `javascript:`).
const ACTIVE: [&str; 15] = [
    "script",
    "foreignObject",
    "iframe",
    "embed",
    "object",
    "audio",
    "video",
    "canvas",
    "animate",
    "set",
    "animateMotion",
    "animateTransform",
    "animateColor",
    "discard",
    "handler",
];

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Illustrator's "Save As SVG" with editing data: `<switch><foreignObject
/// requiredExtensions="&ns_ai;">` holding a reference to its private data, then the
/// drawing as the fallback. Not web content (no HTML inside), and usvg skips any branch
/// with `requiredExtensions`.
fn illustrator_fallback(node: Node) -> bool {
    node.tag_name().name() == "foreignObject"
        && node.has_attribute("requiredExtensions")
        && node
            .parent_element()
            .is_some_and(|p| p.tag_name().name() == "switch")
        && !node
            .descendants()
            .any(|d| d.tag_name().namespace() == Some(XHTML_NS))
}

/// Definitions that usvg resolves by reference (counted against `max_svg_defs`).
const DEFS: [&str; 8] = [
    "clipPath",
    "mask",
    "pattern",
    "marker",
    "symbol",
    "linearGradient",
    "radialGradient",
    "filter",
];

/// Elements whose `href` usvg follows to another element.
const HREF_FOLLOWED: [&str; 8] = [
    "use",
    "pattern",
    "linearGradient",
    "radialGradient",
    "filter",
    "feImage",
    "textPath",
    "tref",
];

/// Upper bound on simplecss's work parsing `text`: the end of every declaration (one
/// `:` each) and every rule (one `{`) computes a line and column by scanning from the
/// start of the text.
fn parse_cost(text: &str) -> u64 {
    let ends = text.bytes().filter(|b| matches!(b, b':' | b'{')).count() as u64;
    (ends + 1).saturating_mul(text.len() as u64 + 1)
}

/// Elements usvg turns into a path: they paint, and markers go on their vertices (usvg's
/// `marker::is_valid` doesn't look at the tag).
const SHAPES: [&str; 7] = [
    "path", "rect", "circle", "ellipse", "line", "polyline", "polygon",
];

/// Most cubics one arc may become. usvg's arcs (path `A` commands, circles, ellipses and
/// rounded corners) are split by kurbo into more cubics the bigger the radius, with no
/// limit: a 100-byte path with a radius of 1e50 is billions of cubics, and svgtypes
/// drains each arc's cubics with `Vec::remove(0)` (quadratic). An ordinary arc needs at
/// most 4 or 5 per turn; 64 is a radius of about 6·10^9 units.
const MAX_ARC_CUBICS: u64 = 64;

const ARC_TOO_BIG: ProcessError =
    ProcessError::TooComplex("a curve or circle is too big to draw (a huge radius)");

/// The namespaces usvg reads presentation and geometry attributes from (its
/// `parse_svg_attribute`).
fn svg_attr_ns(ns: Option<&str>) -> bool {
    matches!(
        ns,
        None | Some(SVG_NS) | Some(XLINK_NS) | Some("http://www.w3.org/XML/1998/namespace")
    )
}

/// The values of `name` that usvg may read on `node` (in any of its namespaces).
fn svg_attrs<'a>(node: Node<'a, '_>, name: &'a str) -> impl Iterator<Item = &'a str> {
    node.attributes()
        .filter(move |a| a.name() == name && svg_attr_ns(a.namespace()))
        .map(|a| a.value())
}

/// Upper bound on the cubics kurbo 0.13.1 (`Arc::append_iter`, at the 0.1 tolerance
/// svgtypes and usvg ask for) makes for `turns` (at most 1) of an ellipse whose larger
/// radius is `r`: `ceil(max(4, (1.1163·r/0.1)^(1/6))·turns)`, plus one for rounding.
/// `u64::MAX` when `r` is infinite (kurbo then never stops).
fn arc_cubics(r: f64, turns: f64) -> u64 {
    let per_turn = (1.1163 * r / 0.1).powf(1.0 / 6.0).max(4.0);
    let n = (per_turn * turns).ceil() + 1.0;
    if n < 1e18 { n as u64 } else { u64::MAX }
}

/// What the prescan knows about lengths in other units than user units.
#[derive(Clone, Copy)]
struct Units {
    /// The size `%` is relative to: the root's `viewBox` (`None` when it isn't known for
    /// sure: nested `<svg>`s and `<symbol>`s have their own).
    percent_base: Option<f64>,
    /// The font size `em` is relative to: usvg's default (12) when nothing in the file
    /// sets a font size, otherwise unknown.
    font_size: Option<f64>,
}

/// Upper bound on `value` (an SVG length) in user units, as usvg converts it (96 dpi),
/// or infinity when the prescan can't know it.
fn length_bound(len: svgtypes::Length, units: Units) -> f64 {
    use svgtypes::LengthUnit as U;
    let n = len.number.abs();
    let per = match len.unit {
        U::None | U::Px => Some(1.0),
        U::In => Some(96.0),
        U::Cm => Some(96.0 / 2.54),
        U::Mm => Some(96.0 / 25.4),
        U::Pt => Some(96.0 / 72.0),
        U::Pc => Some(96.0 / 6.0),
        U::Percent => units.percent_base.map(|b| b / 100.0),
        U::Em => units.font_size,
        U::Ex => units.font_size.map(|f| f / 2.0),
    };
    per.map_or(f64::INFINITY, |p| n * p)
}

/// Upper bound on the radius usvg gives `node`'s attribute `name` (absent, unparsable
/// and negative values aren't used: 0).
fn radius_bound(node: Node, name: &str, units: Units) -> f64 {
    svg_attrs(node, name)
        .filter_map(|v| v.parse::<svgtypes::Length>().ok())
        .filter(|l| !l.number.is_sign_negative())
        .map(|l| length_bound(l, units))
        .fold(0.0, f64::max)
}

/// The `units` facts for a document (see [`Units`]).
fn units_of(root: Node, els: &[El], sheets: &[&str]) -> Units {
    let sets_font = |s: &str| s.contains("font");
    let fonts = sheets.iter().any(|s| sets_font(s))
        || els.iter().any(|e| {
            e.node
                .attributes()
                .any(|a| sets_font(a.name()) || (a.name() == "style" && sets_font(a.value())))
        });
    let font_size = (!fonts).then_some(12.0);
    // usvg resolves `%` against the root's viewBox, or its size when the viewBox isn't
    // valid: the largest of both bounds it. Nested `<svg>`s and `<symbol>`s set their
    // own, which the prescan doesn't follow.
    let nested = els
        .iter()
        .skip(1)
        .any(|e| matches!(e.node.tag_name().name(), "svg" | "symbol"));
    let percent_base = (!nested).then(|| {
        let mut base: f64 = 100.0; // usvg's default size
        for vb in svg_attrs(root, "viewBox").filter_map(|v| v.parse::<svgtypes::ViewBox>().ok()) {
            // As tiny-skia's rect has it: the right edge minus the left, in f32.
            let (x, y, w, h) = (vb.x as f32, vb.y as f32, vb.w as f32, vb.h as f32);
            for side in [vb.w, vb.h, f64::from((w + x) - x), f64::from((h + y) - y)] {
                base = base.max(side.abs());
            }
        }
        let mut size = base;
        for name in ["width", "height"] {
            for len in svg_attrs(root, name).filter_map(|v| v.parse::<svgtypes::Length>().ok()) {
                let side = match len.unit {
                    svgtypes::LengthUnit::Percent => len.number.abs() / 100.0 * base,
                    _ => length_bound(
                        len,
                        Units {
                            percent_base: None,
                            font_size,
                        },
                    ),
                };
                size = size.max(side);
            }
        }
        size
    });
    Units {
        percent_base,
        font_size,
    }
}

/// Upper bound on the segments usvg makes for shape `node` (`name`), which is also the
/// number of its marker vertices: one or two per path command or point, and every arc
/// as kurbo splits it ([`arc_cubics`]). Rejects an arc that would become more than
/// [`MAX_ARC_CUBICS`] cubics.
fn shape_segments(node: Node, name: &str, units: Units) -> Result<u64, ProcessError> {
    let quarters = |r: f64| -> Result<u64, ProcessError> {
        let n = arc_cubics(r, 0.25);
        if n > MAX_ARC_CUBICS {
            return Err(ARC_TOO_BIG);
        }
        Ok(4 * n)
    };
    Ok(match name {
        "path" => svg_attrs(node, "d").try_fold(0u64, |acc, d| {
            path_segments(d).map(|n| acc.saturating_add(n))
        })?,
        "polyline" | "polygon" => {
            svg_attrs(node, "points").fold(2u64, |acc, p| acc.saturating_add(p.len() as u64))
        }
        "line" => 2,
        "circle" => 2 + quarters(radius_bound(node, "r", units))?,
        // An `rx` without a `ry` is used for both, and the other way round.
        "ellipse" => {
            2 + quarters(radius_bound(node, "rx", units).max(radius_bound(node, "ry", units)))?
        }
        "rect" => {
            // Rounded corners are clamped to half the width and height.
            let r = radius_bound(node, "rx", units).max(radius_bound(node, "ry", units));
            let half =
                radius_bound(node, "width", units).max(radius_bound(node, "height", units)) / 2.0;
            let r = r.min(half);
            if r > 0.0 { 6 + quarters(r)? } else { 6 }
        }
        _ => 0,
    })
}

/// Upper bound on the segments usvg makes from a path's `d`, read with svgtypes'
/// parser as usvg reads it (up to the first error): two per command (an implicit move
/// may come first) and every arc as kurbo splits it.
///
/// An arc's split depends on its larger radius after usvg scales both radii up to reach
/// the end point (by at most half the chord over the smaller radius). The chord comes
/// from tracking the current point; svgtypes continues from where the previous arc's
/// last cubic ended, which is off the exact end point by rounding (up to about 1e-8 of
/// the radius, from a cancellation in kurbo's centre), so that slack is carried along.
fn path_segments(d: &str) -> Result<u64, ProcessError> {
    use svgtypes::PathSegment as S;
    // Current point and subpath start, each with how far svgtypes' may be from them.
    let (mut cur, mut cur_slack) = ((0f64, 0f64), 0f64);
    let (mut start, mut start_slack) = ((0f64, 0f64), 0f64);
    let mut total = 2u64;
    for seg in svgtypes::PathParser::from(d) {
        let Ok(seg) = seg else {
            break;
        };
        total = total.saturating_add(2);
        let (abs, end) = match seg {
            S::MoveTo { abs, x, y }
            | S::LineTo { abs, x, y }
            | S::CurveTo { abs, x, y, .. }
            | S::SmoothCurveTo { abs, x, y, .. }
            | S::Quadratic { abs, x, y, .. }
            | S::SmoothQuadratic { abs, x, y }
            | S::EllipticalArc { abs, x, y, .. } => (abs, (x, y)),
            S::HorizontalLineTo { abs, x } => (abs, (x, if abs { cur.1 } else { 0.0 })),
            S::VerticalLineTo { abs, y } => (abs, (if abs { cur.0 } else { 0.0 }, y)),
            S::ClosePath { .. } => {
                (cur, cur_slack) = (start, start_slack);
                continue;
            }
        };
        let (end, mut end_slack) = if abs {
            // A horizontal or vertical line keeps the other coordinate, with its slack.
            let keeps = matches!(seg, S::HorizontalLineTo { .. } | S::VerticalLineTo { .. });
            (end, if keeps { cur_slack } else { 0.0 })
        } else {
            ((cur.0 + end.0, cur.1 + end.1), cur_slack)
        };
        if let S::EllipticalArc { rx, ry, .. } = seg {
            let (rx, ry) = (rx.abs(), ry.abs());
            // kurbo draws a line when a radius is this small (`is_straight_line`).
            if rx > 1e-5 && ry > 1e-5 {
                let half = ((end.0 - cur.0).hypot(end.1 - cur.1) / 2.0) + cur_slack + end_slack;
                let r = rx.max(ry) * (half / rx.min(ry)).max(1.0);
                let n = arc_cubics(r, 1.0);
                if n > MAX_ARC_CUBICS {
                    return Err(ARC_TOO_BIG);
                }
                total = total.saturating_add(n);
                let magnitude = r + cur.0.abs() + cur.1.abs() + end.0.abs() + end.1.abs();
                end_slack += 1e-6 * magnitude;
            }
        }
        (cur, cur_slack) = (end, end_slack);
        if matches!(seg, S::MoveTo { .. }) {
            (start, start_slack) = (cur, cur_slack);
        }
    }
    Ok(total)
}

/// Prescan the parsed document. See the module docs.
pub(crate) fn scan(doc: &roxmltree::Document, limits: &Limits) -> Result<Facts, ProcessError> {
    let root = doc.root_element();
    if root.tag_name().name() != "svg" {
        return Err(ProcessError::InvalidSvg(
            "the root element is not <svg>".into(),
        ));
    }

    // ---- elements in document order: parents, depths, roles, facts, costs ----
    let total_nodes = doc.descendants().count();
    let mut index_of = vec![u32::MAX; total_nodes + 1];
    let mut els: Vec<El> = Vec::new();
    let mut facts = Facts::default();
    let (mut defs, mut uses) = (0usize, 0usize);
    let mut sheets: Vec<&str> = Vec::new();
    for node in root.descendants().filter(|n| n.is_element()) {
        let parent = node
            .parent_element()
            .and_then(|p| index_of.get(p.id().get_usize()).copied())
            .filter(|&i| i != u32::MAX)
            .map(|i| i as usize);
        let parent_el = parent.and_then(|p| els.get(p));
        let depth = parent_el.map_or(1, |p| p.depth + 1);
        if depth > limits.max_svg_depth {
            return Err(ProcessError::TooComplex("elements are nested too deeply"));
        }
        let name = node.tag_name().name();
        let svg_ns = matches!(node.tag_name().namespace(), None | Some(SVG_NS));
        // By the element's own tag, never its parent's: see `Role`.
        let role = if !svg_ns
            || matches!(
                name,
                "foreignObject" | "style" | "script" | "title" | "desc" | "metadata"
            ) {
            Role::Inert
        } else if matches!(name, "linearGradient" | "radialGradient" | "stop")
            || name.starts_with("fe")
        {
            Role::HrefOnly
        } else {
            Role::Graphic
        };
        // usvg reads every `style` element, in any namespace and anywhere.
        if name == "style"
            && matches!(node.attribute("type"), None | Some("text/css"))
            && let Some(text) = node.text()
        {
            sheets.push(text);
        }

        if svg_ns {
            match name {
                "text" | "tspan" | "textPath" => facts.text = true,
                "image" => facts.images += 1,
                "filter" => facts.filters = true,
                "use" => uses += 1,
                _ => {}
            }
            if ACTIVE.contains(&name) && !illustrator_fallback(node) {
                facts.active = true;
            }
            if DEFS.contains(&name) {
                defs += 1;
            }
        }
        for a in node.attributes() {
            if a.name().len() > 2 && a.name().starts_with("on") {
                facts.active = true;
            }
            // External links (`<a href>`, `<use href="file.svg#x">`, ...). Images are
            // counted separately and never loaded.
            if a.name() == "href" && name != "image" && !a.value().trim_start().starts_with('#') {
                facts.active = true;
            }
        }

        let use_hrefs = if role != Role::Inert && name == "use" {
            [node.attribute("href"), node.attribute((XLINK_NS, "href"))]
        } else {
            [None, None]
        };
        let preceding = node
            .prev_siblings()
            .skip(1)
            .take_while(|n| !n.is_element())
            .count() as u64;
        if let Some(slot) = index_of.get_mut(node.id().get_usize()) {
            *slot = els.len() as u32;
        }
        els.push(El {
            node,
            parent,
            depth,
            end: 0,
            role,
            definition: name == "defs" || DEFS.contains(&name),
            use_hrefs,
            refs: Vec::new(),
            inherit_from: None,
            shape: svg_ns && SHAPES.contains(&name),
            segments: 0,
            style_cost: node.attribute("style").map_or(0, parse_cost),
            visit_cost: 1 + preceding,
        });
    }
    if uses > limits.max_svg_uses {
        return Err(ProcessError::TooComplex("too many <use> elements"));
    }
    if defs > limits.max_svg_defs {
        return Err(ProcessError::TooComplex(
            "too many clip paths, masks, patterns, gradients or filters",
        ));
    }
    // Shapes anywhere, even in definitions or unparsed elements (a `<use>` may copy them).
    let units = units_of(root, &els, &sheets);
    for el in els.iter_mut().filter(|e| e.shape) {
        el.segments = shape_segments(el.node, el.node.tag_name().name(), units)?;
    }
    for (i, el) in els.iter_mut().enumerate() {
        el.end = i + 1;
    }
    for i in (0..els.len()).rev() {
        if let Some(p) = els[i].parent {
            els[p].end = els[p].end.max(els[i].end);
        }
    }
    let max_depth = els.iter().map(|e| e.depth).max().unwrap_or(1);

    // ---- ids (duplicates allowed: a reference may resolve to any of them). usvg also
    // takes `xml:id` and `svg:id`, so any namespace counts. ----
    let mut ids: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, el) in els.iter().enumerate() {
        for a in el.node.attributes().filter(|a| a.name() == "id") {
            ids.entry(a.value()).or_default().push(i);
        }
    }

    // ---- `<tref>`: usvg copies all the text inside the target (finding it by a scan of
    // the whole document) into every `<tref>` of a `<text>`. ----
    let trefs: Vec<usize> = (0..els.len())
        .filter(|&i| {
            let n = els[i].node;
            n.tag_name().name() == "tref" && matches!(n.tag_name().namespace(), None | Some(SVG_NS))
        })
        .collect();
    if !trefs.is_empty() {
        if (trefs.len() as u64).saturating_mul(total_nodes as u64) > MAX_TREF_WORK {
            return Err(TREF_TOO_MUCH);
        }
        // Text bytes in each element's subtree (children come after their parents).
        let mut text_in = vec![0u64; els.len()];
        for t in doc.descendants().filter(|n| n.is_text()) {
            if let Some(i) = t
                .parent_element()
                .and_then(|p| index_of.get(p.id().get_usize()).copied())
                .filter(|&i| i != u32::MAX)
            {
                text_in[i as usize] += t.text().map_or(0, str::len) as u64;
            }
        }
        for i in (0..els.len()).rev() {
            if let Some(p) = els[i].parent {
                text_in[p] = text_in[p].saturating_add(text_in[i]);
            }
        }
        let copied = trefs.iter().fold(0u64, |acc, &i| {
            let n = els[i].node;
            let hrefs = [n.attribute((XLINK_NS, "href")), n.attribute("href")];
            hrefs
                .into_iter()
                .flatten()
                .flat_map(href_ids)
                .flat_map(|id| ids.get(id).into_iter().flatten())
                .fold(acc, |acc, &t| acc.saturating_add(text_in[t]))
        });
        if copied > MAX_TREF_TEXT_BYTES {
            return Err(TREF_TOO_MUCH);
        }
    }

    // ---- the parser's tree: every element, plus a copy of each `<use>` target ----
    // Bounds the `<use>` expansion and the per-copy CSS work (style attributes are
    // parsed, and every rule matched, once per copy) before simplecss runs.
    let sheet_cost = sheets
        .iter()
        .fold(0u64, |acc, t| acc.saturating_add(parse_cost(t)));
    if sheet_cost > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }
    let mut parsed = Graph::with_capacity(els.len());
    for (i, el) in els.iter().enumerate() {
        parsed.begin_node()?;
        if el.role != Role::Inert {
            parsed.children(i, &els, |c| c.role != Role::Inert);
            for id in el.use_hrefs.into_iter().flatten().flat_map(href_ids) {
                parsed.add_targets(&ids, id, 1);
            }
        }
    }
    parsed.finish();
    parsed.check_cycles()?;
    let copies = parsed.walk(|e| els[e].visit_cost);
    // usvg parses the `style` attribute of every copy (`style_work`); we tokenise it on
    // every original that isn't inert, including the ones inside inert elements, which
    // `style_work` leaves out unless a `<use>` reaches them.
    let style_work = parsed.walk(|e| els[e].style_cost);
    let original_style = els
        .iter()
        .filter(|e| e.role != Role::Inert)
        .fold(0u64, |acc, e| acc.saturating_add(e.style_cost));
    if style_work.size.max(original_style) > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }
    let max_expanded = u64::from(limits.max_svg_nodes).saturating_add(limits.max_svg_expansion);
    if copies.size > max_expanded {
        return Err(EXPANDS);
    }

    // ---- stylesheet rules: what matching them costs usvg ----
    let mut sheet = simplecss::StyleSheet::new();
    for text in &sheets {
        sheet.parse_more(text);
    }
    let mut per_element = 0u64;
    let mut ref_rules = Vec::new();
    for rule in &sheet.rules {
        per_element = per_element.saturating_add(selector_cost(&rule.selector, max_depth)?);
        if rule
            .declarations
            .iter()
            .any(|d| d.value.contains("url(") || is_inherit(d.value))
        {
            ref_rules.push(rule);
        }
    }
    // usvg tries every rule on every copy (`copies`). We try the rules that can add a
    // reference on every original that isn't inert, including the ones inside inert
    // elements, which `copies` leaves out unless a `<use>` reaches them.
    let originals = els.iter().filter(|e| e.role != Role::Inert).count() as u64;
    if per_element.saturating_mul(copies.size.max(originals)) > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }

    // ---- own references: attributes, `style` attributes, stylesheet rules ----
    // Every attribute counts, whatever its namespace: usvg reads presentation attributes
    // in the XML namespace too. A value of `inherit` takes the parent's value (for
    // `clip-path`, `mask` and `filter` too), so it takes the parent's references.
    let mut n_refs = 0usize;
    for i in 0..els.len() {
        let el = &els[i];
        if el.role == Role::Inert {
            continue;
        }
        let name = el.node.tag_name().name();
        let mut refs = Vec::new();
        let mut inherits = false;
        for a in el.node.attributes() {
            let value = a.value();
            if a.name() == "href" {
                if HREF_FOLLOWED.contains(&name) {
                    refs.extend(href_ids(value).map(|id| (RefKind::Other, id)));
                }
            } else if a.name() == "style" {
                for decl in simplecss::DeclarationTokenizer::from(value) {
                    let kind = RefKind::of_property(decl.name);
                    refs.extend(url_ids(decl.value).map(|id| (kind, id)));
                    inherits |= is_inherit(decl.value);
                }
            } else {
                let kind = RefKind::of_property(a.name());
                refs.extend(url_ids(value).map(|id| (kind, id)));
                inherits |= is_inherit(value);
            }
        }
        // usvg applies the stylesheet to every element it parses. All kinds are kept,
        // also on gradients, stops and primitives: their descendants inherit paint and
        // markers from them (the converter graph follows only what each role follows).
        for rule in &ref_rules {
            if rule.selector.matches(&XmlNode(el.node)) {
                for decl in &rule.declarations {
                    let kind = RefKind::of_property(decl.name);
                    refs.extend(url_ids(decl.value).map(|id| (kind, id)));
                    inherits |= is_inherit(decl.value);
                }
            }
        }
        if inherits && let Some(p) = el.parent {
            refs.extend(els[p].refs.iter().copied());
        }
        refs.sort_unstable();
        refs.dedup();
        refs.retain(|(_, id)| ids.contains_key(id));
        n_refs += refs.len();
        if n_refs > MAX_REFS {
            return Err(TOO_MANY_REFS);
        }
        els[i].refs = refs;
    }

    // ---- inherited references: nearest ancestor with paint or marker references ----
    for i in 0..els.len() {
        if let Some(p) = els[i].parent {
            let parent_has = els[p].refs.iter().any(|(k, _)| k.inherited());
            els[i].inherit_from = if parent_has {
                Some(p)
            } else {
                els[p].inherit_from
            };
        }
    }

    // ---- the converter: rendered children, own and inherited references ----
    // A `<use>` copy inherits paint and markers from the `<use>` and its ancestors, and
    // usvg converts them once per shape of the copy (an objectBoundingBox pattern's
    // content is cloned for every path, a marker drawn at every vertex), so those edges
    // weigh the shapes and the vertices of the expanded target.
    let shapes_in = parsed.sizes(|e| u64::from(els[e].shape));
    let vertices_in = parsed.sizes(|e| els[e].segments);
    let mut converted = Graph::with_capacity(els.len());
    for (i, el) in els.iter().enumerate() {
        converted.begin_node()?;
        if el.role == Role::Inert {
            continue;
        }
        if el.role == Role::HrefOnly {
            // Not drawn: neither children nor paint. Descendants are reached by id.
            converted.refs(&ids, &el.refs, (1, 0), |k| !k.inherited());
        } else {
            // Definitions inside are converted only when referenced.
            converted.children(i, &els, |c| !c.definition && c.role != Role::Inert);
            let uses = el.use_hrefs.into_iter().flatten().flat_map(href_ids);
            let copies = if el.use_hrefs.iter().any(Option::is_some) {
                uses.flat_map(|id| ids.get(id).into_iter().flatten()).fold(
                    (0u64, 0u64),
                    |(s, v), &t| {
                        (
                            s.saturating_add(shapes_in[t]),
                            v.saturating_add(vertices_in[t]),
                        )
                    },
                )
            } else {
                (1, el.segments)
            };
            converted.refs(&ids, &el.refs, copies, |_| true);
            // From every ancestor, whatever its role (usvg's `find_attribute`).
            let mut a = el.inherit_from;
            while let Some(anc) = a {
                converted.refs(&ids, &els[anc].refs, copies, RefKind::inherited);
                if converted.edges.len() > MAX_EDGES {
                    return Err(TOO_MANY_REFS);
                }
                a = els[anc].inherit_from;
            }
        }
    }
    converted.finish();
    converted.check_cycles()?;
    let shapes = converted.walk(|_| 1);
    if shapes.chain.max(copies.chain) > limits.max_svg_nesting {
        return Err(ProcessError::TooComplex(
            "references nest too deeply (patterns, masks, clip paths, markers or <use>)",
        ));
    }
    if shapes.size > max_expanded {
        return Err(EXPANDS);
    }
    // The path segments usvg makes, every copy and every arc's cubics included: memory
    // the painter's own budget would only see after usvg has allocated it.
    let segments = converted.walk(|e| els[e].segments);
    if segments.size > limits.max_svg_expanded_segments {
        return Err(EXPANDS);
    }
    Ok(facts)
}

/// A graph over the elements (node 0 is the root), in compressed adjacency form. Edge
/// weight: how many copies of the target's content following the edge makes (one per
/// vertex for markers).
struct Graph {
    start: Vec<usize>,
    edges: Vec<(usize, u64)>,
}

/// The longest chain from the root, and the weighted size of everything it expands to.
struct Reach {
    chain: usize,
    size: u64,
}

impl Graph {
    fn with_capacity(nodes: usize) -> Graph {
        Graph {
            start: Vec::with_capacity(nodes + 1),
            edges: Vec::new(),
        }
    }

    /// Start the edges of the next node, in element order.
    fn begin_node(&mut self) -> Result<(), ProcessError> {
        if self.edges.len() > MAX_EDGES {
            return Err(TOO_MANY_REFS);
        }
        self.start.push(self.edges.len());
        Ok(())
    }

    /// After the last node.
    fn finish(&mut self) {
        self.start.push(self.edges.len());
    }

    /// Edges to element `i`'s children that `keep` accepts.
    fn children(&mut self, i: usize, els: &[El], keep: impl Fn(&El) -> bool) {
        let mut c = i + 1;
        while c < els[i].end {
            if keep(&els[c]) {
                self.edges.push((c, 1));
            }
            c = els[c].end;
        }
    }

    fn add_targets(&mut self, ids: &HashMap<&str, Vec<usize>>, id: &str, copies: u64) {
        for &t in ids.get(id).map_or(&[][..], Vec::as_slice) {
            self.edges.push((t, copies));
        }
    }

    /// Edges for the references whose kind `follow` accepts. `copies`: how many times
    /// the element uses a paint server, and how many marker vertices it has.
    fn refs(
        &mut self,
        ids: &HashMap<&str, Vec<usize>>,
        refs: &[(RefKind, &str)],
        (paints, vertices): (u64, u64),
        follow: impl Fn(RefKind) -> bool,
    ) {
        for &(kind, id) in refs {
            if !follow(kind) {
                continue;
            }
            let copies = match kind {
                RefKind::Marker => vertices,
                RefKind::Paint => paints,
                RefKind::Other => 1,
            };
            if copies > 0 {
                self.add_targets(ids, id, copies);
            }
        }
    }

    fn nodes(&self) -> usize {
        self.start.len().saturating_sub(1)
    }

    fn out(&self, node: usize) -> &[(usize, u64)] {
        &self.edges[self.start[node]..self.start[node + 1]]
    }

    /// Reject any cycle, starting from every node. Iterative.
    fn check_cycles(&self) -> Result<(), ProcessError> {
        const NEW: u8 = 0;
        const OPEN: u8 = 1;
        const DONE: u8 = 2;
        let n = self.nodes();
        let mut state = vec![NEW; n];
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if state[root] != NEW {
                continue;
            }
            state[root] = OPEN;
            stack.push((root, 0));
            while let Some(top) = stack.last_mut() {
                let (node, k) = *top;
                if let Some(&(t, _)) = self.out(node).get(k) {
                    top.1 += 1;
                    match state[t] {
                        NEW => {
                            state[t] = OPEN;
                            stack.push((t, 0));
                        }
                        OPEN => {
                            return Err(ProcessError::TooComplex(
                                "patterns, masks, clip paths, markers, filters or <use> refer \
                                 to each other in a loop",
                            ));
                        }
                        _ => {}
                    }
                } else {
                    state[node] = DONE;
                    stack.pop();
                }
            }
        }
        Ok(())
    }

    /// Longest chain and weighted expanded size from the root (node 0). The graph must
    /// be acyclic (`check_cycles`). Iterative.
    fn walk(&self, weight: impl Fn(usize) -> u64) -> Reach {
        if self.nodes() == 0 {
            return Reach { chain: 0, size: 0 };
        }
        let (chain, size) = self.expand(std::iter::once(0), weight);
        Reach {
            chain: chain[0],
            size: size[0],
        }
    }

    /// The weighted expanded size from every node. The graph must be acyclic.
    fn sizes(&self, weight: impl Fn(usize) -> u64) -> Vec<u64> {
        self.expand(0..self.nodes(), weight).1
    }

    /// Longest chain and weighted expanded size from each of `roots` (and every node
    /// they reach; the others are 0). Iterative, post-order.
    fn expand(
        &self,
        roots: impl Iterator<Item = usize>,
        weight: impl Fn(usize) -> u64,
    ) -> (Vec<usize>, Vec<u64>) {
        let n = self.nodes();
        let mut done = vec![false; n];
        let mut chain = vec![0usize; n];
        let mut size = vec![0u64; n];
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for root in roots {
            if done[root] {
                continue;
            }
            stack.push((root, 0));
            while let Some(top) = stack.last_mut() {
                let (node, k) = *top;
                if let Some(&(t, _)) = self.out(node).get(k) {
                    top.1 += 1;
                    if !done[t] {
                        stack.push((t, 0));
                    }
                } else {
                    let out = self.out(node);
                    chain[node] = 1 + out.iter().map(|&(t, _)| chain[t]).max().unwrap_or(0);
                    size[node] = out.iter().fold(weight(node), |acc, &(t, copies)| {
                        acc.saturating_add(size[t].saturating_mul(copies))
                    });
                    done[node] = true;
                    stack.pop();
                }
            }
        }
        (chain, size)
    }
}

/// Upper bound on the work of matching `selector` against one element: its length (each
/// simple selector is checked) times `max_depth + 1` per descendant combinator (each
/// backtracks through every ancestor).
fn selector_cost(selector: &simplecss::Selector, max_depth: usize) -> Result<u64, ProcessError> {
    let text = selector.to_string();
    let (mut descendant, mut components) = (0u32, 1usize);
    let parsed = SelectorTokenizer::from(text.as_str()).try_for_each(|t| {
        match t? {
            SelectorToken::DescendantCombinator => {
                descendant += 1;
                components += 1;
            }
            SelectorToken::ChildCombinator | SelectorToken::AdjacentCombinator => components += 1,
            _ => {}
        }
        Ok::<(), simplecss::Error>(())
    });
    if parsed.is_err() {
        // Our display of it didn't re-tokenise; assume every space is a combinator.
        let spaces = text.matches(' ').count();
        descendant = u32::try_from(spaces).unwrap_or(u32::MAX);
        components = spaces + 1;
    }
    if components > MAX_SELECTOR_COMPONENTS {
        return Err(CSS_TOO_COMPLEX);
    }
    let per_level = max_depth as u64 + 1;
    Ok((text.len() as u64 + 1).saturating_mul(per_level.saturating_pow(descendant)))
}

/// A property value of `inherit`.
fn is_inherit(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("inherit")
}

/// XML and svgtypes whitespace.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// The id an `href` names, as svgtypes reads it (`IRI`: spaces, `#`, then everything up
/// to a space), and the same trimmed. Ids may contain tabs or newlines (written as
/// character references), so both are kept: an extra candidate only adds an edge.
fn href_ids(value: &str) -> impl Iterator<Item = &str> {
    let exact = value
        .trim_start_matches(is_space)
        .strip_prefix('#')
        .map(|rest| rest.split(' ').next().unwrap_or(rest));
    candidates(exact)
}

/// Ids in `url(#id)` references in a property value, as svgtypes reads them
/// (`FuncIRI`: quoted, up to the quote and trimmed at the end; unquoted, up to a space or
/// `)`), and the same trimmed.
fn url_ids(value: &str) -> impl Iterator<Item = &str> {
    value.split("url(").skip(1).flat_map(|rest| {
        let rest = rest.trim_start_matches(is_space);
        let quote = rest.chars().next().filter(|c| matches!(c, '\'' | '"'));
        let rest = match quote {
            Some(q) => rest[q.len_utf8()..].trim_start_matches(is_space),
            None => rest,
        };
        let exact = rest.strip_prefix('#').map(|link| match quote {
            Some(q) => link.split(q).next().unwrap_or(link).trim_end(),
            None => link.split([' ', ')']).next().unwrap_or(link),
        });
        candidates(exact)
    })
}

fn candidates(exact: Option<&str>) -> impl Iterator<Item = &str> {
    let trimmed = exact.map(str::trim).filter(|t| Some(*t) != exact);
    exact.into_iter().chain(trimmed).filter(|id| !id.is_empty())
}

/// The element view simplecss matches against: the same as usvg's (`svgtree/parse.rs`),
/// so the same rules match.
struct XmlNode<'a, 'input>(Node<'a, 'input>);

impl simplecss::Element for XmlNode<'_, '_> {
    fn parent_element(&self) -> Option<Self> {
        self.0.parent_element().map(XmlNode)
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.0.prev_sibling_element().map(XmlNode)
    }

    fn has_local_name(&self, local_name: &str) -> bool {
        self.0.tag_name().name() == local_name
    }

    fn attribute_matches(&self, local_name: &str, operator: simplecss::AttributeOperator) -> bool {
        match self.0.attribute(local_name) {
            Some(value) => operator.matches(value),
            None => false,
        }
    }

    fn pseudo_class_matches(&self, class: simplecss::PseudoClass) -> bool {
        match class {
            simplecss::PseudoClass::FirstChild => self.prev_sibling_element().is_none(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_scan_counts_open_tags() {
        assert_eq!(depth_upper_bound(b"<svg><g><g/></g></svg>"), Some(2));
        assert_eq!(
            depth_upper_bound(b"<svg><!-- <g><g><g> --><g></g></svg>"),
            Some(2)
        );
        assert_eq!(
            depth_upper_bound(b"<!DOCTYPE svg [<!ELEMENT g ANY>]><svg><g a=\"/>\"></g></svg>"),
            Some(2)
        );
        assert_eq!(depth_upper_bound(b"<svg><![CDATA[<g><g>]]></svg>"), Some(1));
    }

    #[test]
    fn depth_scan_reads_the_doctype_as_roxmltree_does() {
        let nested = "<svg><g><g></g></g></svg>";
        let depth = |doctype: &str| depth_upper_bound(format!("{doctype}{nested}").as_bytes());
        // Each of these is a DOCTYPE that roxmltree parses before the three elements.
        for doctype in [
            "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://x/svg11.dtd\">",
            "<!DOCTYPE svg PUBLIC \"a>b\" 'c]>'>",
            // A quoted `[` (the review's bypass: counting brackets swallowed the file).
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"[\">]>",
            // roxmltree ends ATTLIST, ELEMENT and NOTATION at the first `>`, quoted or not.
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \">]>",
            "<!DOCTYPE svg [<!ELEMENT svg '>]>",
            // Entity values, comments and PIs are skipped whole.
            "<!DOCTYPE svg [<!ENTITY a \">]>\"> <!ENTITY b SYSTEM 'x>]'>]>",
            "<!DOCTYPE svg [ <!-- ]> --> <?pi ]> ?> ]\n>",
        ] {
            assert_eq!(depth(doctype), Some(3), "{doctype}");
        }
        // Anything roxmltree doesn't parse can't be bounded.
        for bad in [
            "<!DOCTYPE svg [<!ENTITY a \"x\">",
            "<!DOCTYPE svg [<!FOO>]>",
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"x\"]>",
            "<!DOCTYPE svg [<!-- ]>",
            "<!DOCTYPE svg \"",
            "<!doctype svg>",
            "<!FOO>",
        ] {
            assert_eq!(depth(bad), None, "{bad}");
        }
        // An unterminated comment, CDATA section or tag is the end of the input for
        // roxmltree too.
        assert_eq!(depth_upper_bound(b"<svg><g><!-- <g>"), Some(2));
    }

    #[test]
    fn reference_ids_are_read_as_svgtypes_reads_them() {
        let ids: Vec<&str> = url_ids("url(#a) url( \"#b\" ) url('#c'),url(x.svg#d) none").collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(url_ids("url(#)").count(), 0);
        // A tab (a character reference in the file) is part of the id for svgtypes.
        assert_eq!(url_ids("url(#a\t)").collect::<Vec<_>>(), vec!["a\t", "a"]);
        assert_eq!(url_ids("url('#a\t ')").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids(" #a").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids("#a\t").collect::<Vec<_>>(), vec!["a\t", "a"]);
        assert_eq!(href_ids("#a b").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids("x.svg#a").count(), 0);
    }

    fn parse(svg: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(svg).expect("test XML")
    }

    fn run(svg: &str) -> Result<Facts, ProcessError> {
        scan(&parse(svg), &Limits::default())
    }

    #[test]
    fn three_step_pattern_cycle_is_rejected() {
        let pat = |id: &str, next: &str| {
            format!(
                "<pattern id=\"{id}\" width=\"9\" height=\"9\"><rect width=\"5\" height=\"5\" \
                 fill=\"url(#{next})\"/></pattern>"
            )
        };
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs>{}{}{}</defs>\
             <rect width=\"9\" height=\"9\" fill=\"url(#A)\"/></svg>",
            pat("A", "B"),
            pat("B", "C"),
            pat("C", "A")
        );
        assert!(matches!(run(&svg), Err(ProcessError::TooComplex(m)) if m.contains("loop")));
        // Without the back edge it is fine.
        let ok = svg.replace("url(#A)\"/></pattern>", "black\"/></pattern>");
        assert_eq!(run(&ok), Ok(Facts::default()));
    }

    #[test]
    fn inherited_and_stylesheet_references_count() {
        // A pattern whose content inherits the fill that points at it.
        let inherited = "<svg xmlns=\"http://www.w3.org/2000/svg\"><g fill=\"url(#A)\">\
             <pattern id=\"A\" width=\"9\" height=\"9\"><rect width=\"5\" height=\"5\"/></pattern>\
             <rect width=\"9\" height=\"9\"/></g></svg>";
        assert!(matches!(run(inherited), Err(ProcessError::TooComplex(_))));
        // The same loop made with CSS classes.
        let css = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.a{fill:url(#B)}\
             .b{fill:url(#A)}</style><pattern id=\"A\" width=\"9\" height=\"9\">\
             <rect class=\"a\" width=\"5\" height=\"5\"/></pattern><pattern id=\"B\" width=\"9\" \
             height=\"9\"><rect class=\"b\" width=\"5\" height=\"5\"/></pattern>\
             <rect width=\"9\" height=\"9\" fill=\"url(#A)\"/></svg>";
        assert!(matches!(run(css), Err(ProcessError::TooComplex(_))));
        // Gradients referenced from a class on their own group are not a loop: stops
        // aren't drawn.
        let grad = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.g{fill:url(#G)}</style>\
             <g class=\"g\"><linearGradient id=\"G\"><stop offset=\"0\"/></linearGradient>\
             <rect width=\"9\" height=\"9\"/></g></svg>";
        assert_eq!(run(grad), Ok(Facts::default()));
    }

    fn is_loop(r: &Result<Facts, ProcessError>) -> bool {
        matches!(r, Err(ProcessError::TooComplex(m)) if m.contains("loop"))
    }

    const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:o=\"urn:other\">";

    fn pat(id: &str, rect_attrs: &str) -> String {
        format!(
            "<pattern id=\"{id}\" width=\"9\" height=\"9\" patternUnits=\"userSpaceOnUse\">\
             <rect width=\"5\" height=\"5\"{rect_attrs}/></pattern>"
        )
    }

    #[test]
    fn definitions_inside_gradients_stops_and_primitives_are_followed() {
        // usvg parses every child of a gradient, stop or filter primitive and resolves an
        // id anywhere in its tree, so a definition nested in one is converted when
        // something references it, and its content inherits paint and markers from them.
        // Each of these aborted usvg 0.48.1 on a 64 MiB stack.
        let mask = |id: &str, next: &str| {
            format!(
                "<mask id=\"{id}\"><rect width=\"9\" height=\"9\" fill=\"white\" \
                 mask=\"url(#{next})\"/></mask>"
            )
        };
        let clip = |id: &str, next: &str| {
            format!(
                "<clipPath id=\"{id}\"><rect width=\"9\" height=\"9\" \
                 clip-path=\"url(#{next})\"/></clipPath>"
            )
        };
        let wrappers = [
            ("<linearGradient id=\"W\"{a}>", "</linearGradient>"),
            ("<radialGradient id=\"W\"{a}>", "</radialGradient>"),
            (
                "<linearGradient id=\"W\"><stop offset=\"0\"{a}>",
                "</stop></linearGradient>",
            ),
            ("<filter id=\"W\"><feFlood{a}>", "</feFlood></filter>"),
            (
                "<filter id=\"W\"><feMerge><feMergeNode{a}>",
                "</feMergeNode></feMerge></filter>",
            ),
        ];
        for (open, close) in wrappers {
            let wrap =
                |attrs: &str, inner: &str| format!("{}{inner}{close}", open.replace("{a}", attrs));
            let cases = [
                // The pattern's content inherits the wrapper's fill or stroke.
                (
                    "inherited fill",
                    format!(
                        "{SVG}{}<rect width=\"9\" height=\"9\" fill=\"url(#P)\"/></svg>",
                        wrap(" fill=\"url(#P)\"", &pat("P", ""))
                    ),
                ),
                (
                    "inherited stroke",
                    format!(
                        "{SVG}{}<rect width=\"9\" height=\"9\" stroke=\"url(#P)\"/></svg>",
                        wrap(" style=\"stroke:url(#P)\"", &pat("P", ""))
                    ),
                ),
                // A mask whose content inherits a fill from the wrapper, naming a pattern
                // that masks with it.
                (
                    "inherited fill into a mask",
                    format!(
                        "{SVG}{}{}<rect width=\"9\" height=\"9\" fill=\"url(#P)\"/></svg>",
                        wrap(
                            " fill=\"url(#P)\"",
                            "<mask id=\"M\"><rect width=\"9\" height=\"9\"/></mask>"
                        ),
                        pat("P", " mask=\"url(#M)\"")
                    ),
                ),
                // Three-step loops with one link inside the wrapper.
                (
                    "pattern loop",
                    format!(
                        "{SVG}{}{}{}<rect width=\"9\" height=\"9\" fill=\"url(#A)\"/></svg>",
                        wrap("", &pat("A", " fill=\"url(#B)\"")),
                        pat("B", " fill=\"url(#C)\""),
                        pat("C", " fill=\"url(#A)\"")
                    ),
                ),
                (
                    "mask loop",
                    format!(
                        "{SVG}{}{}{}<rect width=\"9\" height=\"9\" mask=\"url(#A)\"/></svg>",
                        wrap("", &mask("A", "B")),
                        mask("B", "C"),
                        mask("C", "A")
                    ),
                ),
                (
                    "clip loop",
                    format!(
                        "{SVG}{}{}{}<rect width=\"9\" height=\"9\" clip-path=\"url(#A)\"/></svg>",
                        wrap("", &clip("A", "B")),
                        clip("B", "C"),
                        clip("C", "A")
                    ),
                ),
                // The same through a stylesheet rule that matches the wrapper.
                (
                    "inherited fill from a stylesheet",
                    format!(
                        "{SVG}<style>.w{{fill:url(#P)}}</style>{}<rect width=\"9\" height=\"9\" \
                         fill=\"url(#P)\"/></svg>",
                        wrap(" class=\"w\"", &pat("P", ""))
                    ),
                ),
            ];
            for (name, svg) in cases {
                assert!(is_loop(&run(&svg)), "{name} in {open}: {:?}", run(&svg));
            }
            // A pattern kept in a gradient and used normally is fine.
            let ok = format!(
                "{SVG}{}<rect width=\"9\" height=\"9\" fill=\"url(#P)\"/></svg>",
                wrap("", &pat("P", " fill=\"url(#W)\""))
            );
            assert!(run(&ok).is_ok(), "{open}: {:?}", run(&ok));
        }
    }

    #[test]
    fn elements_inside_unparsed_elements_count_when_a_use_copies_them() {
        // usvg never parses a `<foreignObject>`, `<style>`, metadata or a foreign element,
        // but a `<use>` copies any SVG element in the document, wherever it is. Each of
        // these aborted usvg 0.48.1 on a 64 MiB stack.
        for (open, close) in [
            (
                "<foreignObject width=\"1\" height=\"1\">",
                "</foreignObject>",
            ),
            ("<o:thing>", "</o:thing>"),
            ("<metadata>", "</metadata>"),
            ("<title>", "</title>"),
            ("<style>", "</style>"),
        ] {
            let svg = format!(
                "{SVG}{open}<g id=\"x\"><rect width=\"5\" height=\"5\" fill=\"url(#B)\"/></g>\
                 {close}<pattern id=\"A\" width=\"9\" height=\"9\" patternUnits=\"userSpaceOnUse\">\
                 <use href=\"#x\"/></pattern>{}{}<rect width=\"9\" height=\"9\" \
                 fill=\"url(#A)\"/></svg>",
                pat("B", " fill=\"url(#C)\""),
                pat("C", " fill=\"url(#A)\"")
            );
            assert!(is_loop(&run(&svg)), "{open}: {:?}", run(&svg));
            // Copies made there count towards the expansion too.
            let bomb = format!(
                "{SVG}{open}<g id=\"x\">{}</g>{close}{}</svg>",
                "<rect width=\"1\" height=\"1\"/>".repeat(200),
                "<use href=\"#x\"/>".repeat(400)
            );
            assert_eq!(run(&bomb), Err(EXPANDS), "{open}");
        }
    }

    #[test]
    fn nesting_multiplies_along_reference_chains() {
        // 40 patterns, each with 40 nested groups: under every per-element cap, but
        // about 1700 levels deep once followed.
        let limits = Limits {
            max_svg_nesting: 1000,
            ..Limits::default()
        };
        let mut s = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"><defs>");
        for i in 0..40 {
            let fill = if i < 39 {
                format!("url(#p{})", i + 1)
            } else {
                "black".into()
            };
            s += &format!(
                "<pattern id=\"p{i}\" width=\"9\" height=\"9\">{}<rect width=\"5\" height=\"5\" \
                 fill=\"{fill}\"/>{}</pattern>",
                "<g>".repeat(40),
                "</g>".repeat(40)
            );
        }
        s += "</defs><rect width=\"9\" height=\"9\" fill=\"url(#p0)\"/></svg>";
        let doc = parse(&s);
        assert!(matches!(
            scan(&doc, &limits),
            Err(ProcessError::TooComplex(m)) if m.contains("nest")
        ));
        assert!(scan(&doc, &Limits::default()).is_ok());
    }

    #[test]
    fn markers_expand_per_vertex() {
        let mut s = String::from(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><marker id=\"m\" markerWidth=\"9\" \
             markerHeight=\"9\">",
        );
        s += &"<rect width=\"1\" height=\"1\"/>".repeat(1000);
        s += "</marker><path marker-mid=\"url(#m)\" d=\"M0 0";
        s += &" 1 1".repeat(1000);
        s += "\"/></svg>";
        assert_eq!(run(&s), Err(EXPANDS));
    }

    #[test]
    fn markers_on_rects_circles_and_ellipses_expand_per_vertex() {
        // usvg draws markers on every shape (`marker::is_valid` doesn't look at the tag),
        // and turns a rect, circle or ellipse into a path with up to 10 vertices. Three
        // markers, each with 10 such shapes carrying the next one, are 10^6 copies: the
        // 2.4 KB four-level file reached 2.9 GB inside usvg and aborted.
        let shapes = [
            "<circle cx=\"5\" cy=\"5\" r=\"4\"{m}/>",
            "<ellipse cx=\"5\" cy=\"5\" rx=\"4\" ry=\"2\"{m}/>",
            "<rect width=\"4\" height=\"4\"{m}/>",
            "<rect width=\"4\" height=\"4\" rx=\"1\"{m}/>",
        ];
        for shape in shapes {
            let with = |next: Option<usize>| {
                let m = next.map_or(String::new(), |n| format!(" style=\"marker:url(#M{n})\""));
                shape.replace("{m}", &m)
            };
            let chain = |levels: usize| {
                let mut s = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"><defs>");
                for i in 0..levels {
                    let next = (i + 1 < levels).then_some(i + 1);
                    s += &format!("<marker id=\"M{i}\">{}</marker>", with(next).repeat(10));
                }
                s + "</defs>" + &with(Some(0)) + "</svg>"
            };
            assert_eq!(run(&chain(3)), Err(EXPANDS), "{shape}");
            // Two levels (about 11,000 copies) are within the limits.
            assert_eq!(run(&chain(2)), Ok(Facts::default()), "{shape}");
        }
    }

    #[test]
    fn paint_inherited_by_use_copies_counts_every_shape() {
        // `<use>` copies inherit fill and markers from the `<use>` and its ancestors, and
        // usvg converts an objectBoundingBox pattern's content once per path that uses
        // it. Each pattern holds a `<use>` of N shapes inside a group filled with the
        // next pattern: N^levels copies (15^4 took 324 MB; 6^8, a 1 KB file, aborted).
        type Link = fn(&str) -> String;
        let chain = |n: usize, levels: usize, link: Link| {
            let mut s = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs><g id=\"grp\">{}</g>",
                "<rect width=\"1\" height=\"1\"/>".repeat(n)
            );
            for i in 0..levels {
                let next = if i + 1 < levels {
                    link(&format!("P{}", i + 1))
                } else {
                    "<use href=\"#grp\"/>".into()
                };
                s += &format!("<pattern id=\"P{i}\" width=\"1\" height=\"1\">{next}</pattern>");
            }
            s + "</defs><rect width=\"9\" height=\"9\" fill=\"url(#P0)\"/></svg>"
        };
        let links: [(&str, Link); 3] = [
            ("fill on a group around the <use>", |p| {
                format!("<g fill=\"url(#{p})\"><use href=\"#grp\"/></g>")
            }),
            ("fill on the <use>", |p| {
                format!("<use href=\"#grp\" fill=\"url(#{p})\"/>")
            }),
            ("fill from a stylesheet", |p| {
                format!("<style>.{p}{{fill:url(#{p})}}</style><use class=\"{p}\" href=\"#grp\"/>")
            }),
        ];
        for (name, link) in links {
            assert_eq!(run(&chain(15, 4, link)), Err(EXPANDS), "{name}");
            assert_eq!(run(&chain(6, 8, link)), Err(EXPANDS), "{name}");
            // 10^3 copies are fine.
            assert!(run(&chain(10, 3, link)).is_ok(), "{name}");
        }
        // Markers inherited by the copies: one per vertex of every copied shape.
        let mut s =
            String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"><defs><marker id=\"m\">");
        s += &"<rect width=\"1\" height=\"1\"/>".repeat(100);
        s += "</marker><g id=\"grp\">";
        s += &"<path d=\"M0 0L1 1L2 0L3 1L4 0\"/>".repeat(50);
        s += "</g></defs><use href=\"#grp\" marker-mid=\"url(#m)\"/>";
        s += &"<use href=\"#grp\" style=\"marker:url(#m)\"/>".repeat(9);
        s += "</svg>";
        assert_eq!(run(&s), Err(EXPANDS));
    }

    #[test]
    fn huge_arcs_are_too_big_before_usvg_splits_them() {
        // kurbo splits an arc into more cubics the bigger its radius: a 100-byte path
        // with a radius of 1e50 is billions of cubics (it aborted), and 1e30 took 11 s.
        let svg = |body: &str| {
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">{body}</svg>"
            )
        };
        for big in [
            "<path d=\"M0 0A1e50 1e50 0 1 1 1e50 0Z\"/>",
            "<path d=\"M0 0A1e30 1e30 0 1 1 1 0Z\"/>",
            // Relative arcs, and a radius scaled up to reach the end point (1e30 / 1).
            "<path d=\"m0 0a1 1 0 0 1 2 0a1e9 1e9 0 1 1 1e30 0\"/>",
            "<path d=\"M0 0A1e9 1e-4 0 0 1 1e20 0\"/>",
            "<circle r=\"1e30\"/>",
            "<circle r=\"3e38\"/>",
            "<circle r=\"1e12in\"/>",
            "<ellipse rx=\"1\" ry=\"1e30\"/>",
            "<rect width=\"1e30\" height=\"1e30\" rx=\"1e29\"/>",
            "<rect width=\"1e30\" height=\"1e30\" ry=\"1e29\"/>",
            // Geometry in another namespace usvg reads.
            "<circle xml:r=\"1e30\"/>",
            // Lengths the prescan can't bound: `em` once a font size is set anywhere, `%`
            // when nested `<svg>`s set their own viewport.
            "<g font-size=\"1e30\"><circle r=\"1em\"/></g>",
            "<g style=\"font:1e30px x\"><circle r=\"1ex\"/></g>",
            "<svg width=\"1e30\" height=\"1e30\"><circle r=\"50%\"/></svg>",
        ] {
            assert_eq!(run(&svg(big)), Err(ARC_TOO_BIG), "{big}");
        }
        // `%` of a huge root: its viewBox, or its size when usvg can't use the viewBox
        // (in f32, 1e30 + 1 - 1e30 is 0).
        for root in [
            "viewBox=\"0 0 1e30 1e30\"",
            "viewBox=\"1e30 0 1 1\" width=\"1e30\"",
            "width=\"1e30\" height=\"1\"",
            "width=\"1e28%\"",
        ] {
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" {root}><circle r=\"50%\"/></svg>"
            );
            assert_eq!(run(&svg), Err(ARC_TOO_BIG), "{root}");
        }
        for fine in [
            "<path d=\"M10 50A40 40 0 1 1 90 50A40 40 0 1 1 10 50Z\"/>",
            "<path d=\"m10 50a40 10 30 1 0 80 0a1e-6 5 0 0 1 9 9\"/>",
            "<circle cx=\"50\" cy=\"50\" r=\"50%\"/>",
            "<circle cx=\"50\" cy=\"50\" r=\"2em\"/>",
            "<circle cx=\"50\" cy=\"50\" r=\"0.5in\"/>",
            // Clamped to half the width.
            "<rect width=\"10\" height=\"10\" rx=\"1e30\"/>",
            "<ellipse rx=\"1e3\" ry=\"5\"/>",
        ] {
            assert_eq!(run(&svg(fine)), Ok(Facts::default()), "{fine}");
        }
    }

    #[test]
    fn segment_bounds_hold_for_what_usvg_makes() {
        // Random paths of absolute and relative arcs (radii from 1e-6 to 5e9, so that
        // radii get scaled up and the end points drift), lines and closes, against the
        // segments svgtypes really makes of them (usvg's own path reader); and shapes
        // against usvg's paths.
        let mut seed = 0x9e37_79b9u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut num = |max_exp: u32| -> f64 {
            let r = rnd();
            let m = f64::from(r % 2000) / 200.0 - 5.0;
            m * 10f64.powi((r / 2000 % (max_exp + 7)) as i32 - 6)
        };
        let mut checked = 0;
        for _ in 0..1000 {
            let mut d = String::from("M0 0");
            for _ in 0..(num(0).abs() as usize + 3) {
                let (rx, ry, rot) = (num(9), num(9), num(2));
                let (x, y) = (num(6), num(6));
                let (large, sweep) = (u8::from(x > 0.0), u8::from(y > 0.0));
                d += &match (num(0) * 10.0) as i64 {
                    ..=-20 => format!("A{rx} {ry} {rot} {large} {sweep} {x} {y}"),
                    -19..=10 => format!("a{rx} {ry} {rot} {large} {sweep} {x} {y}"),
                    11..=30 => format!("l{x} {y}"),
                    _ => "z".into(),
                };
            }
            let Ok(bound) = path_segments(&d) else {
                continue;
            };
            let made = svgtypes::SimplifyingPathParser::from(d.as_str())
                .map_while(Result::ok)
                .count() as u64;
            assert!(made <= bound, "{d}: {made} > {bound}");
            checked += 1;
        }
        assert!(checked > 300, "{checked}");

        let units = Units {
            percent_base: Some(100.0),
            font_size: Some(12.0),
        };
        for r in ["1", "50", "1e4", "1e8", "2e12", "50%", "3em", "2in"] {
            for shape in [
                format!("<circle r=\"{r}\"/>"),
                format!("<ellipse rx=\"{r}\" ry=\"7\"/>"),
                format!("<rect width=\"5e12\" height=\"1e12\" rx=\"{r}\"/>"),
                format!("<rect width=\"5e12\" height=\"1e12\" ry=\"{r}\"/>"),
            ] {
                let svg = format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">{shape}</svg>"
                );
                let doc = parse(&svg);
                let node = doc.root_element().first_element_child().expect("shape");
                let bound = shape_segments(node, node.tag_name().name(), units).expect(&shape);
                let tree = usvg::Tree::from_xmltree(&doc, &usvg::Options::default()).expect(&svg);
                let made = match tree.root().children() {
                    [usvg::Node::Path(p)] => p.data().len() as u64,
                    other => panic!("{shape}: {other:?}"),
                };
                assert!(made <= bound, "{shape}: {made} > {bound}");
            }
        }
    }

    #[test]
    fn segments_from_copies_are_bounded() {
        // A long path copied by `<use>`s: few elements, but usvg holds every copy's
        // segments (300 copies of 4,000 would be 1.2M). Counted as 2 per command: 8,004
        // per copy, so 62 copies are within the 500,000 budget and 63 are not.
        let svg = |copies: usize| {
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs><path id=\"p\" d=\"M0 0{}\"/>\
                 </defs>{}</svg>",
                "L1 1".repeat(4000),
                "<use href=\"#p\"/>".repeat(copies)
            )
        };
        assert_eq!(Limits::default().max_svg_expanded_segments, 500_000);
        assert_eq!(run(&svg(63)), Err(EXPANDS));
        assert_eq!(run(&svg(62)), Ok(Facts::default()));
    }

    #[test]
    fn tref_copies_are_bounded() {
        // usvg copies the target's text into every `<tref>`, finding it by a scan of the
        // whole document.
        let svg = |trefs: usize, text: usize| {
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\"><g id=\"x\"><text>{}</text></g>\
                 <text>{}</text></svg>",
                "A".repeat(text),
                "<tref href=\"#x\"/>".repeat(trefs)
            )
        };
        assert_eq!(run(&svg(100, 20_000)), Err(TREF_TOO_MUCH));
        assert_eq!(run(&svg(5_000, 1)), Err(TREF_TOO_MUCH));
        let facts = Facts {
            text: true,
            ..Facts::default()
        };
        assert_eq!(run(&svg(10, 20_000)), Ok(facts.clone()));
        assert_eq!(run(&svg(500, 1)), Ok(facts));
    }

    #[test]
    fn css_costs_are_bounded_before_simplecss_runs() {
        // Backtracking selectors.
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>x g g g g g g g g g g \
             {{fill:red}}</style>{}<rect width=\"9\" height=\"9\"/>{}</svg>",
            "<g>".repeat(60),
            "</g>".repeat(60)
        );
        assert_eq!(run(&svg), Err(CSS_TOO_COMPLEX));
        // Quadratic declaration parsing, in a sheet and in a style attribute.
        let decls = "fill:red;".repeat(16_000);
        let sheet =
            format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><style>a{{{decls}}}</style></svg>");
        assert_eq!(run(&sheet), Err(CSS_TOO_COMPLEX));
        let attr =
            format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><rect style=\"{decls}\"/></svg>");
        assert_eq!(run(&attr), Err(CSS_TOO_COMPLEX));
        // ... also on elements usvg never parses (we read their references all the same).
        for (open, close) in [
            ("<metadata>", "</metadata>"),
            ("<title>", "</title>"),
            ("<foreignObject>", "</foreignObject>"),
            ("<o:x xmlns:o=\"urn:o\">", "</o:x>"),
        ] {
            let inert = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\">{open}<g style=\"{}\"/>{close}</svg>",
                "a:b;".repeat(3_000)
            );
            assert_eq!(run(&inert), Err(CSS_TOO_COMPLEX), "{open}");
        }
        // A modest style attribute copied by many `<use>`s.
        let copied = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><g id=\"g\">{}</g>{}</svg>",
            format!("<rect style=\"{}\"/>", "fill:red;".repeat(200)).repeat(20),
            "<use href=\"#g\"/>".repeat(400)
        );
        assert_eq!(run(&copied), Err(CSS_TOO_COMPLEX));
        // Simple class rules (what Illustrator exports) are cheap.
        let simple = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.st0{fill:#231F20;}\
             .st1{fill:url(#g)}</style><linearGradient id=\"g\"><stop offset=\"0\"/>\
             </linearGradient><path class=\"st0\" d=\"M0 0H9V9Z\"/><path class=\"st1\" \
             d=\"M0 0H9V9Z\"/></svg>";
        assert_eq!(run(simple), Ok(Facts::default()));
    }
}
