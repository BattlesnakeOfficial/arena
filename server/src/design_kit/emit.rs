//! Path -> `d` string. Output alphabet is `MLQCZ0-9.- ` only.

use std::fmt::Write as _;
use tiny_skia::{PathSegment, Point};

/// Absolute commands, 2 decimals (0.01 of a cell), trailing zeros trimmed.
pub(crate) fn path_to_d(path: &tiny_skia::Path) -> String {
    let mut d = String::with_capacity(path.len() * 12);
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                d.push('M');
                pt(&mut d, p);
            }
            PathSegment::LineTo(p) => {
                d.push('L');
                pt(&mut d, p);
            }
            PathSegment::QuadTo(a, p) => {
                d.push('Q');
                pt(&mut d, a);
                d.push(' ');
                pt(&mut d, p);
            }
            PathSegment::CubicTo(a, b, p) => {
                d.push('C');
                pt(&mut d, a);
                d.push(' ');
                pt(&mut d, b);
                d.push(' ');
                pt(&mut d, p);
            }
            PathSegment::Close => d.push('Z'),
        }
    }
    d
}

fn pt(d: &mut String, p: Point) {
    num(d, p.x);
    d.push(' ');
    num(d, p.y);
}

fn num(d: &mut String, v: f32) {
    // Paths are finite by construction (tiny-skia rejects non-finite), but be defensive.
    let v = if v.is_finite() { v } else { 0.0 };
    let r = (v as f64 * 100.0).round() / 100.0;
    let r = if r == 0.0 { 0.0 } else { r }; // no "-0"
    let mut s = String::new();
    let _ = write!(s, "{r:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    d.push_str(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(0.0, -0.001);
        pb.line_to(12.345, 100.0);
        pb.cubic_to(1.5, 2.25, 3.0, 4.0, 5.0, 6.0);
        pb.close();
        let d = path_to_d(&pb.finish().unwrap());
        assert_eq!(d, "M0 0L12.35 100C1.5 2.25 3 4 5 6Z");
    }
}
