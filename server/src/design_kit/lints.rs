//! Friendly findings about an upload, and the shape rules that produce them.
//!
//! Shape lints are measured on the clean path ([`Metrics`]) and depend on the asset kind,
//! so [`for_kind`] runs once per kind. Input facts (what the pipeline noticed or changed
//! while reading the file) don't depend on the kind and are collected separately.
//!
//! Every warn-level threshold sits outside the range of all 184 official heads and tails
//! (measured with rsvg at the same 200 px); see `docs/design-kit.md` for the derivation.

use super::raster::Metrics;
use super::{AssetKind, Fix};

/// How prominently the studio shows a lint. Serialized as [`Severity::as_str`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Will look wrong on the board. Shown first, with a fix button when there is one.
    Warn,
    /// Worth knowing; the result is still fine.
    Tip,
    /// What the studio did on your behalf. Collapsed under "Details".
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Warn => "warn",
            Severity::Tip => "tip",
            Severity::Info => "info",
        }
    }
}

/// Where a reversed or rotated drawing's full-height (attach) side is. Never the left:
/// that is where it belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Right,
    Top,
    Bottom,
}

/// One finding. `code()` is the stable identifier; `message()` is the user-facing copy.
#[derive(Debug, Clone, PartialEq)]
pub enum Lint {
    // ---- shape (per kind) ----
    /// L1: coverage < 15%.
    NearlyEmpty { fill_pct: f32 },
    /// L2 (heads): coverage > 95%.
    SolidSquare { fill_pct: f32 },
    /// L3: left-edge coverage < 85%.
    NeckGap { left_edge_pct: f32 },
    /// L3b: the drawing doesn't reach the edges (x0 > 2, y0 > 3 or y1 < 97). Offers Fit
    /// unless Fit would change nothing (see [`fit_helps`]).
    Margins { bbox: [f32; 4] },
    /// L4 (heads): the drawing's full-height side is on the right and its left side looks
    /// like a front (mirrored), or, when no side says it is turned or mirrored, its
    /// centre of mass is right of x = 52.
    FacesLeft { centroid_x: f32 },
    /// L4b (heads): the drawing's full-height side is the top or bottom and the side
    /// opposite looks like a front (rotated), or the neck is on the left but the right
    /// side is also more than 60% filled (a flat front, or a quarter turn of a square-ish
    /// head). `right_edge_pct` is the drawing's own right side.
    FacesUpDown { right_edge_pct: f32 },
    /// L4c (tails): the drawing's left side isn't full height (< 85%) but another side is
    /// (≥ 85%), and the side opposite that one looks like a tip (< 60%).
    TailReversed { attach_edge: Edge },
    /// L5: enclosed holes > 55% of the filled-in silhouette.
    OutlineOnly { hole_pct: f32 },
    // ---- input facts (kind-independent) ----
    /// L6: specks and pinholes smaller than 1 unit² were removed.
    SpecksRemoved { count: usize },
    /// L9: colours were merged into one silhouette.
    ColoursFlattened,
    /// Template guides or reference shapes were visible in the export and were ignored.
    GuidesVisible,
    /// The `draw-here` layer held the drawing, and visible shapes on other layers were
    /// left out.
    OutsideDrawHereIgnored,
    /// Soft or semi-transparent pixels were thresholded at 50% opacity.
    SemiTransparent,
    /// The canvas wasn't square, so it was centred in one.
    NonSquare { width: u32, height: u32 },
    /// The image is smaller than `Limits::min_useful_side`.
    LowResolution { width: u32, height: u32 },
    // ---- SVG input facts ----
    /// Strokes were turned into filled outlines.
    StrokesConverted { count: usize },
    /// Gradients were painted in one flat colour, their average (dark ones join the shape,
    /// light or faint ones become holes or are left out); patterns solid.
    Gradient,
    /// Embedded or linked `<image>`s were ignored (never loaded).
    ImageIgnored { count: usize },
    /// Text that wasn't converted to outlines was ignored.
    TextIgnored,
    /// Clip paths were applied (`clipped`) and/or masks were ignored (`masked`: masked
    /// shapes show in full).
    ClipOrMask { clipped: bool, masked: bool },
    /// Filters (blur, shadows, ...) were ignored.
    FiltersIgnored,
    /// Scripts, event handlers, embedded HTML, animations or external links were
    /// dropped. They never reach the output, which is one path.
    ActiveContentRemoved,
    /// Part of the drawing is outside the square; the board cuts it off.
    OutsideCanvas,
}

/// Thresholds (decision 8 of the DEV-1539 plan), all on the 200 px metrics mask.
pub const NEARLY_EMPTY_BELOW_PCT: f32 = 15.0;
pub const SOLID_SQUARE_ABOVE_PCT: f32 = 95.0;
pub const NECK_GAP_BELOW_PCT: f32 = 85.0;
pub const MARGIN_MAX_X0: f32 = 2.0;
pub const MARGIN_MAX_Y0: f32 = 3.0;
pub const MARGIN_MIN_Y1: f32 = 97.0;
pub const FACES_LEFT_ABOVE_X: f32 = 52.0;
pub const FACES_UP_DOWN_ABOVE_PCT: f32 = 60.0;
pub const ATTACH_EDGE_MIN_PCT: f32 = 85.0;
/// A side opposite a full-height side only reads as a front or tip (so the full-height
/// side as a turned or mirrored attach edge) below this: every official head's front is
/// at most 47.5% full, and 95% of the tails' tips at most 49.5%.
pub const FRONT_BELOW_PCT: f32 = 60.0;
pub const OUTLINE_ONLY_ABOVE_PCT: f32 = 55.0;

impl Lint {
    /// Stable snake_case identifier (used by the client and in logs).
    pub fn code(&self) -> &'static str {
        match self {
            Lint::NearlyEmpty { .. } => "nearly_empty",
            Lint::SolidSquare { .. } => "solid_square",
            Lint::NeckGap { .. } => "neck_gap",
            Lint::Margins { .. } => "margins",
            Lint::FacesLeft { .. } => "faces_left",
            Lint::FacesUpDown { .. } => "faces_up_down",
            Lint::TailReversed { .. } => "tail_reversed",
            Lint::OutlineOnly { .. } => "outline_only",
            Lint::SpecksRemoved { .. } => "specks_removed",
            Lint::ColoursFlattened => "colours_flattened",
            Lint::GuidesVisible => "guides_visible",
            Lint::OutsideDrawHereIgnored => "outside_draw_here_ignored",
            Lint::SemiTransparent => "semi_transparent",
            Lint::NonSquare { .. } => "non_square",
            Lint::LowResolution { .. } => "low_resolution",
            Lint::StrokesConverted { .. } => "strokes_converted",
            Lint::Gradient => "gradient",
            Lint::ImageIgnored { .. } => "image_ignored",
            Lint::TextIgnored => "text_ignored",
            Lint::ClipOrMask { .. } => "clip_or_mask",
            Lint::FiltersIgnored => "filters_ignored",
            Lint::ActiveContentRemoved => "active_content_removed",
            Lint::OutsideCanvas => "outside_canvas",
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            Lint::NearlyEmpty { .. }
            | Lint::SolidSquare { .. }
            | Lint::NeckGap { .. }
            | Lint::Margins { .. }
            | Lint::FacesLeft { .. }
            | Lint::FacesUpDown { .. }
            | Lint::TailReversed { .. }
            | Lint::OutlineOnly { .. } => Severity::Warn,
            Lint::NonSquare { .. }
            | Lint::LowResolution { .. }
            | Lint::ImageIgnored { .. }
            | Lint::TextIgnored
            | Lint::OutsideDrawHereIgnored => Severity::Tip,
            Lint::SpecksRemoved { .. }
            | Lint::ColoursFlattened
            | Lint::GuidesVisible
            | Lint::SemiTransparent
            | Lint::StrokesConverted { .. }
            | Lint::Gradient
            | Lint::ClipOrMask { .. }
            | Lint::FiltersIgnored
            | Lint::ActiveContentRemoved
            | Lint::OutsideCanvas => Severity::Info,
        }
    }

    /// The one-tap fix the studio can offer next to this lint.
    pub fn fix(&self) -> Option<Fix> {
        match self {
            Lint::Margins { bbox } if fit_helps(*bbox) => Some(Fix::Fit),
            Lint::FacesLeft { .. }
            | Lint::TailReversed {
                attach_edge: Edge::Right,
            } => Some(Fix::Flip),
            _ => None,
        }
    }

    /// Anchor in the studio guide that explains the rule.
    pub fn guide_anchor(&self) -> &'static str {
        match self {
            Lint::NeckGap { .. } => "#neck",
            Lint::Margins { .. } | Lint::NonSquare { .. } => "#margins",
            Lint::FacesLeft { .. } | Lint::FacesUpDown { .. } | Lint::TailReversed { .. } => {
                "#direction"
            }
            Lint::NearlyEmpty { .. }
            | Lint::OutlineOnly { .. }
            | Lint::StrokesConverted { .. }
            | Lint::TextIgnored
            | Lint::ImageIgnored { .. } => "#fill",
            Lint::SolidSquare { .. } => "#holes",
            Lint::SpecksRemoved { .. } | Lint::LowResolution { .. } => "#small",
            Lint::ColoursFlattened
            | Lint::SemiTransparent
            | Lint::Gradient
            | Lint::ClipOrMask { .. }
            | Lint::FiltersIgnored
            | Lint::ActiveContentRemoved => "#colour",
            Lint::GuidesVisible | Lint::OutsideDrawHereIgnored => "#guides",
            Lint::OutsideCanvas => "#margins",
        }
    }

    /// User-facing copy, in template terms (1000 px canvas) and app terms.
    pub fn message(&self) -> String {
        match self {
            Lint::NearlyEmpty { .. } => {
                "There's very little here. Draw in solid black and make it big.".into()
            }
            Lint::SolidSquare { .. } => "This is nearly a solid square. Cut out an eye or a \
                 mouth so it reads as a head."
                .into(),
            Lint::NeckGap { .. } => "The left edge is where the body joins. Fill it from top to \
                 bottom, or you'll see a notch. In Procreate, drag the colour dot into the shape \
                 to fill it."
                .into(),
            Lint::Margins { bbox } if fit_helps(*bbox) => "Your drawing doesn't reach the edges \
                 of the square, so it'll look small and leave a gap at the neck. Tap Fit to \
                 stretch it, or draw edge to edge."
                .into(),
            Lint::Margins { .. } => "Your drawing doesn't reach the top and bottom of the \
                 square, so it'll look squashed on the board. Draw it taller, from edge to edge."
                .into(),
            Lint::FacesLeft { .. } => {
                "Heads should face right, and the game turns them for you. Tap Flip.".into()
            }
            Lint::FacesUpDown { .. } => {
                "This looks rotated. Heads face right, with the neck on the left.".into()
            }
            Lint::TailReversed {
                attach_edge: Edge::Right,
            } => "Tails join the body on the left and point right. Tap Flip.".into(),
            Lint::TailReversed { .. } => "Tails join the body on the left and point right. \
                 Rotate your drawing so its full-height edge is on the left."
                .into(),
            Lint::OutlineOnly { .. } => "This looks like an outline. Only filled shapes show up, \
                 so fill the inside and erase the details you want as holes."
                .into(),
            Lint::SpecksRemoved { count } => format!(
                "We removed {count} tiny speck{} or pinhole{} (smaller than 10 × 10 px on the \
                 1000 px template). Keep details at least 40 px.",
                plural(*count),
                plural(*count)
            ),
            Lint::ColoursFlattened => "We used one colour. Light or white areas became holes, and \
                 everything else became the snake's colour."
                .into(),
            Lint::GuidesVisible => {
                "We ignored the template guides. Hide them next time for the cleanest result."
                    .into()
            }
            Lint::OutsideDrawHereIgnored => "We only used your \"Draw here\" layer and left out \
                 the shapes on your other layers. Move everything you drew into \"Draw here\", \
                 or hide the layers you don't want."
                .into(),
            Lint::SemiTransparent => "Soft or see-through strokes only count where they're at \
                 least half opaque. Use a solid brush at full opacity."
                .into(),
            Lint::NonSquare { width, height } => format!(
                "Your canvas is {width} × {height} px, so we centred it in a square. Use the \
                 1000 × 1000 px template so it lines up."
            ),
            Lint::LowResolution { width, height } => format!(
                "Your image is only {width} × {height} px, so edges may look soft. Export at \
                 1000 × 1000 px, the template's size."
            ),
            Lint::StrokesConverted { count } => format!(
                "We turned {count} stroke{} into filled shapes. To see exactly what you'll get, \
                 use Outline Stroke (Illustrator) or Stroke to Path (Inkscape) before exporting.",
                plural(*count)
            ),
            Lint::Gradient => "Each gradient became one flat colour, its average: dark ones \
                 joined the shape, and light or faint ones became holes or were left out. \
                 Patterns became solid shapes. Check the preview."
                .into(),
            Lint::ImageIgnored { count } => format!(
                "We ignored {count} embedded image{}. Draw with vector shapes, or upload your \
                 drawing as a PNG instead.",
                plural(*count)
            ),
            Lint::TextIgnored => "Text isn't supported, so we left it out. Convert it to \
                 outlines first (Type → Create Outlines, or Path → Object to Path)."
                .into(),
            Lint::ClipOrMask { clipped, masked } => match (clipped, masked) {
                (true, true) => "We applied your clipping paths. Masks are ignored, so masked \
                     shapes show in full; check the preview."
                    .into(),
                (false, true) => "Masks are ignored, so masked shapes show in full; check the \
                     preview."
                    .into(),
                _ => "We applied your clipping paths; check the preview.".into(),
            },
            Lint::FiltersIgnored => "Filters such as blurs and drop shadows are ignored.".into(),
            Lint::ActiveContentRemoved => "We removed scripts, links and embedded web content. \
                 Only the shapes are kept."
                .into(),
            Lint::OutsideCanvas => {
                "Part of your drawing is outside the square. The game cuts it off.".into()
            }
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The drawing has a margin: it doesn't reach the left edge, the top or the bottom.
fn has_margins(b: [f32; 4]) -> bool {
    b[0] > MARGIN_MAX_X0 || b[1] > MARGIN_MAX_Y0 || b[3] < MARGIN_MIN_Y1
}

/// The bounds Fit would give a drawing with bounds `b` (see `fix.rs`): the larger side
/// scaled to 100, the left side moved to x = 0, centred vertically.
fn fitted_bbox(b: [f32; 4]) -> [f32; 4] {
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    let span = w.max(h);
    if span <= 0.0 {
        return b;
    }
    let (w, h) = (w * 100.0 / span, h * 100.0 / span);
    [0.0, (100.0 - h) / 2.0, w, (100.0 + h) / 2.0]
}

/// Whether Fit is worth offering for a drawing with margins: it would clear them, or at
/// least move the drawing to the neck edge or make it noticeably bigger. Fit keeps the
/// aspect ratio, so a wide, short drawing can't be fitted to full height; once it has
/// been fitted, Fit is no longer offered (it would do nothing) and the `margins` copy
/// asks for a taller drawing instead.
pub(crate) fn fit_helps(b: [f32; 4]) -> bool {
    let span = (b[2] - b[0]).max(b[3] - b[1]);
    !has_margins(fitted_bbox(b)) || b[0] > MARGIN_MAX_X0 || span < MARGIN_MIN_Y1
}

/// Shape lints for one kind.
///
/// Direction is judged from the drawing's own sides (`Metrics::drawing_edges`), so a
/// padded or short drawing whose neck is on the left isn't mistaken for a rotated one:
/// * the drawing's left side is full height: the neck is on the left. A head whose
///   right side is also more than 60% full gets `faces_up_down`;
/// * otherwise a full-height side whose opposite side looks like a front or tip (below
///   [`FRONT_BELOW_PCT`]) says where the neck went: the right is a mirror (`faces_left` /
///   `tail_reversed`, both offer Flip), the top or bottom a rotation (`faces_up_down` /
///   `tail_reversed`, no fix). A full-height side whose opposite is full too (a head
///   with a flat top and bottom, a block) says nothing: the left side is still the
///   neck, with a gap in it;
/// * a head with no such side and its mass right of x = 52 gets `faces_left`.
///
/// Suppression, so the artist gets one problem and one fix:
/// * `neck_gap` is left out when `margins` offers Fit and the fitted drawing would have
///   a full-height left edge,
/// * `neck_gap` is left out when a direction lint found the full-height side elsewhere,
/// * `faces_up_down` is left out for a solid square (it has a full right side too).
pub(crate) fn for_kind(m: &Metrics, kind: AssetKind) -> Vec<Lint> {
    let head = kind == AssetKind::Head;
    let [own_left, own_right, own_top, own_bottom] = m.drawing_edges;
    let neck_on_left = own_left >= ATTACH_EDGE_MIN_PCT;
    // The drawing's full-height (attach) side, when it isn't the left one: full height,
    // with a front or tip opposite. On ties the last one wins, so Right (which Flip can
    // fix) is last.
    // A drawing whose top and bottom are both full (trans-rights-scarf, whose white
    // stripe crosses the neck) has no front opposite either, so neither counts: its
    // left side is the neck, with a gap.
    let attach_elsewhere = [
        (Edge::Bottom, own_bottom, own_top),
        (Edge::Top, own_top, own_bottom),
        (Edge::Right, own_right, own_left),
    ]
    .into_iter()
    .filter(|&(_, pct, opposite)| {
        !neck_on_left && pct >= ATTACH_EDGE_MIN_PCT && opposite < FRONT_BELOW_PCT
    })
    .max_by(|a, b| a.1.total_cmp(&b.1))
    .map(|(edge, ..)| edge);

    let mut out = Vec::new();
    if m.fill_pct < NEARLY_EMPTY_BELOW_PCT {
        out.push(Lint::NearlyEmpty {
            fill_pct: m.fill_pct,
        });
    }
    let solid_square = m.fill_pct > SOLID_SQUARE_ABOVE_PCT;
    if head && solid_square {
        out.push(Lint::SolidSquare {
            fill_pct: m.fill_pct,
        });
    }

    let margins = m.bbox.filter(|b| has_margins(*b));

    let mut direction = Vec::new();
    if head {
        let centroid_x = m.centroid.map_or(50.0, |[cx, _]| cx);
        match attach_elsewhere {
            Some(Edge::Right) => direction.push(Lint::FacesLeft { centroid_x }),
            Some(Edge::Top | Edge::Bottom) => direction.push(Lint::FacesUpDown {
                right_edge_pct: own_right,
            }),
            None if neck_on_left => {
                if own_right > FACES_UP_DOWN_ABOVE_PCT && !solid_square {
                    direction.push(Lint::FacesUpDown {
                        right_edge_pct: own_right,
                    });
                }
            }
            None => {
                if centroid_x > FACES_LEFT_ABOVE_X {
                    direction.push(Lint::FacesLeft { centroid_x });
                }
            }
        }
    } else if let Some(attach_edge) = attach_elsewhere {
        direction.push(Lint::TailReversed { attach_edge });
    }

    let neck_open = m.left_edge_pct < NECK_GAP_BELOW_PCT;
    // Fit scales the larger side to 100, so the drawing's own left side ends up covering
    // `own_left * h / max(w, h)` of the square's height.
    let gap_explained_by_margins = margins.is_some_and(|b| {
        let (w, h) = (b[2] - b[0], b[3] - b[1]);
        fit_helps(b) && own_left * h / w.max(h) >= NECK_GAP_BELOW_PCT
    });
    let gap_explained_by_direction = attach_elsewhere.is_some() && !direction.is_empty();
    if neck_open && !gap_explained_by_margins && !gap_explained_by_direction {
        out.push(Lint::NeckGap {
            left_edge_pct: m.left_edge_pct,
        });
    }
    if let Some(bbox) = margins {
        out.push(Lint::Margins { bbox });
    }
    out.append(&mut direction);

    if m.hole_pct > OUTLINE_ONLY_ABOVE_PCT {
        out.push(Lint::OutlineOnly {
            hole_pct: m.hole_pct,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Metrics of a drawing that reaches every edge of the square, with these sides
    /// (`[left, right, top, bottom]`) and its centre of mass at x = 45.
    fn edges(e: [f32; 4]) -> Metrics {
        Metrics {
            fill_pct: 70.0,
            left_edge_pct: e[0],
            right_edge_pct: e[1],
            top_edge_pct: e[2],
            bottom_edge_pct: e[3],
            left_edge_gaps: Vec::new(),
            bbox: Some([0.0, 0.0, 100.0, 100.0]),
            drawing_edges: e,
            centroid: Some([45.0, 50.0]),
            hole_pct: 0.0,
            holes: 0,
        }
    }

    fn lint_codes(e: [f32; 4], kind: AssetKind) -> Vec<&'static str> {
        for_kind(&edges(e), kind).iter().map(Lint::code).collect()
    }

    #[test]
    fn a_full_side_is_the_attach_edge_only_opposite_a_front() {
        use AssetKind::{Head, Tail};
        // Notched necks: the top and bottom are both full, or the right is full but the
        // left (its opposite) is mostly there.
        for e in [
            [80.0, 17.0, 100.0, 100.0],
            [80.0, 26.0, 100.0, 89.5],
            [60.0, 17.0, 100.0, 100.0],
            [80.0, 100.0, 100.0, 100.0],
            [60.0, 100.0, 50.0, 50.0],
        ] {
            assert_eq!(lint_codes(e, Head), ["neck_gap"], "{e:?}");
            assert_eq!(lint_codes(e, Tail), ["neck_gap"], "{e:?}");
        }
        // Turned: the top is full and the bottom (the old front) isn't.
        let rotated = [55.5, 51.0, 100.0, 3.5];
        assert_eq!(lint_codes(rotated, Head), ["faces_up_down"]);
        let tail = for_kind(&edges(rotated), Tail);
        assert_eq!(
            tail,
            [Lint::TailReversed {
                attach_edge: Edge::Top
            }]
        );
        assert_eq!(tail[0].fix(), None);
        // Mirrored: the right is full and the left is a front.
        let mirrored = [59.0, 100.0, 100.0, 100.0];
        let head = for_kind(&edges(mirrored), Head);
        assert_eq!(
            head.iter().map(Lint::code).collect::<Vec<_>>(),
            ["faces_left"]
        );
        assert_eq!(head[0].fix(), Some(Fix::Flip));
        assert_eq!(lint_codes(mirrored, Tail), ["tail_reversed"]);
    }

    #[test]
    fn a_tie_between_full_sides_goes_to_the_one_flip_fixes() {
        // The right and the top are both full, each opposite a front. Either reading is
        // possible; the mirror is the one with a fix.
        let tie = [40.0, 100.0, 100.0, 40.0];
        let head = for_kind(&edges(tie), AssetKind::Head);
        assert_eq!(
            head.iter().map(Lint::code).collect::<Vec<_>>(),
            ["faces_left"]
        );
        assert_eq!(head[0].fix(), Some(Fix::Flip));
        let tail = for_kind(&edges(tie), AssetKind::Tail);
        assert_eq!(
            tail,
            [Lint::TailReversed {
                attach_edge: Edge::Right
            }]
        );
        assert_eq!(tail[0].fix(), Some(Fix::Flip));
    }

    #[test]
    fn fit_is_offered_until_it_would_do_nothing() {
        // Padded square-ish drawing: Fit clears the margins.
        assert!(fit_helps([10.0, 10.0, 90.0, 90.0]));
        // Wide and padded: Fit can't reach full height, but it does make it bigger.
        assert!(fit_helps([10.0, 30.0, 90.0, 70.0]));
        // ...and once fitted, Fit would change nothing.
        assert_eq!(
            fitted_bbox([10.0, 30.0, 90.0, 70.0]),
            [0.0, 25.0, 100.0, 75.0]
        );
        assert!(!fit_helps([0.0, 25.0, 100.0, 75.0]));
        // Full width but short: Fit would only re-centre it.
        assert!(!fit_helps([0.0, 0.0, 100.0, 80.0]));
        // Full width and nearly full height: centring clears the margins.
        assert!(fit_helps([0.0, 0.0, 100.0, 95.0]));
    }
}
