//! Rasterisation (tiny-skia) and the shape metrics behind the lints.
//!
//! Every metric is measured on one [`METRIC_SIDE`] px mask of the clean path, the same
//! resolution the catalog thresholds were derived at (rsvg renders of all 184 official
//! assets; see `docs/design-kit.md`). A pixel is filled when its alpha is > 127.

use super::{FillRule, ProcessError};
use tiny_skia::{Paint, Pixmap, Transform};

/// Side of the mask the metrics are measured on (2 px per unit).
pub const METRIC_SIDE: u32 = 200;

/// Most left-edge gap ranges reported (the longest ones win).
const MAX_GAP_RANGES: usize = 16;

/// Shape metrics of a 100x100 asset. Percentages are 0..=100; positions are in units of
/// the 100x100 viewBox.
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    /// Share of the square covered.
    pub fill_pct: f32,
    /// Share of rows whose leftmost 1-unit strip is at least half filled (the neck or
    /// body joint).
    pub left_edge_pct: f32,
    /// Same, for the rightmost strip.
    pub right_edge_pct: f32,
    /// Share of columns whose top 1-unit strip is at least half filled.
    pub top_edge_pct: f32,
    /// Same, for the bottom strip.
    pub bottom_edge_pct: f32,
    /// Vertical ranges `[y0, y1)` where the left edge is open, top to bottom. At most
    /// 16 (the longest).
    pub left_edge_gaps: Vec<[f32; 2]>,
    /// Ink bounds `[x0, y0, x1, y1]` on the mask; `None` when nothing is filled.
    pub bbox: Option<[f32; 4]>,
    /// Centre of mass of the ink `[x, y]`; `None` when nothing is filled.
    pub centroid: Option<[f32; 2]>,
    /// Left-edge coverage the shape would have after Fit: the strip at the left side of
    /// `bbox`, over the rows `bbox` spans.
    pub fit_left_edge_pct: f32,
    /// Enclosed holes as a share of the silhouette with its holes filled in. High for
    /// outline drawings.
    pub hole_pct: f32,
    /// Enclosed holes (4-connected background regions not touching the border).
    pub holes: usize,
    /// Separate filled pieces (4-connected).
    pub pieces: usize,
    /// Subpaths in the clean path (0 when measured from a mask alone).
    pub subpaths: usize,
    /// Segments in the clean path (0 when measured from a mask alone).
    pub segments: usize,
}

impl Metrics {
    /// Measure a `side`x`side` alpha mask (row-major, 0..=255) covering the 100x100 box.
    /// The lint thresholds assume `side == METRIC_SIDE`. Path statistics are left at 0.
    pub fn from_alpha(alpha: &[u8], side: usize) -> Metrics {
        let ink: Vec<bool> = alpha.iter().map(|&a| a > 127).collect();
        Metrics::from_mask(&ink, side)
    }

    fn from_mask(ink: &[bool], side: usize) -> Metrics {
        let n = side * side;
        let units_per_px = 100.0 / side as f32;
        let pct = |count: usize, of: usize| count as f32 * 100.0 / of.max(1) as f32;
        let at = |x: usize, y: usize| ink.get(y * side + x).copied().unwrap_or(false);
        // Strip width: 1% of the side (2 px at 200), as in the catalog analysis.
        let k = ((side as f32 * 0.01).round() as usize).clamp(1, side.max(1));
        // A row/column counts when at least half of its k-px strip is filled.
        let strip_full = |filled: usize| filled * 2 >= k;

        let mut left_rows = Vec::with_capacity(side);
        let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
        for i in 0..side {
            let l = strip_full((0..k).filter(|&x| at(x, i)).count());
            left_rows.push(l);
            left += usize::from(l);
            right += usize::from(strip_full((side - k..side).filter(|&x| at(x, i)).count()));
            top += usize::from(strip_full((0..k).filter(|&y| at(i, y)).count()));
            bottom += usize::from(strip_full((side - k..side).filter(|&y| at(i, y)).count()));
        }

        // Left-edge gap ranges.
        let mut gaps: Vec<[usize; 2]> = Vec::new();
        for (y, &covered) in left_rows.iter().enumerate() {
            if covered {
                continue;
            }
            match gaps.last_mut() {
                Some(g) if g[1] == y => g[1] = y + 1,
                _ => gaps.push([y, y + 1]),
            }
        }
        if gaps.len() > MAX_GAP_RANGES {
            gaps.sort_by_key(|g| std::cmp::Reverse(g[1] - g[0]));
            gaps.truncate(MAX_GAP_RANGES);
            gaps.sort_by_key(|g| g[0]);
        }
        let left_edge_gaps = gaps
            .iter()
            .map(|g| [g[0] as f32 * units_per_px, g[1] as f32 * units_per_px])
            .collect();

        // Bounds, centroid, fill.
        let (mut filled, mut sx, mut sy) = (0usize, 0f64, 0f64);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..side {
            for x in 0..side {
                if at(x, y) {
                    filled += 1;
                    sx += x as f64 + 0.5;
                    sy += y as f64 + 0.5;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        let (bbox, centroid, fit_left_edge_pct) = if filled == 0 {
            (None, None, 0.0)
        } else {
            let u = units_per_px as f64;
            let fit_rows = (y0..y1)
                .filter(|&y| strip_full((x0..(x0 + k).min(side)).filter(|&x| at(x, y)).count()))
                .count();
            (
                Some([x0, y0, x1, y1].map(|v| v as f32 * units_per_px)),
                Some([
                    (sx / filled as f64 * u) as f32,
                    (sy / filled as f64 * u) as f32,
                ]),
                pct(fit_rows, y1 - y0),
            )
        };

        // Holes: background regions that don't reach the border.
        let (_, bg) = components(ink, side, side, false);
        let enclosed: Vec<&Component> = bg.iter().filter(|c| !c.touches_border).collect();
        let hole_area: usize = enclosed.iter().map(|c| c.area).sum();
        let (_, pieces) = components(ink, side, side, true);

        Metrics {
            fill_pct: pct(filled, n),
            left_edge_pct: pct(left, side),
            right_edge_pct: pct(right, side),
            top_edge_pct: pct(top, side),
            bottom_edge_pct: pct(bottom, side),
            left_edge_gaps,
            bbox,
            centroid,
            fit_left_edge_pct,
            hole_pct: pct(hole_area, filled + hole_area),
            holes: enclosed.len(),
            pieces: pieces.len(),
            subpaths: 0,
            segments: 0,
        }
    }

    /// Measure a clean path (0..100 space) on a [`METRIC_SIDE`] mask.
    pub(crate) fn of_path(path: &tiny_skia::Path, rule: FillRule) -> Result<Metrics, ProcessError> {
        let alpha = render_alpha(path, rule, METRIC_SIDE)?;
        let mut m = Metrics::from_alpha(&alpha, METRIC_SIDE as usize);
        for seg in path.segments() {
            m.segments += 1;
            if matches!(seg, tiny_skia::PathSegment::MoveTo(_)) {
                m.subpaths += 1;
            }
        }
        Ok(m)
    }
}

/// Render `path` (0..100 space) into a `side`x`side` anti-aliased alpha mask.
pub(crate) fn render_alpha(
    path: &tiny_skia::Path,
    rule: FillRule,
    side: u32,
) -> Result<Vec<u8>, ProcessError> {
    let mut pm = Pixmap::new(side, side).ok_or(ProcessError::Internal(
        "could not allocate the metrics raster",
    ))?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    paint.anti_alias = true;
    let s = side as f32 / 100.0;
    pm.fill_path(
        path,
        &paint,
        rule.to_skia(),
        Transform::from_scale(s, s),
        None,
    );
    Ok(pm
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| px[3])
        .collect())
}

/// One 4-connected region of a mask.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Component {
    pub area: usize,
    pub touches_border: bool,
}

/// 4-connected component labelling of pixels where `mask[i] == want`.
/// Returns per-pixel labels (`u32::MAX` for "not this class") and the components.
pub(crate) fn components(
    mask: &[bool],
    w: usize,
    h: usize,
    want: bool,
) -> (Vec<u32>, Vec<Component>) {
    let mut labels = vec![u32::MAX; w * h];
    let mut comps = Vec::new();
    let mut stack = Vec::new();
    for start in 0..(w * h).min(mask.len()) {
        if mask[start] != want || labels[start] != u32::MAX {
            continue;
        }
        let id = comps.len() as u32;
        let mut c = Component {
            area: 0,
            touches_border: false,
        };
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            c.area += 1;
            let (x, y) = (i % w, i / w);
            if x == 0 || y == 0 || x == w - 1 || y == h - 1 {
                c.touches_border = true;
            }
            let mut visit = |j: usize| {
                if mask[j] == want && labels[j] == u32::MAX {
                    labels[j] = id;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        comps.push(c);
    }
    (labels, comps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(f: impl Fn(f32, f32) -> bool) -> Vec<u8> {
        let side = METRIC_SIDE as usize;
        let mut v = Vec::with_capacity(side * side);
        for y in 0..side {
            for x in 0..side {
                let (u, w) = ((x as f32 + 0.5) / 2.0, (y as f32 + 0.5) / 2.0);
                v.push(if f(u, w) { 255 } else { 0 });
            }
        }
        v
    }

    #[test]
    fn full_square() {
        let m = Metrics::from_alpha(&mask(|_, _| true), 200);
        assert_eq!(m.fill_pct, 100.0);
        assert_eq!(m.left_edge_pct, 100.0);
        assert_eq!(m.right_edge_pct, 100.0);
        assert_eq!(m.bbox, Some([0.0, 0.0, 100.0, 100.0]));
        assert_eq!(m.centroid, Some([50.0, 50.0]));
        assert!(m.left_edge_gaps.is_empty());
        assert_eq!(m.hole_pct, 0.0);
        assert_eq!((m.holes, m.pieces), (0, 1));
    }

    #[test]
    fn ring_hole_and_gaps() {
        // Left half of a square frame: holes, and the left edge open between 40 and 60.
        let m = Metrics::from_alpha(
            &mask(|x, y| {
                let frame = !(20.0..80.0).contains(&x) || !(20.0..80.0).contains(&y);
                frame && !(x < 10.0 && (40.0..60.0).contains(&y))
            }),
            200,
        );
        assert_eq!(m.holes, 1);
        // 14400 hole px / (24800 ink + 14400 hole) px.
        assert!((m.hole_pct - 36.73).abs() < 0.01, "{m:?}");
        assert_eq!(m.left_edge_gaps, vec![[40.0, 60.0]]);
        assert_eq!(m.left_edge_pct, 80.0);
        assert_eq!(m.fit_left_edge_pct, 80.0);
    }

    #[test]
    fn empty_mask() {
        let m = Metrics::from_alpha(&mask(|_, _| false), 200);
        assert_eq!(m.fill_pct, 0.0);
        assert_eq!(m.bbox, None);
        assert_eq!(m.left_edge_gaps, vec![[0.0, 100.0]]);
    }
}
