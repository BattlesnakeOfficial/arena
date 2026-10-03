//! Binary mask -> splines via visioncortex (the engine behind vtracer 0.6.x binary mode).
//!
//! Equivalent to vtracer 0.6.5 `binary_image_to_svg` with its default spline settings,
//! except:
//! * specks AND pinholes are removed here first (vtracer only filters ink clusters),
//! * the corner threshold is 40° instead of 60°, so 45° joints (chevron tails, smile's
//!   mouth) stay sharp instead of bulging (tails/default round trip IoU 0.974 -> 0.994,
//!   smile 0.991 -> 0.997, and shorter output), and
//! * geometry stays as numbers instead of an SVG string.

use super::raster::components;
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
pub(crate) fn trace_mask(
    mut mask: Vec<bool>,
    side: usize,
    limits: &Limits,
) -> Result<TraceResult, ProcessError> {
    let px_per_unit = side as f32 / 100.0;
    let min_area = (SPECK_UNITS * px_per_unit).powi(2).ceil() as usize;
    let specks_removed = despeckle(&mut mask, side, min_area);
    if !mask.contains(&true) {
        return Ok(TraceResult {
            path: None,
            specks_removed,
        });
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
        if clusters.len() > limits.max_trace_clusters {
            return Err(ProcessError::TooComplex("too many separate shapes"));
        }
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
        Ok(pb.finish())
    });
    match result {
        Ok(Ok(path)) => Ok(TraceResult {
            path,
            specks_removed,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ProcessError::Internal("tracer panicked")),
    }
}

/// Remove ink pieces smaller than `min_area` px and fill enclosed holes smaller than it.
/// Returns how many pieces + holes were removed.
fn despeckle(mask: &mut [bool], side: usize, min_area: usize) -> usize {
    let mut removed = 0;
    let (labels, comps) = components(mask, side, side, true);
    removed += comps.iter().filter(|c| c.area < min_area).count();
    for (i, l) in labels.iter().enumerate() {
        if let Some(c) = comps.get(*l as usize)
            && c.area < min_area
        {
            mask[i] = false;
        }
    }
    let (labels, comps) = components(mask, side, side, false);
    let is_pinhole = |c: &super::raster::Component| !c.touches_border && c.area < min_area;
    removed += comps.iter().filter(|c| is_pinhole(c)).count();
    for (i, l) in labels.iter().enumerate() {
        if let Some(c) = comps.get(*l as usize)
            && is_pinhole(c)
        {
            mask[i] = true;
        }
    }
    removed
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
        assert_eq!(despeckle(&mut mask, side, 4), 2);
        assert!(mask[10 * side + 10]);
        assert!(!mask[side + 1]);
        assert_eq!(mask.iter().filter(|&&m| m).count(), 100);
    }
}
