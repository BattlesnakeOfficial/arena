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
/// the 100x100 viewBox. Serialized field for field (the studio brackets the
/// `left_edge_gaps` on its close-up).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Edge coverage of the drawing itself, `[left, right, top, bottom]`: the 1-unit
    /// strip along each side of `bbox`, over the rows (or columns) `bbox` spans. The same
    /// as the square's edges when the drawing reaches every edge; for a padded or short
    /// drawing it still shows which side is full height (where the neck is). All 0 when
    /// nothing is filled.
    pub drawing_edges: [f32; 4],
    /// Centre of mass of the ink `[x, y]`; `None` when nothing is filled.
    pub centroid: Option<[f32; 2]>,
    /// Enclosed holes as a share of the silhouette with its holes filled in. High for
    /// outline drawings.
    pub hole_pct: f32,
    /// Enclosed holes: 4-connected background regions that don't touch the border (the
    /// same holes `hole_pct` and SVG's fill rules see).
    pub holes: usize,
}

impl Metrics {
    /// Measure a `side`x`side` alpha mask (row-major, 0..=255) covering the 100x100 box.
    /// The lint thresholds assume `side == METRIC_SIDE`.
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
        let (bbox, centroid, drawing_edges) = if filled == 0 {
            (None, None, [0.0; 4])
        } else {
            let u = units_per_px as f64;
            // Strips along the sides of the drawing's own bounds.
            let (rows, cols) = (y0..y1, x0..x1);
            let left = (x0..(x0 + k).min(x1)).collect::<Vec<_>>();
            let right = (x1.saturating_sub(k).max(x0)..x1).collect::<Vec<_>>();
            let top = (y0..(y0 + k).min(y1)).collect::<Vec<_>>();
            let bottom = (y1.saturating_sub(k).max(y0)..y1).collect::<Vec<_>>();
            let full_rows = |strip: &[usize]| {
                rows.clone()
                    .filter(|&y| strip_full(strip.iter().filter(|&&x| at(x, y)).count()))
                    .count()
            };
            let full_cols = |strip: &[usize]| {
                cols.clone()
                    .filter(|&x| strip_full(strip.iter().filter(|&&y| at(x, y)).count()))
                    .count()
            };
            (
                Some([x0, y0, x1, y1].map(|v| v as f32 * units_per_px)),
                Some([
                    (sx / filled as f64 * u) as f32,
                    (sy / filled as f64 * u) as f32,
                ]),
                [
                    pct(full_rows(&left), y1 - y0),
                    pct(full_rows(&right), y1 - y0),
                    pct(full_cols(&top), x1 - x0),
                    pct(full_cols(&bottom), x1 - x0),
                ],
            )
        };

        // Holes: background regions that don't reach the border.
        let (_, bg) = components(ink, side, side, false);
        let enclosed: Vec<&Component> = bg.iter().filter(|c| !c.touches_border).collect();
        let hole_area: usize = enclosed.iter().map(|c| c.area).sum();

        Metrics {
            fill_pct: pct(filled, n),
            left_edge_pct: pct(left, side),
            right_edge_pct: pct(right, side),
            top_edge_pct: pct(top, side),
            bottom_edge_pct: pct(bottom, side),
            left_edge_gaps,
            bbox,
            drawing_edges,
            centroid,
            hole_pct: pct(hole_area, filled + hole_area),
            holes: enclosed.len(),
        }
    }

    /// Measure a clean path (0..100 space) on a [`METRIC_SIDE`] mask.
    pub(crate) fn of_path(path: &tiny_skia::Path, rule: FillRule) -> Result<Metrics, ProcessError> {
        let alpha = render_alpha(path, rule, METRIC_SIDE)?;
        Ok(Metrics::from_alpha(&alpha, METRIC_SIDE as usize))
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
    /// Pixel bounds `[x0, y0, x1, y1)`.
    pub bbox: [usize; 4],
}

impl Component {
    pub fn bbox_area(&self) -> usize {
        (self.bbox[2] - self.bbox[0]) * (self.bbox[3] - self.bbox[1])
    }
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
            bbox: [usize::MAX, usize::MAX, 0, 0],
        };
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            c.area += 1;
            let (x, y) = (i % w, i / w);
            c.bbox = [
                c.bbox[0].min(x),
                c.bbox[1].min(y),
                c.bbox[2].max(x + 1),
                c.bbox[3].max(y + 1),
            ];
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
        assert_eq!(m.holes, 0);
        assert_eq!(m.drawing_edges, [100.0; 4]);
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
        assert_eq!(m.drawing_edges, [80.0, 100.0, 100.0, 100.0]);
    }

    #[test]
    fn drawing_edges_follow_the_drawing_not_the_square() {
        // A short, padded bar: none of the square's edges are touched, but the drawing's
        // own left and right sides are full height and its top and bottom full width.
        let m = Metrics::from_alpha(
            &mask(|x, y| (20.0..60.0).contains(&x) && (30.0..50.0).contains(&y)),
            200,
        );
        assert_eq!(m.left_edge_pct, 0.0);
        assert_eq!(m.bbox, Some([20.0, 30.0, 60.0, 50.0]));
        assert_eq!(m.drawing_edges, [100.0; 4]);
        // A right triangle (full left side, a point on the right).
        let m = Metrics::from_alpha(&mask(|x, y| y >= x), 200);
        assert_eq!(m.drawing_edges[0], 100.0);
        assert!(m.drawing_edges[1] < 5.0, "{m:?}");
    }

    #[test]
    fn empty_mask() {
        let m = Metrics::from_alpha(&mask(|_, _| false), 200);
        assert_eq!(m.fill_pct, 0.0);
        assert_eq!(m.bbox, None);
        assert_eq!(m.drawing_edges, [0.0; 4]);
        assert_eq!(m.left_edge_gaps, vec![[0.0, 100.0]]);
    }
}
