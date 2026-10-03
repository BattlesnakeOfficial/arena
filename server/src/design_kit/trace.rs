//! Binary mask -> splines via visioncortex (the engine behind vtracer 0.6.x binary mode).
//!
//! Equivalent to vtracer 0.6.5 `binary_image_to_svg` with its default spline settings,
//! except:
//! * specks AND pinholes are removed here first (vtracer only filters ink clusters),
//! * the corner threshold is 40° instead of 60°, so 45° joints (chevron tails, smile's
//!   mouth) stay sharp instead of bulging (tails/default round trip IoU 0.974 -> 0.994,
//!   smile 0.991 -> 0.997, and shorter output),
//! * the work is budgeted before visioncortex runs (see [`trace_mask`]), and
//! * geometry stays as numbers instead of an SVG string.
//!
//! visioncortex's cost is not linear in the image: each cluster's whole bounding box is
//! rescanned (and its holes' boxes), and path simplification is quadratic along
//! perfectly straight runs, such as 45° staircases. It also panics past internal limits:
//! a `u16` cluster index (65535 provisional clusters per scan) and a 1,000,000-step
//! outline walk. A 3.5 KB PNG of diagonal stripes used to take over 2 s in release, and
//! a comb or a 1 px serpentine panicked.

use super::raster::{Component, components};
use super::{Limits, ProcessError, SPECK_UNITS};
use tiny_skia::PathBuilder;
use visioncortex::{BinaryImage, PathSimplifyMode};

// vtracer 0.6.5 ConverterConfig::default() is corner 60deg, length 4.0, splice 45deg,
// 10 iterations; we use a 40deg corner threshold (see the module docs).
const CORNER_THRESHOLD_DEG: f64 = 40.0;
const LENGTH_THRESHOLD: f64 = 4.0;
const SPLICE_THRESHOLD_DEG: f64 = 45.0;
const MAX_ITERATIONS: usize = 10;

pub(crate) struct TraceResult {
    /// `None` when nothing survived speck removal.
    pub path: Option<tiny_skia::Path>,
    /// Ink pieces and enclosed holes smaller than [`SPECK_UNITS`]² that were removed.
    pub specks_removed: usize,
}

/// `mask` is a `side`x`side` grid covering the 100x100 box. Returns a path in 0..100 space.
///
/// Holes come out with the opposite winding (visioncortex traces holes the other way
/// round), so both evenodd and nonzero render them; we emit evenodd.
///
/// Before tracing, the cleaned mask must fit the budget in `limits`, else `TooComplex`:
/// * at most `max_trace_clusters` separate shapes,
/// * at most `max_trace_edges_per_side * side` boundary edges (ink pixel sides that face
///   background or the grid border). Every new provisional cluster in a visioncortex
///   scan starts at a pixel whose top and left sides are boundary edges, and every
///   outline walk is one closed boundary, so this also keeps the scans under 65535
///   clusters and the walks under 1,000,000 steps (the budget is clamped to make sure),
/// * the shapes' and holes' bounding boxes cover at most `max_trace_box_cover` times the
///   grid.
pub(crate) fn trace_mask(
    mut mask: Vec<bool>,
    side: usize,
    limits: &Limits,
) -> Result<TraceResult, ProcessError> {
    let px_per_unit = side as f32 / 100.0;
    let min_area = (SPECK_UNITS * px_per_unit).powi(2).ceil() as usize;
    let cleaned = despeckle(&mut mask, side, min_area);
    let specks_removed = cleaned.removed;
    if cleaned.clusters == 0 {
        return Ok(TraceResult {
            path: None,
            specks_removed,
        });
    }
    if cleaned.clusters > limits.max_trace_clusters {
        return Err(ProcessError::TooComplex("too many separate shapes"));
    }
    // A cluster scan sees at most edges/2 new clusters plus one per pixel of its box's
    // top row and left column; keep that under visioncortex's u16 index.
    let visioncortex_safe = 2 * (u16::MAX as usize).saturating_sub(2 * side + 2);
    let max_edges = (limits.max_trace_edges_per_side * side).min(visioncortex_safe);
    if boundary_edges(&mask, side) > max_edges {
        return Err(ProcessError::TooComplex("the outline is too long"));
    }
    if cleaned.box_area > limits.max_trace_box_cover * side * side {
        return Err(ProcessError::TooComplex(
            "too many large or interleaved shapes",
        ));
    }

    // The cluster walker has a few panics ("STUCK", "no way to go?"); never let them
    // unwind into the caller. A caught panic is a bug, reported as `Internal`.
    let result = std::panic::catch_unwind(move || {
        let mut img = BinaryImage::new_w_h(side, side);
        for (i, &m) in mask.iter().enumerate() {
            if m {
                img.set_pixel(i % side, i / side, true);
            }
        }
        let clusters = img.to_clusters(false);
        let mut pb = PathBuilder::new();
        let scale = 1.0 / px_per_unit as f64;
        for cluster in clusters.iter() {
            let compound = cluster.to_compound_path(
                PathSimplifyMode::Spline,
                CORNER_THRESHOLD_DEG.to_radians(),
                LENGTH_THRESHOLD,
                MAX_ITERATIONS,
                SPLICE_THRESHOLD_DEG.to_radians(),
            );
            for el in compound.iter() {
                if let visioncortex::CompoundPathElement::Spline(s) = el {
                    // Spline points: p0, then (c1, c2, p) triples. Degenerate splines come
                    // back as a single (0,0) point; skip them.
                    if s.points.len() < 4 || (s.points.len() - 1) % 3 != 0 {
                        continue;
                    }
                    let p0 = s.points[0];
                    pb.move_to((p0.x * scale) as f32, (p0.y * scale) as f32);
                    for c in s.points[1..].as_chunks::<3>().0 {
                        pb.cubic_to(
                            (c[0].x * scale) as f32,
                            (c[0].y * scale) as f32,
                            (c[1].x * scale) as f32,
                            (c[1].y * scale) as f32,
                            (c[2].x * scale) as f32,
                            (c[2].y * scale) as f32,
                        );
                    }
                    pb.close();
                }
            }
        }
        pb.finish()
    });
    match result {
        Ok(path) => Ok(TraceResult {
            path,
            specks_removed,
        }),
        Err(_) => Err(ProcessError::Internal("tracer panicked")),
    }
}

/// What [`despeckle`] removed and what it left for the tracer.
struct Cleaned {
    /// Ink pieces + enclosed holes removed.
    removed: usize,
    /// Ink pieces left. (Filling a pinhole can join two pieces, so this may overcount.)
    clusters: usize,
    /// Total bounding-box area of the ink pieces and enclosed holes left, in px.
    box_area: usize,
}

/// Remove ink pieces smaller than `min_area` px and fill enclosed holes smaller than it.
fn despeckle(mask: &mut [bool], side: usize, min_area: usize) -> Cleaned {
    let (labels, comps) = components(mask, side, side, true);
    let mut removed = comps.iter().filter(|c| c.area < min_area).count();
    for (i, l) in labels.iter().enumerate() {
        if let Some(c) = comps.get(*l as usize)
            && c.area < min_area
        {
            mask[i] = false;
        }
    }
    let kept = || comps.iter().filter(|c| c.area >= min_area);
    let clusters = kept().count();
    let mut box_area: usize = kept().map(Component::bbox_area).sum();

    let (labels, comps) = components(mask, side, side, false);
    let is_pinhole = |c: &Component| !c.touches_border && c.area < min_area;
    removed += comps.iter().filter(|c| is_pinhole(c)).count();
    for (i, l) in labels.iter().enumerate() {
        if let Some(c) = comps.get(*l as usize)
            && is_pinhole(c)
        {
            mask[i] = true;
        }
    }
    box_area += comps
        .iter()
        .filter(|c| !c.touches_border && !is_pinhole(c))
        .map(Component::bbox_area)
        .sum::<usize>();
    Cleaned {
        removed,
        clusters,
        box_area,
    }
}

/// Ink pixel sides that face background or the grid border.
fn boundary_edges(mask: &[bool], side: usize) -> usize {
    let at = |x: usize, y: usize| mask.get(y * side + x).copied().unwrap_or(false);
    let mut edges = 0;
    for y in 0..side {
        for x in 0..side {
            if !at(x, y) {
                continue;
            }
            edges += usize::from(x == 0 || !at(x - 1, y))
                + usize::from(x + 1 == side || !at(x + 1, y))
                + usize::from(y == 0 || !at(x, y - 1))
                + usize::from(y + 1 == side || !at(x, y + 1));
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn despeckle_removes_small_pieces_and_pinholes() {
        let side = 20;
        let mut mask = vec![false; side * side];
        // A 10x10 block with a 1px pinhole, plus a lone 1px speck.
        for y in 5..15 {
            for x in 5..15 {
                mask[y * side + x] = true;
            }
        }
        mask[10 * side + 10] = false;
        mask[side + 1] = true;
        let cleaned = despeckle(&mut mask, side, 4);
        assert_eq!(
            (cleaned.removed, cleaned.clusters, cleaned.box_area),
            (2, 1, 100)
        );
        assert!(mask[10 * side + 10]);
        assert!(!mask[side + 1]);
        assert_eq!(mask.iter().filter(|&&m| m).count(), 100);
        assert_eq!(boundary_edges(&mask, side), 40);
    }

    #[test]
    fn boundary_edges_count_the_grid_border() {
        assert_eq!(boundary_edges(&[true; 9], 3), 12);
        assert_eq!(boundary_edges(&[false; 9], 3), 0);
        // A plus sign: 5 pixels, 12 exposed sides.
        let plus = [false, true, false, true, true, true, false, true, false];
        assert_eq!(boundary_edges(&plus, 3), 12);
    }
}
