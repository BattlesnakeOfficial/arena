//! One-tap fixes: affine rewrites of the clean path (absolute M/L/Q/C points).

use super::Fix;
use tiny_skia::Transform;

/// Apply `fix` to a path in 0..100 space. Returns the path unchanged when there is
/// nothing to fit (an empty or zero-size shape).
pub(crate) fn apply(path: tiny_skia::Path, fix: Fix) -> tiny_skia::Path {
    let Some(ts) = transform(&path, fix) else {
        return path;
    };
    // `transform` only fails on non-finite results, which our transforms can't produce
    // from a finite path; fall back to the original rather than lose the shape.
    path.clone().transform(ts).unwrap_or(path)
}

/// The transform for `fix` on this path.
///
/// * Flip: mirror horizontally, `x -> 100 - x` (heads face right, tails point right).
/// * Fit: scale uniformly so the bounding box fills 0..100 on its larger side, keeping
///   the aspect ratio; the left edge goes to x = 0 (the neck/body joint) and the shape is
///   centred vertically. The box is the visible part (clipped to the square): spline
///   overshoot past the canvas edge is cut off by the board anyway.
fn transform(path: &tiny_skia::Path, fix: Fix) -> Option<Transform> {
    match fix {
        Fix::Flip => Some(Transform::from_row(-1.0, 0.0, 0.0, 1.0, 100.0, 0.0)),
        Fix::Fit => {
            let b = path.compute_tight_bounds()?;
            let (x0, y0) = (b.left().max(0.0), b.top().max(0.0));
            let (x1, y1) = (b.right().min(100.0), b.bottom().min(100.0));
            let (w, h) = (x1 - x0, y1 - y0);
            if w <= f32::EPSILON || h <= f32::EPSILON {
                return None;
            }
            let s = (100.0 / w).min(100.0 / h);
            let tx = -x0 * s;
            let ty = (100.0 - h * s) / 2.0 - y0 * s;
            Some(Transform::from_row(s, 0.0, 0.0, s, tx, ty))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> tiny_skia::Path {
        let r = tiny_skia::Rect::from_ltrb(x0, y0, x1, y1).unwrap();
        tiny_skia::PathBuilder::from_rect(r)
    }

    fn bounds(p: &tiny_skia::Path) -> [f32; 4] {
        let b = p.compute_tight_bounds().unwrap();
        [b.left(), b.top(), b.right(), b.bottom()]
    }

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn flip_mirrors() {
        let p = apply(rect(10.0, 20.0, 30.0, 90.0), Fix::Flip);
        assert!(
            close(bounds(&p), [70.0, 20.0, 90.0, 90.0]),
            "{:?}",
            bounds(&p)
        );
    }

    #[test]
    fn fit_fills_the_square() {
        let p = apply(rect(10.0, 10.0, 90.0, 90.0), Fix::Fit);
        assert!(
            close(bounds(&p), [0.0, 0.0, 100.0, 100.0]),
            "{:?}",
            bounds(&p)
        );
    }

    #[test]
    fn fit_keeps_aspect_and_anchors_left() {
        // Tall: height binds, left-anchored.
        let p = apply(rect(30.0, 10.0, 50.0, 60.0), Fix::Fit);
        assert!(
            close(bounds(&p), [0.0, 0.0, 40.0, 100.0]),
            "{:?}",
            bounds(&p)
        );
        // Wide: width binds, centred vertically.
        let p = apply(rect(10.0, 40.0, 60.0, 60.0), Fix::Fit);
        assert!(
            close(bounds(&p), [0.0, 30.0, 100.0, 70.0]),
            "{:?}",
            bounds(&p)
        );
    }

    #[test]
    fn fit_measures_only_the_visible_part() {
        // Overshoot past the canvas (left and bottom) doesn't count as drawing.
        let p = apply(rect(-5.0, 10.0, 50.0, 105.0), Fix::Fit);
        let s = 100.0 / 90.0;
        let want = [-5.0 * s, 0.0, 50.0 * s, 100.0 + 5.0 * s];
        assert!(close(bounds(&p), want), "{:?}", bounds(&p));
    }
}
