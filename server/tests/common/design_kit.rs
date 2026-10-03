//! Shared helpers for the `design_kit` integration tests.
//!
//! resvg is the independent oracle: it renders both the vendored catalog assets and our
//! output, and never runs in production code.

use arena::design_kit::{AssetKind, CleanShape, Lint};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

pub fn fixture_path(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/design_kit")
        .join(rel)
}

pub fn fixture(rel: &str) -> Vec<u8> {
    let p = fixture_path(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// The vendored catalog samples (single-colour, Standard group).
pub const HEADS: [&str; 6] = ["default", "smile", "beluga", "bendr", "pixel", "sand-worm"];
pub const TAILS: [&str; 6] = [
    "default",
    "round-bum",
    "curled",
    "bolt",
    "block-bum",
    "pixel",
];

pub fn kind_dir(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Head => "heads",
        AssetKind::Tail => "tails",
    }
}

/// Inner markup of a vendored catalog SVG (between the root tags).
pub fn catalog_inner(kind: AssetKind, slug: &str) -> String {
    let raw = fixture(&format!("catalog/{}/{slug}.svg", kind_dir(kind)));
    let text = String::from_utf8(raw).expect("catalog SVGs are UTF-8");
    let open = text.find("<svg").expect("has <svg");
    let body = open + text[open..].find('>').expect("root tag closes") + 1;
    let close = text.rfind("</svg>").expect("has </svg>");
    text[body..close].to_string()
}

/// The board's wrapper: what the game does with an asset, filled with `fill`.
pub fn board_svg(inner: &str, fill: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" width=\"100\" \
         height=\"100\" fill=\"{fill}\">{inner}</svg>"
    )
}

/// A catalog asset in solid black, as an artist would draw it.
pub fn catalog_svg(kind: AssetKind, slug: &str) -> String {
    board_svg(&catalog_inner(kind, slug), "#000000")
}

/// Render an SVG (fit and centred into `side`x`side`) with resvg. Premultiplied RGBA.
pub fn render(svg: &str, side: u32) -> Pixmap {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("valid test SVG");
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    let s = side as f32 / w.max(h);
    let ts = Transform::from_row(
        s,
        0.0,
        0.0,
        s,
        (side as f32 - w * s) / 2.0,
        (side as f32 - h * s) / 2.0,
    );
    let mut pm = Pixmap::new(side, side).expect("pixmap");
    resvg::render(&tree, ts, &mut pm.as_mut());
    pm
}

pub fn alpha(svg: &str, side: u32) -> Vec<u8> {
    render(svg, side)
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3])
        .collect()
}

/// Alpha of our output rendered by resvg.
pub fn shape_alpha(shape: &CleanShape, side: u32) -> Vec<u8> {
    alpha(&shape.to_svg(), side)
}

/// Intersection over union of two alpha masks (filled = alpha >= 128).
pub fn iou(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let (mut i, mut u) = (0usize, 0usize);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (*x >= 128, *y >= 128);
        i += usize::from(x && y);
        u += usize::from(x || y);
    }
    if u == 0 { 1.0 } else { i as f64 / u as f64 }
}

/// How far our outline strays from the reference's, in units, on two square alpha masks
/// (filled = alpha >= 128): the largest distance from a pixel where they differ to the
/// reference's outline (3-4 chamfer, within about 6% of Euclidean; ink on the canvas
/// edge counts as outline). A curve cut by a straight chord scores the thickness of the
/// sliver it cuts off; anti-aliasing differences score at most a pixel.
pub fn outline_error(reference: &[u8], got: &[u8]) -> f64 {
    assert_eq!(reference.len(), got.len());
    let side = reference.len().isqrt();
    assert_eq!(side * side, reference.len(), "square masks");
    let ink = |x: isize, y: isize| {
        x >= 0
            && y >= 0
            && x < side as isize
            && y < side as isize
            && reference[y as usize * side + x as usize] >= 128
    };
    const FAR: u32 = u32::MAX / 2;
    // Chamfer distance transform seeded at the reference's outline pixels.
    let mut d = vec![FAR; side * side];
    for y in 0..side as isize {
        for x in 0..side as isize {
            let here = ink(x, y);
            if [(-1, 0), (1, 0), (0, -1), (0, 1)]
                .iter()
                .any(|(dx, dy)| ink(x + dx, y + dy) != here)
            {
                d[y as usize * side + x as usize] = 0;
            }
        }
    }
    // Two raster passes, each taking the 4 neighbours already visited (+3 straight, +4
    // diagonal).
    let n = side as isize;
    let mut relax = |x: isize, y: isize, s: isize| {
        let at = |d: &[u32], x: isize, y: isize| {
            if (0..n).contains(&x) && (0..n).contains(&y) {
                d[(y * n + x) as usize]
            } else {
                FAR
            }
        };
        let best = [
            at(&d, x - s, y) + 3,
            at(&d, x - s, y - s) + 4,
            at(&d, x, y - s) + 3,
            at(&d, x + s, y - s) + 4,
        ]
        .into_iter()
        .min()
        .unwrap_or(FAR);
        let i = (y * n + x) as usize;
        d[i] = d[i].min(best);
    };
    for y in 0..n {
        for x in 0..n {
            relax(x, y, 1);
        }
    }
    for y in (0..n).rev() {
        for x in (0..n).rev() {
            relax(x, y, -1);
        }
    }
    let worst = reference
        .iter()
        .zip(got)
        .zip(&d)
        .filter(|((r, g), _)| (**r >= 128) != (**g >= 128))
        .map(|(_, &dist)| dist)
        .max()
        .unwrap_or(0);
    f64::from(worst) / 3.0 / (side as f64 / 100.0)
}

/// Output invariants: the exact template, one path, and a numeric `d`.
pub fn assert_clean(shape: &CleanShape) {
    let svg = shape.to_svg();
    let prefix =
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><path fill-rule=\"";
    assert!(svg.starts_with(prefix), "{svg}");
    assert!(svg.ends_with("\"/></svg>"), "{svg}");
    assert!(
        shape
            .path_d()
            .chars()
            .all(|c| matches!(c, 'M' | 'L' | 'Q' | 'C' | 'Z' | '0'..='9' | '.' | '-' | ' ')),
        "unexpected char in d: {}",
        shape.path_d()
    );
    assert_eq!(svg.matches('<').count(), 3, "{svg}");
    assert_eq!(svg.matches("<path").count(), 1, "{svg}");
    assert!(
        shape.path_d().starts_with('M'),
        "d must start with M: {}",
        shape.path_d()
    );
}

pub fn codes(lints: &[Lint]) -> Vec<&'static str> {
    lints.iter().map(Lint::code).collect()
}

pub fn encode_png(w: u32, h: u32, ct: png::ColorType, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(ct);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().expect("png header");
        wr.write_image_data(data).expect("png data");
    }
    out
}

/// Straight (non-premultiplied) RGBA bytes of a pixmap.
pub fn straight_rgba(pm: &Pixmap) -> Vec<u8> {
    pm.pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// RGBA PNG of an SVG rendered by resvg (transparent background).
pub fn svg_to_png(svg: &str, side: u32) -> Vec<u8> {
    let pm = render(svg, side);
    encode_png(side, side, png::ColorType::Rgba, &straight_rgba(&pm))
}

/// Opaque RGB composite of straight RGBA over white.
pub fn over_white(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = p[3] as u32;
            let c = |v: u8| ((v as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
            [c(p[0]), c(p[1]), c(p[2])]
        })
        .collect()
}

pub fn encode_jpeg(w: u16, h: u16, rgb: &[u8], quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, quality);
    enc.encode(rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .expect("jpeg encode");
    out
}

/// RGBA PNG from a coverage function f(x_units, y_units) -> bool (black ink on
/// transparent), sampled at pixel centres.
pub fn rgba_png(side: u32, f: impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let mut data = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let (u, v) = (
                (x as f32 + 0.5) * 100.0 / side as f32,
                (y as f32 + 0.5) * 100.0 / side as f32,
            );
            data.extend_from_slice(if f(u, v) {
                &[0, 0, 0, 255]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    encode_png(side, side, png::ColorType::Rgba, &data)
}

/// Ideal 400 px mask of a coverage function, for IoU against our output.
pub fn ideal(f: impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(400 * 400);
    for y in 0..400 {
        for x in 0..400 {
            v.push(if f((x as f32 + 0.5) / 4.0, (y as f32 + 0.5) / 4.0) {
                255
            } else {
                0
            });
        }
    }
    v
}

/// Deterministic xorshift for test noise.
pub struct Rng(pub u32);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// A def chain of `links` patterns, masks or pattern/`<use>` pairs, each wrapping its
/// content in `nest` groups (the critique's probe: under every per-element cap, but
/// nesting multiplies along the chain).
pub fn reference_chain(kind: &str, links: usize, nest: usize) -> String {
    let mut s = String::from(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         viewBox=\"0 0 100 100\"><defs>",
    );
    let open = "<g>".repeat(nest);
    let close = "</g>".repeat(nest);
    for i in 0..links {
        let next = |attr: &str| {
            if i + 1 < links {
                format!(" {attr}=\"url(#p{})\"", i + 1)
            } else {
                String::new()
            }
        };
        match kind {
            "pattern" => {
                let fill = if i + 1 < links {
                    format!("url(#p{})", i + 1)
                } else {
                    "black".into()
                };
                s += &format!(
                    "<pattern id=\"p{i}\" width=\"10\" height=\"10\" \
                     patternUnits=\"userSpaceOnUse\">{open}<rect width=\"5\" height=\"5\" \
                     fill=\"{fill}\"/>{close}</pattern>"
                );
            }
            "mask" => {
                s += &format!(
                    "<mask id=\"p{i}\">{open}<rect width=\"100\" height=\"100\" \
                     fill=\"white\"{}/>{close}</mask>",
                    next("mask")
                );
            }
            "clip" => {
                s += &format!(
                    "<clipPath id=\"p{i}\"{}>{open}<rect width=\"100\" height=\"100\"/>{close}\
                     </clipPath>",
                    next("clip-path")
                );
            }
            "patuse" => {
                let fill = if i + 1 < links {
                    format!("url(#p{})", i + 1)
                } else {
                    "black".into()
                };
                s += &format!(
                    "<g id=\"g{i}\">{open}<rect width=\"5\" height=\"5\" \
                     fill=\"{fill}\"/>{close}</g><pattern id=\"p{i}\" width=\"10\" height=\"10\" \
                     patternUnits=\"userSpaceOnUse\">{open}<use href=\"#g{i}\"/>{close}</pattern>"
                );
            }
            _ => unreachable!("{kind}"),
        }
    }
    let attr = match kind {
        "mask" => "mask",
        "clip" => "clip-path",
        _ => "fill",
    };
    s += &format!("</defs><rect width=\"100\" height=\"100\" {attr}=\"url(#p0)\"/></svg>");
    s
}
