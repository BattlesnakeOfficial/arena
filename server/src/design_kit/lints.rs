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

/// How prominently the studio shows a lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

/// Which edge of the square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
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
    /// L3b: the drawing doesn't reach the edges (x0 > 2, y0 > 3 or y1 < 97).
    Margins { bbox: [f32; 4] },
    /// L4 (heads): centre of mass right of x = 52.
    FacesLeft { centroid_x: f32 },
    /// L4b (heads): right-edge coverage > 60% (a flat front, or the neck turned to the
    /// right), or the full-height edge is the top or bottom one instead of the left.
    FacesUpDown { right_edge_pct: f32 },
    /// L4c (tails): the left edge isn't full height (< 85%) but another edge is (≥ 85%).
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
    /// Soft or semi-transparent pixels were thresholded at 50% opacity.
    SemiTransparent,
    /// The canvas wasn't square, so it was centred in one.
    NonSquare { width: u32, height: u32 },
    /// The image is smaller than `Limits::min_useful_side`.
    LowResolution { width: u32, height: u32 },
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
            Lint::SemiTransparent => "semi_transparent",
            Lint::NonSquare { .. } => "non_square",
            Lint::LowResolution { .. } => "low_resolution",
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
            Lint::NonSquare { .. } | Lint::LowResolution { .. } => Severity::Tip,
            Lint::SpecksRemoved { .. }
            | Lint::ColoursFlattened
            | Lint::GuidesVisible
            | Lint::SemiTransparent => Severity::Info,
        }
    }

    /// The one-tap fix the studio can offer next to this lint.
    pub fn fix(&self) -> Option<Fix> {
        match self {
            Lint::Margins { .. } => Some(Fix::Fit),
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
            Lint::NearlyEmpty { .. } | Lint::OutlineOnly { .. } => "#fill",
            Lint::SolidSquare { .. } => "#holes",
            Lint::SpecksRemoved { .. } | Lint::LowResolution { .. } => "#small",
            Lint::ColoursFlattened | Lint::SemiTransparent => "#colour",
            Lint::GuidesVisible => "#guides",
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
            Lint::Margins { .. } => "Your drawing doesn't reach the edges of the square, so it'll \
                 look small and leave a gap at the neck. Tap Fit to stretch it, or draw edge to \
                 edge."
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
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Shape lints for one kind.
///
/// Suppression, so the artist gets one problem and one fix:
/// * `neck_gap` is left out when `margins` fires and Fit would close the gap (the strip
///   at the drawing's own left side is full),
/// * `neck_gap` is left out when a direction lint (`faces_left`, `faces_up_down`,
///   `tail_reversed`) fires and the full-height edge is on another side, and
/// * `faces_up_down` is left out when `faces_left` or `solid_square` fires (a mirrored
///   head and a solid square have a full right edge too).
pub(crate) fn for_kind(m: &Metrics, kind: AssetKind) -> Vec<Lint> {
    let head = kind == AssetKind::Head;
    let neck_open = m.left_edge_pct < NECK_GAP_BELOW_PCT;
    // The full-height (attach) edge is somewhere other than the left.
    // On ties the last one wins, so Right (which Flip can fix) is last.
    let attach_elsewhere = [
        (Edge::Bottom, m.bottom_edge_pct),
        (Edge::Top, m.top_edge_pct),
        (Edge::Right, m.right_edge_pct),
    ]
    .into_iter()
    .filter(|(_, pct)| neck_open && *pct >= ATTACH_EDGE_MIN_PCT)
    .max_by(|a, b| a.1.total_cmp(&b.1))
    .map(|(edge, _)| edge);

    let mut out = Vec::new();
    if m.fill_pct < NEARLY_EMPTY_BELOW_PCT {
        out.push(Lint::NearlyEmpty {
            fill_pct: m.fill_pct,
        });
    }
    if head && m.fill_pct > SOLID_SQUARE_ABOVE_PCT {
        out.push(Lint::SolidSquare {
            fill_pct: m.fill_pct,
        });
    }

    let margins = m
        .bbox
        .filter(|b| b[0] > MARGIN_MAX_X0 || b[1] > MARGIN_MAX_Y0 || b[3] < MARGIN_MIN_Y1);

    let mut direction = Vec::new();
    if head {
        let faces_left = m
            .centroid
            .map(|[cx, _]| cx)
            .filter(|&cx| cx > FACES_LEFT_ABOVE_X);
        // Right edge filled like a neck, or the neck is on the top or bottom edge. A
        // mirrored head (faces_left) and a solid square also have a full right edge;
        // their own lint already explains it.
        let neck_top_or_bottom = matches!(attach_elsewhere, Some(Edge::Top | Edge::Bottom));
        let faces_up_down = (m.right_edge_pct > FACES_UP_DOWN_ABOVE_PCT || neck_top_or_bottom)
            && faces_left.is_none()
            && m.fill_pct <= SOLID_SQUARE_ABOVE_PCT;
        if let Some(centroid_x) = faces_left {
            direction.push(Lint::FacesLeft { centroid_x });
        }
        if faces_up_down {
            direction.push(Lint::FacesUpDown {
                right_edge_pct: m.right_edge_pct,
            });
        }
    } else if let Some(attach_edge) = attach_elsewhere {
        direction.push(Lint::TailReversed { attach_edge });
    }

    let gap_explained_by_margins = margins.is_some() && m.fit_left_edge_pct >= NECK_GAP_BELOW_PCT;
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
