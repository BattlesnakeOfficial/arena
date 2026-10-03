//! design_kit raster input (PNG/JPEG): sniffing, round trips, the template-aware ink rule,
//! lints, fixes, hostile inputs and output invariants.
//!
//! Catalog assets are rasterised with resvg (a dev-dependency) and compared against the
//! original with IoU on 400 px masks.

mod common;

use arena::design_kit::{
    AssetKind, CleanShape, Edge, Fix, InputFormat, Limits, Lint, ProcessError, RejectedFormat,
    Severity, process_upload, sniff,
};
use common::design_kit::*;
use proptest::prelude::*;

fn run(bytes: &[u8], fixes: &[Fix]) -> Result<CleanShape, ProcessError> {
    let r = process_upload(bytes, &Limits::default(), fixes);
    if let Ok(s) = &r {
        assert_clean(s);
    }
    r
}

fn head(slug: &str) -> String {
    catalog_svg(AssetKind::Head, slug)
}

fn tail(slug: &str) -> String {
    catalog_svg(AssetKind::Tail, slug)
}

/// A catalog asset drawn in black under an SVG transform.
fn transformed(kind: AssetKind, slug: &str, transform: &str) -> String {
    board_svg(
        &format!(
            "<g transform=\"{transform}\">{}</g>",
            catalog_inner(kind, slug)
        ),
        "#000000",
    )
}

const MIRROR: &str = "scale(-1,1) translate(-100,0)";
/// Clockwise quarter turn: the neck moves to the top and the head faces down.
const ROTATE_CW: &str = "rotate(90 50 50)";
/// 80% of full size, centred (Procreate canvas padding).
const SCALE_80: &str = "translate(10 10) scale(0.8)";

// ---------------------------------------------------------------------------------------
// 1. Sniffing
// ---------------------------------------------------------------------------------------

#[test]
fn sniffing_gives_exact_variants() {
    use InputFormat::*;
    use ProcessError::*;
    use RejectedFormat::*;
    let png = rgba_png(64, |x, _| x < 50.0);
    let jpeg = encode_jpeg(8, 8, &[0; 8 * 8 * 3], 80);
    let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M0 0H50V100H0Z"/></svg>"#;
    let cases: Vec<(&str, Vec<u8>, Result<InputFormat, ProcessError>)> = vec![
        ("png", png, Ok(Png)),
        ("jpeg", jpeg, Ok(Jpeg)),
        ("svg", svg.to_vec(), Ok(Svg)),
        (
            "heic",
            b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec(),
            Err(UnsupportedFormat(Heic)),
        ),
        (
            "gif",
            b"GIF89a\x01\0\x01\0\0\0\0".to_vec(),
            Err(UnsupportedFormat(Gif)),
        ),
        (
            "webp",
            b"RIFF\x24\0\0\0WEBPVP8 \x18\0\0\0".to_vec(),
            Err(UnsupportedFormat(Webp)),
        ),
        (
            "psd",
            b"8BPS\0\x01\0\0\0\0\0\0".to_vec(),
            Err(UnsupportedFormat(Psd)),
        ),
        (
            "zip",
            b"PK\x03\x04\x14\0\0\0".to_vec(),
            Err(UnsupportedFormat(Zip)),
        ),
        (
            "pdf",
            b"%PDF-1.7\n%\xe2\xe3".to_vec(),
            Err(UnsupportedFormat(Pdf)),
        ),
        (
            "avif",
            b"\0\0\0\x1cftypavif\0\0\0\0avifmif1".to_vec(),
            Err(UnsupportedFormat(Heic)),
        ),
        (
            "mp4 video",
            b"\0\0\0\x18ftypisom\0\0\x02\0isom".to_vec(),
            Err(UnknownFormat),
        ),
        ("empty", Vec::new(), Err(EmptyFile)),
        (
            "garbage",
            b"definitely not an image".to_vec(),
            Err(UnknownFormat),
        ),
    ];
    for (name, bytes, want) in cases {
        assert_eq!(sniff(&bytes), want, "{name}");
        // The pipeline reports sniffing errors unchanged.
        if let Err(e) = want {
            assert_eq!(run(&bytes, &[]).err(), Some(e), "{name}");
        }
    }

    // SVG is recognised but not processed until PR 2.
    let e = run(svg, &[]).expect_err("svg is not supported yet");
    assert_eq!(e, NotYetSupported(Svg));
    assert_eq!(e.code(), "not_yet_supported");
    assert!(!e.is_internal());

    // App-specific advice.
    let advice = |r: RejectedFormat| UnsupportedFormat(r).user_message();
    assert!(advice(Psd).contains("template") && advice(Psd).contains("PNG"));
    assert!(advice(Zip).contains("Actions → Share → PNG"));
    assert!(advice(Pdf).contains("Export → SVG or PNG"));
    assert!(advice(Heic).contains("JPEG or PNG"));
    assert_eq!(UnsupportedFormat(Psd).code(), "unsupported_format");
}

// ---------------------------------------------------------------------------------------
// 2. Round trips
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Export {
    /// Transparent background (Procreate with the background layer hidden).
    Alpha,
    /// Opaque, black on white.
    Opaque,
    /// Black on white, JPEG quality 80.
    Jpeg80,
    /// Transparent, with wobble, blur, grain and specks.
    Rough,
}

fn export(svg: &str, side: u32, how: Export) -> Vec<u8> {
    let rgba = straight_rgba(&render(svg, side));
    match how {
        Export::Alpha => encode_png(side, side, png::ColorType::Rgba, &rgba),
        Export::Opaque => encode_png(side, side, png::ColorType::Rgb, &over_white(&rgba)),
        Export::Jpeg80 => encode_jpeg(side as u16, side as u16, &over_white(&rgba), 80),
        Export::Rough => encode_png(
            side,
            side,
            png::ColorType::Rgba,
            &roughen(&rgba, side as usize),
        ),
    }
}

/// Make a clean render look hand-made: wobbly edges (±0.3 units), a soft blur, alpha
/// grain, and specks/pinholes smaller than 1 unit².
fn roughen(rgba: &[u8], side: usize) -> Vec<u8> {
    let px_per_unit = side as f32 / 100.0;
    let src: Vec<f32> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3] as f32)
        .collect();
    let at = |x: f32, y: f32| {
        let xi = (x.round() as isize).clamp(0, side as isize - 1) as usize;
        let yi = (y.round() as isize).clamp(0, side as isize - 1) as usize;
        src[yi * side + xi]
    };
    let amp = 0.3 * px_per_unit;
    let tau = std::f32::consts::TAU;
    let mut a = vec![0f32; side * side];
    for y in 0..side {
        for x in 0..side {
            let (xf, yf) = (x as f32, y as f32);
            let dx = amp * (tau * yf / (9.7 * px_per_unit) + 0.3).sin();
            let dy = amp * (tau * xf / (13.1 * px_per_unit) + 1.1).sin();
            a[y * side + x] = at(xf + dx, yf + dy);
        }
    }
    // Two passes of a 3x3 box blur.
    for _ in 0..2 {
        let prev = a.clone();
        for y in 0..side {
            for x in 0..side {
                let mut sum = 0.0;
                for (dx, dy) in (-1isize..=1).flat_map(|dx| (-1isize..=1).map(move |dy| (dx, dy))) {
                    let xi = (x as isize + dx).clamp(0, side as isize - 1) as usize;
                    let yi = (y as isize + dy).clamp(0, side as isize - 1) as usize;
                    sum += prev[yi * side + xi];
                }
                a[y * side + x] = sum / 9.0;
            }
        }
    }
    let mut rng = Rng(0x9e37_79b9);
    for v in &mut a {
        *v = (*v + (rng.unit() - 0.5) * 80.0).clamp(0.0, 255.0);
    }
    // Specks outside and pinholes inside, radius <= 0.3 units.
    for _ in 0..300 {
        let (cx, cy) = (rng.unit() * side as f32, rng.unit() * side as f32);
        let r = (0.15 + rng.unit() * 0.15) * px_per_unit;
        let centre = (cy as usize).min(side - 1) * side + (cx as usize).min(side - 1);
        let fill = if a[centre] >= 128.0 { 0.0 } else { 255.0 };
        let (x0, x1) = (
            (cx - r).floor().max(0.0) as usize,
            ((cx + r).ceil() as usize).min(side),
        );
        let (y0, y1) = (
            (cy - r).floor().max(0.0) as usize,
            ((cy + r).ceil() as usize).min(side),
        );
        for y in y0..y1 {
            for x in x0..x1 {
                if (x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2) <= r * r {
                    a[y * side + x] = fill;
                }
            }
        }
    }
    a.iter().flat_map(|&v| [0, 0, 0, v.round() as u8]).collect()
}

/// Vendored samples made only of straight edges and sharp corners. Exported cleanly,
/// they must come back with every corner where it was.
const STRAIGHT_EDGED: [(AssetKind, &str); 5] = [
    (AssetKind::Head, "pixel"),
    (AssetKind::Tail, "default"),
    (AssetKind::Tail, "bolt"),
    (AssetKind::Tail, "block-bum"),
    (AssetKind::Tail, "pixel"),
];

fn check_round_trip(kind: AssetKind, slug: &str, side: u32, how: Export) -> CleanShape {
    let svg = catalog_svg(kind, slug);
    let label = format!("{}/{slug} @{side} {how:?}", kind_dir(kind));
    let shape = run(&export(&svg, side, how), &[]).unwrap_or_else(|e| panic!("{label}: {e:?}"));
    let clean = matches!(how, Export::Alpha | Export::Opaque);
    let max_error = if clean && STRAIGHT_EDGED.contains(&(kind, slug)) {
        // One pixel of the comparison: a right angle rounded off with a radius over
        // about 1 unit fails.
        0.25
    } else {
        MAX_OUTLINE_ERROR
    };
    check_faithful(&label, &svg, &shape, max_error);
    assert!(
        shape.metrics().left_edge_pct >= 95.0,
        "{label}: left edge {:?}",
        shape.metrics()
    );
    assert!(
        shape.passes(kind),
        "{label}: official assets pass every check, got {:?}",
        shape.lints()
    );
    assert!(
        !shape.info().contains(&Lint::GuidesVisible),
        "{label}: {:?}",
        shape.info()
    );
    shape
}

/// How far (units) a trace's outline may stray from the drawing's. A curve cut by a
/// chord, a bulged flat front or a rounded-off corner shows up here long before it moves
/// IoU below 0.97: the 45° splice threshold cut 2.75 to 8 units off vendored samples
/// that still scored up to 0.995.
const MAX_OUTLINE_ERROR: f64 = 1.25;

/// The trace matches `reference` (an SVG of the intended shape) on 400 px masks: IoU at
/// least 0.97, and no point where they differ more than `max_error` units from the
/// reference's outline.
fn check_faithful(label: &str, reference: &str, shape: &CleanShape, max_error: f64) {
    let (want, got) = (alpha(reference, 400), shape_alpha(shape, 400));
    let score = iou(&want, &got);
    let error = outline_error(&want, &got);
    println!(
        "{label}: IoU {score:.4}, outline error {error:.2}, left edge {:.1}%, d {} B",
        shape.metrics().left_edge_pct,
        shape.path_d().len()
    );
    assert!(score >= 0.97, "{label}: IoU {score:.4}");
    assert!(
        error <= max_error,
        "{label}: outline off by {error:.2} units (at most {max_error})"
    );
}

/// Every vendored sample of `kind`, transparent and opaque, exported at `side` px.
fn every_sample_round_trips_at(kind: AssetKind, side: u32) {
    let slugs = match kind {
        AssetKind::Head => HEADS,
        AssetKind::Tail => TAILS,
    };
    for slug in slugs {
        for how in [Export::Alpha, Export::Opaque] {
            check_round_trip(kind, slug, side, how);
        }
    }
}

// One test per kind and size, so they run in parallel.

#[test]
fn every_sample_round_trips_at_512() {
    every_sample_round_trips_at(AssetKind::Head, 512);
    every_sample_round_trips_at(AssetKind::Tail, 512);
}

/// The template's own size.
#[test]
fn every_head_round_trips_at_1000() {
    every_sample_round_trips_at(AssetKind::Head, 1000);
}

#[test]
fn every_tail_round_trips_at_1000() {
    every_sample_round_trips_at(AssetKind::Tail, 1000);
}

#[test]
fn every_head_round_trips_at_1024() {
    every_sample_round_trips_at(AssetKind::Head, 1024);
}

#[test]
fn every_tail_round_trips_at_1024() {
    every_sample_round_trips_at(AssetKind::Tail, 1024);
}

/// The largest accepted size, traced on a box-downscaled 1024 px grid.
#[test]
fn every_head_round_trips_at_2048() {
    every_sample_round_trips_at(AssetKind::Head, 2048);
}

#[test]
fn every_tail_round_trips_at_2048() {
    every_sample_round_trips_at(AssetKind::Tail, 2048);
}

#[test]
fn the_trace_does_not_depend_on_the_export_size() {
    // Where the tracer splits an outline into cubics depends on the exact pixel grid.
    // With a 45° splice threshold one cubic spanned beluga's straight top and the turn
    // after it, and cut a wedge about 8 units deep off it at 512, 700, 1000 and 1024 px
    // (IoU 0.949 to 0.958) but not at 1023 or 1025.
    let svg = board_svg(&catalog_inner(AssetKind::Head, "beluga"), "#1e3a8a");
    for side in [512, 700, 1000, 1023, 1024, 1025] {
        let rgba = straight_rgba(&render(&svg, side));
        let png = encode_png(side, side, png::ColorType::Rgb, &over_white(&rgba));
        check_coloured(
            &format!("navy beluga @{side} opaque"),
            &png,
            &head("beluga"),
            AssetKind::Head,
        );
    }
}

#[test]
fn roughened_drawings_round_trip() {
    for (kind, slug) in [
        (AssetKind::Head, "smile"),
        (AssetKind::Head, "pixel"),
        (AssetKind::Tail, "round-bum"),
    ] {
        let shape = check_round_trip(kind, slug, 1024, Export::Rough);
        assert!(
            shape
                .info()
                .iter()
                .any(|l| matches!(l, Lint::SpecksRemoved { count } if *count > 0)),
            "{slug}: {:?}",
            shape.info()
        );
    }
}

#[test]
fn jpeg_q80_round_trips() {
    for (kind, slug) in [
        (AssetKind::Head, "beluga"),
        (AssetKind::Head, "sand-worm"),
        (AssetKind::Tail, "bolt"),
    ] {
        let shape = check_round_trip(kind, slug, 1024, Export::Jpeg80);
        assert_eq!(shape.input(), InputFormat::Jpeg);
        assert!(
            !shape.info().contains(&Lint::ColoursFlattened),
            "{:?}",
            shape.info()
        );
    }
}

#[test]
fn navy_and_crimson_drawings_are_ink() {
    // Each is within 24 of a guide colour multiplied over the reference ghost, so they
    // would vanish if those products were excluded without a visible ghost.
    for fill in ["#144682", "#961946"] {
        let svg = board_svg(&catalog_inner(AssetKind::Head, "default"), fill);
        let rgba = straight_rgba(&render(&svg, 512));
        let exports = [
            (
                "transparent",
                encode_png(512, 512, png::ColorType::Rgba, &rgba),
            ),
            (
                "opaque",
                encode_png(512, 512, png::ColorType::Rgb, &over_white(&rgba)),
            ),
        ];
        for (how, png) in exports {
            let s = run(&png, &[]).unwrap_or_else(|e| panic!("{fill} {how}: {e:?}"));
            let score = iou(&alpha(&head("default"), 400), &shape_alpha(&s, 400));
            assert!(score >= 0.97, "{fill} {how}: IoU {score:.4}");
            assert_eq!(s.info(), [Lint::ColoursFlattened], "{fill} {how}");
            assert!(s.passes(AssetKind::Head), "{fill} {how}: {:?}", s.lints());
        }
    }
}

/// `svg` composited over an opaque `background`, then softened with `blur` passes of a
/// 3x3 box blur (a 1 px soft edge per pass, like a slightly soft brush), as an RGB PNG.
fn soft_opaque(svg: &str, side: u32, background: [u8; 3], blur: usize) -> Vec<u8> {
    let rgba = straight_rgba(&render(svg, side));
    let mut rgb: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = p[3] as u32;
            let c = |v: u8, bg: u8| ((v as u32 * a + bg as u32 * (255 - a) + 127) / 255) as u8;
            [
                c(p[0], background[0]),
                c(p[1], background[1]),
                c(p[2], background[2]),
            ]
        })
        .collect();
    let side = side as usize;
    for _ in 0..blur {
        // Separable: rows, then columns.
        for (stride, step) in [(side * 3, 3), (3, side * 3)] {
            let prev = rgb.clone();
            for line in 0..side {
                for i in 0..side {
                    for ch in 0..3 {
                        let at = |j: usize| prev[line * stride + j * step + ch] as u32;
                        let (lo, hi) = (i.saturating_sub(1), (i + 1).min(side - 1));
                        let sum = at(lo) + at(i) + at(hi);
                        rgb[line * stride + i * step + ch] = ((sum + 1) / 3) as u8;
                    }
                }
            }
        }
    }
    encode_png(side as u32, side as u32, png::ColorType::Rgb, &rgb)
}

/// Upload `png` and check it against `reference` (an SVG of the intended shape): a
/// faithful trace, no warnings, and no template guides or reference reported. (A soft
/// edge of a blue close to the template's labels can also leave specks: the blend
/// exclusion around that guide colour catches part of the edge.)
fn check_coloured(label: &str, png: &[u8], reference: &str, kind: AssetKind) {
    let s = run(png, &[]).unwrap_or_else(|e| panic!("{label}: {e:?}"));
    check_faithful(label, reference, &s, MAX_OUTLINE_ERROR);
    assert!(
        s.info().first() == Some(&Lint::ColoursFlattened)
            && s.info()[1..]
                .iter()
                .all(|l| matches!(l, Lint::SpecksRemoved { .. })),
        "{label}: {:?}",
        s.info()
    );
    assert!(s.passes(kind), "{label}: {:?}", s.lints());
}

const WHITE_BG: [u8; 3] = [255, 255, 255];

#[test]
fn soft_edged_navy_and_crimson_drawings_are_ink() {
    // The template's 1000 px with an ordinary 1 px soft edge. Where navy blends into
    // white it passes within 11 of the reference ghost's colour, about 0.1% of the
    // canvas for these heads. That used to count as a visible reference, which turns on
    // the exclusion of guide-over-ghost products: dark blues and crimson. The drawing
    // then vanished, with "We only found the template guides".
    for slug in HEADS {
        for (fill, side) in [("#283c82", 1000), ("#144682", 512)] {
            let svg = board_svg(&catalog_inner(AssetKind::Head, slug), fill);
            check_coloured(
                &format!("{slug} {fill} soft @{side}"),
                &soft_opaque(&svg, side, WHITE_BG, 1),
                &head(slug),
                AssetKind::Head,
            );
        }
    }
    for (slug, fill, blur) in [
        ("default", "#283c82", 2),
        ("smile", "#1e3a8a", 2),
        ("beluga", "#961946", 2),
    ] {
        let svg = board_svg(&catalog_inner(AssetKind::Head, slug), fill);
        check_coloured(
            &format!("{slug} {fill} blur {blur}"),
            &soft_opaque(&svg, 1000, WHITE_BG, blur),
            &head(slug),
            AssetKind::Head,
        );
    }
}

#[test]
fn navy_and_crimson_fills_inside_a_soft_black_outline_are_ink() {
    // A black outline's soft edge is grey, and greys from about 190 to 213 are within
    // 24 of the ghost. They used to switch on the product exclusion, so the fill was
    // dropped and the head came back as an outline ("fill your shape").
    let inner = catalog_inner(AssetKind::Head, "smile");
    let outlined = |fill: &str| {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" width=\"100\" \
             height=\"100\" fill=\"{fill}\" stroke=\"#000\" stroke-width=\"1.5\">{inner}</svg>"
        )
    };
    let reference = outlined("#000");
    for fill in ["#144682", "#961946", "#1e3a8a"] {
        check_coloured(
            &format!("smile {fill} in a black outline"),
            &soft_opaque(&outlined(fill), 1000, WHITE_BG, 1),
            &reference,
            AssetKind::Head,
        );
    }
}

#[test]
fn navy_and_crimson_drawings_on_light_grey_are_ink() {
    // A light-grey background (#cccccc is 13 from the ghost) is not a reference.
    for background in [[0xcc; 3], [0xd0; 3], [0xc8; 3]] {
        for fill in ["#144682", "#961946"] {
            let svg = board_svg(&catalog_inner(AssetKind::Head, "smile"), fill);
            check_coloured(
                &format!("smile {fill} on {background:?}"),
                &soft_opaque(&svg, 1000, background, 0),
                &head("smile"),
                AssetKind::Head,
            );
        }
    }
    // White with a small grey drop shadow (0.2% of the canvas).
    let svg = board_svg(
        &format!(
            "<rect x=\"60\" y=\"90\" width=\"10\" height=\"2\" fill=\"#cdcdcd\"/>{}",
            catalog_inner(AssetKind::Head, "smile")
        ),
        "#144682",
    );
    let s = run(&soft_opaque(&svg, 1000, WHITE_BG, 0), &[]).expect("smile with a shadow");
    let score = iou(&alpha(&head("smile"), 400), &shape_alpha(&s, 400));
    assert!(score >= 0.97, "IoU {score:.4}");
    assert_eq!(s.info(), [Lint::ColoursFlattened]);
}

/// An opaque JPEG of `svg` with an EXIF APP1 segment giving `orientation`.
fn jpeg_with_orientation(svg: &str, side: u32, orientation: u16) -> Vec<u8> {
    let rgb = over_white(&straight_rgba(&render(svg, side)));
    // TIFF, little-endian: IFD0 at 8 with one entry, tag 0x0112 (SHORT, count 1).
    let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&[0; 6]); // value padding, then "no next IFD"
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
    enc.add_app_segment(1, exif).expect("APP1");
    enc.encode(&rgb, side as u16, side as u16, jpeg_encoder::ColorType::Rgb)
        .expect("jpeg encode");
    out
}

#[test]
fn exif_orientation_is_applied() {
    // An iPad camera stores the pixels turned and says "rotate 90° clockwise to view"
    // (orientation 6). The browser shows the artist an upright head; so must we.
    let stored = transformed(AssetKind::Head, "default", "rotate(-90 50 50)");
    let s = run(&jpeg_with_orientation(&stored, 512, 6), &[]).expect("orientation 6");
    let score = iou(&alpha(&head("default"), 400), &shape_alpha(&s, 400));
    assert!(score >= 0.97, "IoU {score:.4}");
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints());
    // Without the tag the same pixels are a head facing up.
    let plain = run(&jpeg_with_orientation(&stored, 512, 1), &[]).expect("orientation 1");
    assert_eq!(codes(&plain.lints().head), ["faces_up_down"]);
    // Orientation 2 is a mirror: the stored mirror image reads as the original.
    let mirrored = transformed(AssetKind::Head, "default", MIRROR);
    let s = run(&jpeg_with_orientation(&mirrored, 512, 2), &[]).expect("orientation 2");
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints());
}

#[test]
fn progressive_jpegs_have_a_smaller_size_cap() {
    // Progressive JPEGs hold every coefficient until the last scan, so colour ones are
    // capped at max_side / sqrt(2). Scaled down here to keep the test fast.
    let limits = Limits {
        max_raster_side: 512,
        ..Limits::default()
    };
    // libjpeg's standard progressive script (PIL, mozjpeg, most apps) starts with one DC
    // scan carrying every component, so only the progressive check catches it; the
    // multi-scan one doesn't. jpeg-encoder writes one component per scan, so its
    // progressive files below would be caught by either. The fixture is a plain head
    // saved by PIL 10.4 (400 px, quality 80, progressive, 4:2:0).
    let libjpeg = fixture("jpeg/progressive-400.jpg");
    assert!(
        libjpeg.windows(2).any(|w| w == [0xff, 0xc2]),
        "SOF2: progressive"
    );
    assert_eq!(
        first_scan_components(&libjpeg),
        Some(3),
        "interleaved DC scan"
    );
    assert_eq!(
        process_upload(&libjpeg, &limits, &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 400,
            height: 400,
            max_side: 362
        })
    );
    let s = run(&libjpeg, &[]).expect("within the default cap");
    assert_eq!(s.input(), InputFormat::Jpeg);
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints());

    let encode = |side: u16, progressive: bool, colour: jpeg_encoder::ColorType| {
        let channels = if colour == jpeg_encoder::ColorType::Luma {
            1
        } else {
            3
        };
        let mut data = vec![255u8; side as usize * side as usize * channels];
        data[..side as usize * channels * 40].fill(0);
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 80);
        enc.set_progressive(progressive);
        enc.encode(&data, side, side, colour).expect("jpeg encode");
        out
    };
    let rgb = jpeg_encoder::ColorType::Rgb;
    assert_eq!(
        process_upload(&encode(400, true, rgb), &limits, &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 400,
            height: 400,
            max_side: 362
        })
    );
    // Baseline at the same size, and progressive within the cap or in greyscale, are fine.
    for (side, progressive, colour) in [
        (400, false, rgb),
        (360, true, rgb),
        (400, true, jpeg_encoder::ColorType::Luma),
    ] {
        let r = process_upload(&encode(side, progressive, colour), &limits, &[]);
        assert!(r.is_ok(), "{side} progressive={progressive}: {:?}", r.err());
    }
}

/// How many scans (SOS markers) a JPEG has. Entropy-coded data never contains 0xFF 0xDA
/// (a literal 0xFF is stuffed as 0xFF 0x00), so every match is a scan header.
fn scan_count(jpeg: &[u8]) -> usize {
    jpeg.windows(2).filter(|w| w == &[0xff, 0xda]).count()
}

/// `Ns` of a test JPEG's first scan header: how many components its first scan carries.
fn first_scan_components(jpeg: &[u8]) -> Option<u8> {
    let at = jpeg.windows(2).position(|w| w == [0xff, 0xda])?;
    jpeg.get(at + 4).copied()
}

#[test]
fn multi_scan_baseline_jpegs_have_the_progressive_size_cap() {
    // A baseline JPEG with one scan per component (what encoders write when they optimise
    // their Huffman tables) also makes zune-jpeg keep every coefficient until the last
    // scan: a 2048 px CMYK one peaked at 47 MB. It gets the same cap as a progressive one.
    let limits = Limits {
        max_raster_side: 512,
        ..Limits::default()
    };
    let encode = |side: u16, colour: jpeg_encoder::ColorType| {
        let channels = match colour {
            jpeg_encoder::ColorType::Luma => 1,
            jpeg_encoder::ColorType::Cmyk => 4,
            _ => 3,
        };
        let mut data = vec![255u8; side as usize * side as usize * channels];
        data[..side as usize * channels * 40].fill(0);
        if colour == jpeg_encoder::ColorType::Cmyk {
            // CMYK is ink, so 0 is white: ink the top rows, leave the rest white.
            for v in data.iter_mut() {
                *v = 255 - *v;
            }
        }
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        enc.set_optimized_huffman_tables(true);
        enc.encode(&data, side, side, colour).expect("jpeg encode");
        out
    };
    let rgb = jpeg_encoder::ColorType::Rgb;
    let cmyk = jpeg_encoder::ColorType::Cmyk;
    assert_eq!(scan_count(&encode(64, rgb)), 3, "one scan per component");
    assert_eq!(
        process_upload(&encode(400, rgb), &limits, &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 400,
            height: 400,
            max_side: 362
        })
    );
    assert_eq!(scan_count(&encode(64, cmyk)), 4, "one scan per component");
    assert_eq!(
        process_upload(&encode(400, cmyk), &limits, &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 400,
            height: 400,
            max_side: 313
        })
    );
    // Within the cap, and greyscale (one component, so its only scan has them all).
    for (side, colour) in [
        (360, rgb),
        (310, cmyk),
        (400, jpeg_encoder::ColorType::Luma),
    ] {
        let r = process_upload(&encode(side, colour), &limits, &[]);
        assert!(r.is_ok(), "{side} {colour:?}: {:?}", r.err());
    }
}

#[test]
fn a_jpeg_the_decoder_panics_on_is_invalid() {
    // zune-jpeg 0.5 panics on a CMYK JPEG with one scan per component and subsampled
    // colour, even a tiny one (a length assertion in its upsampler, which the NEON, AVX2
    // and scalar versions all make). That must not unwind out of process_upload.
    let side = 64u16;
    let mut data = vec![0u8; side as usize * side as usize * 4];
    data[..side as usize * 4 * 20].fill(255);
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, 80);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
    enc.set_optimized_huffman_tables(true);
    enc.encode(&data, side, side, jpeg_encoder::ColorType::Cmyk)
        .expect("jpeg encode");
    let r = run(&out, &[]);
    assert!(matches!(r, Err(ProcessError::InvalidImage(_))), "{r:?}");
}

#[test]
fn png_metadata_chunks_are_skipped() {
    // An ICC profile that inflates to 8 MiB and 3 MiB of plain text: the decoder's own
    // allocations are capped at 4 MiB, so reading these chunks would either inflate a
    // profile we never use or fail the upload. They are skipped instead.
    let rgba = straight_rgba(&render(&head("default"), 256));
    let mut info = png::Info::with_size(256, 256);
    info.color_type = png::ColorType::Rgba;
    info.bit_depth = png::BitDepth::Eight;
    info.icc_profile = Some(std::borrow::Cow::Owned(vec![0; 8 << 20]));
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::with_info(&mut out, info).expect("png info");
        enc.add_text_chunk("Comment".into(), "a".repeat(3 << 20))
            .expect("tEXt");
        enc.write_header()
            .expect("png header")
            .write_image_data(&rgba)
            .expect("png data");
    }
    assert!(out.len() < 4 << 20, "{} bytes", out.len());
    let s = run(&out, &[]).expect("PNG with big metadata");
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints());
}

// ---------------------------------------------------------------------------------------
// 3. Ink matrix: the template's layers in every visibility combination
// ---------------------------------------------------------------------------------------

/// Premultiplied RGBA in 0..1.
type Px = [f32; 4];

fn premul_layer(rgba_straight: &[u8]) -> Vec<Px> {
    rgba_straight
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let a = p[3] as f32 / 255.0;
            [
                p[0] as f32 / 255.0 * a,
                p[1] as f32 / 255.0 * a,
                p[2] as f32 / 255.0 * a,
                a,
            ]
        })
        .collect()
}

fn decode_rgba_png(bytes: &[u8]) -> (u32, Vec<u8>) {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().expect("png info");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.width, info.height);
    (info.width, buf)
}

/// Normal ("source over") compositing.
fn over(dst: &mut [Px], src: &[Px]) {
    for (d, s) in dst.iter_mut().zip(src) {
        for c in 0..4 {
            d[c] = s[c] + d[c] * (1.0 - s[3]);
        }
    }
}

/// Multiply blend (separable, W3C compositing): co = cs(1-ab) + cb(1-as) + cs*cb.
fn multiply(dst: &mut [Px], src: &[Px]) {
    for (d, s) in dst.iter_mut().zip(src) {
        let (ab, a_s) = (d[3], s[3]);
        for c in 0..3 {
            d[c] = s[c] * (1.0 - ab) + d[c] * (1.0 - a_s) + s[c] * d[c];
        }
        d[3] = a_s + ab - a_s * ab;
    }
}

/// Export the stack as a straight RGBA PNG, the way Procreate's "Share → PNG" does.
fn export_stack(stack: &[Px], side: u32) -> Vec<u8> {
    let data: Vec<u8> = stack
        .iter()
        .flat_map(|p| {
            let a = p[3];
            let ch = |v: f32| {
                if a <= 0.0 {
                    0
                } else {
                    (v / a * 255.0).round().clamp(0.0, 255.0) as u8
                }
            };
            [ch(p[0]), ch(p[1]), ch(p[2]), (a * 255.0).round() as u8]
        })
        .collect();
    encode_png(side, side, png::ColorType::Rgba, &data)
}

/// Draw `drawing` (black) in the template for `kind`, over the reference ghost of
/// `reference`, toggling the Background, Guides and Reference layers.
fn ink_matrix(kind: AssetKind, drawing: &str, reference: &str, guide_png: &str) {
    let (side, guides_rgba) = decode_rgba_png(&fixture(guide_png));
    assert_eq!(side, 1000, "the template is 1000x1000");
    let draw = premul_layer(&straight_rgba(&render(&catalog_svg(kind, drawing), side)));
    // The generator bakes the ghost colour into the reference pixels.
    let ghost_svg = board_svg(&catalog_inner(kind, reference), "#c8c2d4");
    let ghost = premul_layer(&straight_rgba(&render(&ghost_svg, side)));
    let guides = premul_layer(&guides_rgba);
    let white = vec![[1.0f32; 4]; (side * side) as usize];

    let clean = run(&export_stack(&draw, side), &[]).expect("clean drawing");
    let original = alpha(&catalog_svg(kind, drawing), 400);
    let clean_alpha = shape_alpha(&clean, 400);
    assert!(iou(&original, &clean_alpha) >= 0.97);

    let mut failures = Vec::new();
    for background in [true, false] {
        for show_guides in [true, false] {
            for show_reference in [true, false] {
                // Bottom to top: Background, Reference, Draw here, Guides (Multiply).
                let mut stack = if background {
                    white.clone()
                } else {
                    vec![[0.0; 4]; white.len()]
                };
                if show_reference {
                    over(&mut stack, &ghost);
                }
                over(&mut stack, &draw);
                if show_guides {
                    multiply(&mut stack, &guides);
                }
                let label = format!(
                    "{}/{drawing}: background={background} guides={show_guides} \
                     reference={show_reference}",
                    kind_dir(kind)
                );
                let shape = match run(&export_stack(&stack, side), &[]) {
                    Ok(s) => s,
                    Err(e) => {
                        failures.push(format!("{label}: {e:?}"));
                        continue;
                    }
                };
                let score = iou(&clean_alpha, &shape_alpha(&shape, 400));
                println!("{label}: IoU vs clean {score:.4}, info {:?}", shape.info());
                if score < 0.99 {
                    failures.push(format!("{label}: IoU vs clean {score:.4}"));
                }
                // guides_visible fires exactly when they are visible, and nothing else
                // about the template leaks into the info lints.
                let want = if show_guides || show_reference {
                    vec![Lint::GuidesVisible]
                } else {
                    vec![]
                };
                if shape.info() != want {
                    failures.push(format!("{label}: info {:?}", shape.info()));
                }
                if !shape.passes(kind) {
                    failures.push(format!("{label}: {:?}", shape.lints()));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn ink_rule_ignores_head_template_layers() {
    ink_matrix(
        AssetKind::Head,
        "beluga",
        "default",
        "template/head-guide.png",
    );
}

#[test]
fn ink_rule_ignores_tail_template_layers() {
    ink_matrix(
        AssetKind::Tail,
        "curled",
        "default",
        "template/tail-guide.png",
    );
}

#[test]
fn guides_alone_are_empty_with_a_hint() {
    let guides = fixture("template/head-guide.png");
    let e = run(&guides, &[]).expect_err("guides alone are not a drawing");
    // Where the pink attach label meets a blue guide line (around x 35, y 500 of the
    // overlay), the anti-aliased purple between them is no template colour. Those few
    // pixels are cleaned up as specks; a drawing covers that spot anyway (it's inside
    // the neck), so the ink matrix above sees exactly [guides_visible].
    assert_eq!(
        e,
        ProcessError::Empty {
            info: vec![Lint::GuidesVisible, Lint::SpecksRemoved { count: 3 }]
        }
    );
    assert!(e.user_message().contains("guides"), "{}", e.user_message());
}

// ---------------------------------------------------------------------------------------
// 4. Lints
// ---------------------------------------------------------------------------------------

fn lint_case(name: &str, png: &[u8], want_head: &[&str], want_tail: &[&str]) -> CleanShape {
    let shape = run(png, &[]).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(
        (codes(&shape.lints().head), codes(&shape.lints().tail)),
        (want_head.to_vec(), want_tail.to_vec()),
        "{name}: metrics {:?}",
        shape.metrics()
    );
    for l in shape.lints().head.iter().chain(&shape.lints().tail) {
        assert_eq!(l.severity(), Severity::Warn, "{name}: {l:?}");
        assert!(l.guide_anchor().starts_with('#'));
        assert!(!l.message().is_empty());
    }
    shape
}

#[test]
fn round_blob_has_a_neck_gap() {
    let blob = |x: f32, y: f32| (x - 50.0).powi(2) + (y - 50.0).powi(2) <= 50.0f32.powi(2);
    let s = lint_case(
        "round blob",
        &rgba_png(512, blob),
        &["neck_gap"],
        &["neck_gap"],
    );
    assert!(!s.passes(AssetKind::Head) && !s.passes(AssetKind::Tail));
    // The gap brackets cover the top and bottom of the left edge.
    let gaps = &s.metrics().left_edge_gaps;
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    assert!(gaps[0][0] == 0.0 && gaps[1][1] == 100.0, "{gaps:?}");
}

/// `inner` in black on white, with `cut` (SVG markup) painted white over it.
fn cut_png(inner: &str, cut: &str) -> Vec<u8> {
    let svg = board_svg(&format!("{inner}<g fill=\"#fff\">{cut}</g>"), "#000");
    export(&svg, 512, Export::Opaque)
}

/// A 20% notch in the neck: x < 10, y 40–60.
const NOTCH: &str = "<rect x=\"-1\" y=\"40\" width=\"11\" height=\"20\"/>";

#[test]
fn a_neck_gap_is_not_mistaken_for_a_turn_when_the_top_and_bottom_are_flat() {
    // These heads have a full top and bottom as well as the neck, so a notched neck
    // (left side 80%) used to read as "the full-height side is the top": "This looks
    // rotated", with the neck gap left out. A rotated head's front is opposite its neck,
    // and no head's front is more than 47.5% full.
    let gap_only = |label: &str, png: &[u8], want_head: &[&str]| {
        let s = lint_case(label, png, want_head, &["neck_gap"]);
        for l in s.lints().head.iter().chain(&s.lints().tail) {
            assert_eq!(l.fix(), None, "{label}: {l:?}");
        }
    };
    for slug in ["default", "pixel", "sand-worm"] {
        gap_only(
            &format!("notched {slug} head"),
            &cut_png(&catalog_inner(AssetKind::Head, slug), NOTCH),
            &["neck_gap"],
        );
    }
    // The eye drawn a little big, so it breaks the neck edge (left side 83%).
    gap_only(
        "default head with a big eye",
        &cut_png(
            &catalog_inner(AssetKind::Head, "default"),
            "<circle cx=\"8\" cy=\"28.55\" r=\"11\"/>",
        ),
        &["neck_gap"],
    );
    // Tails: a notched block (full on every other side) was offered Flip, which moves the
    // notch to the tip; a flat fishtail was told to rotate.
    gap_only(
        "notched block-bum tail",
        &cut_png(&catalog_inner(AssetKind::Tail, "block-bum"), NOTCH),
        &["solid_square", "neck_gap"],
    );
    gap_only(
        "notched fishtail",
        &cut_png("<path d=\"M0 0H100V30L70 50L100 70V100H0Z\"/>", NOTCH),
        &["neck_gap"],
    );
}

#[test]
fn mirrored_head_faces_left_and_offers_flip() {
    let s = lint_case(
        "mirrored head",
        &svg_to_png(&transformed(AssetKind::Head, "default", MIRROR), 512),
        &["faces_left"],
        &["tail_reversed"],
    );
    assert_eq!(s.lints().head[0].fix(), Some(Fix::Flip));
    assert_eq!(
        s.lints().tail[0],
        Lint::TailReversed {
            attach_edge: Edge::Right
        }
    );
}

#[test]
fn rotated_heads_face_up_or_down() {
    lint_case(
        "rotated default head",
        &svg_to_png(&transformed(AssetKind::Head, "default", ROTATE_CW), 512),
        &["faces_up_down"],
        &[],
    );
    // smile's right edge is open, so only the neck on the top edge gives it away.
    let s = lint_case(
        "rotated smile head",
        &svg_to_png(&transformed(AssetKind::Head, "smile", ROTATE_CW), 512),
        &["faces_up_down"],
        &["tail_reversed"],
    );
    assert_eq!(s.lints().head[0].fix(), None);
    assert_eq!(
        s.lints().tail[0],
        Lint::TailReversed {
            attach_edge: Edge::Top
        }
    );
    assert_eq!(s.lints().tail[0].fix(), None, "Flip can't fix a rotation");
}

#[test]
fn a_quarter_turn_is_judged_by_the_neck_edge_not_the_centre_of_mass() {
    // beluga's mass sits above centre (y 46.7), so a clockwise quarter turn moves it
    // right of x = 52, which alone would read as a mirrored head. The full-height side
    // is the top: it's rotated, and Flip can't fix that.
    let s = lint_case(
        "rotated beluga",
        &svg_to_png(&transformed(AssetKind::Head, "beluga", ROTATE_CW), 512),
        &["faces_up_down"],
        &["tail_reversed"],
    );
    let [cx, _] = s.metrics().centroid.expect("centroid");
    assert!(cx > 52.0, "centroid x {cx}");
    assert_eq!(s.lints().head[0].fix(), None);
    assert_eq!(
        s.lints().tail[0],
        Lint::TailReversed {
            attach_edge: Edge::Top
        }
    );
}

#[test]
fn a_mirror_is_judged_by_the_neck_edge_not_the_centre_of_mass() {
    // guitar's centre of mass is at x 48.5, so mirrored it is 51.5: not past 52. The
    // full-height right side gives the mirror away, and Flip fixes it.
    let png = svg_to_png(&transformed(AssetKind::Head, "guitar", MIRROR), 512);
    let s = lint_case("mirrored guitar", &png, &["faces_left"], &["tail_reversed"]);
    let [cx, _] = s.metrics().centroid.expect("centroid");
    assert!(cx < 52.0, "centroid x {cx}");
    assert_eq!(s.lints().head[0].fix(), Some(Fix::Flip));
    let flipped = run(&png, &[Fix::Flip]).expect("flipped");
    assert!(flipped.passes(AssetKind::Head), "{:?}", flipped.lints());
}

#[test]
fn a_short_drawing_is_not_offered_a_fit_that_does_nothing() {
    // The default head squashed to 80% height: the neck is on the left, it's just short.
    // It already spans the width, so Fit (which keeps the aspect ratio) couldn't make it
    // taller; the margin warning asks for a taller drawing and the neck gap is reported.
    let s = lint_case(
        "short head",
        &svg_to_png(
            &transformed(AssetKind::Head, "default", "scale(1 0.8)"),
            512,
        ),
        &["neck_gap", "margins"],
        &["neck_gap", "margins"],
    );
    let margins = &s.lints().head[1];
    assert_eq!(margins.fix(), None);
    assert!(
        margins.message().contains("taller"),
        "{}",
        margins.message()
    );
}

#[test]
fn a_wide_drawing_is_fitted_once() {
    // A padded head twice as wide as it is tall. Fit helps (it moves it to the neck edge
    // and makes it bigger) but can only reach half the height, so it doesn't hide the
    // neck gap, and afterwards it isn't offered again.
    let png = svg_to_png(
        &transformed(
            AssetKind::Head,
            "default",
            "translate(10 30) scale(0.8 0.4)",
        ),
        512,
    );
    let s = lint_case(
        "wide padded head",
        &png,
        &["neck_gap", "margins"],
        &["neck_gap", "margins"],
    );
    assert_eq!(s.lints().head[1].fix(), Some(Fix::Fit));
    let fitted = run(&png, &[Fix::Fit]).expect("fitted");
    assert_eq!(codes(&fitted.lints().head), ["neck_gap", "margins"]);
    assert_eq!(
        fitted.lints().head[1].fix(),
        None,
        "Fit again would change nothing"
    );
    let b = fitted.metrics().bbox.expect("bbox");
    assert!(
        b[0] < 0.6 && b[2] > 99.4 && (b[1] - 25.0).abs() < 1.0 && (b[3] - 75.0).abs() < 1.0,
        "{b:?}"
    );
    assert!(
        (fitted.metrics().left_edge_pct - 50.0).abs() < 2.0,
        "{:?}",
        fitted.metrics()
    );
}

#[test]
fn fixes_combine_in_one_request() {
    // A mirrored head, narrower than it is tall, with Procreate-style padding needs both
    // Flip and Fit. The studio sends every fix tapped so far, in tap order; they apply
    // Flip first whatever the order, so Fit anchors the flipped neck on the left edge.
    // The drawing must be taller than it is wide: when Fit fills the width, both orders
    // give the same shape.
    let narrow = format!("translate(20 10) scale(0.6 0.8) {MIRROR}");
    let png = svg_to_png(&transformed(AssetKind::Head, "default", &narrow), 512);
    lint_case(
        "mirrored, padded, narrow head",
        &png,
        &["margins", "faces_left"],
        &["margins", "tail_reversed"],
    );
    // The intended result: the head, full height, 75% wide, neck on the left edge.
    let intended = alpha(
        &transformed(AssetKind::Head, "default", "scale(0.75 1)"),
        400,
    );
    for fixes in [
        [Fix::Flip, Fix::Fit],
        // Fit tapped first (margins is listed first).
        [Fix::Fit, Fix::Flip],
    ] {
        let s = run(&png, &fixes).unwrap_or_else(|e| panic!("{fixes:?}: {e:?}"));
        assert!(s.passes(AssetKind::Head), "{fixes:?}: {:?}", s.lints());
        let b = s.metrics().bbox.expect("bbox");
        assert!(
            b[0] < 0.6 && (b[2] - 75.0).abs() < 1.0,
            "{fixes:?}: bbox {b:?}"
        );
        assert!(
            s.metrics().left_edge_pct >= 95.0,
            "{fixes:?}: {:?}",
            s.metrics()
        );
        let score = iou(&intended, &shape_alpha(&s, 400));
        assert!(score >= 0.97, "{fixes:?}: IoU {score:.4}");
    }
    let both = run(&png, &[Fix::Flip, Fix::Fit]).expect("flip + fit");
    let reordered = run(&png, &[Fix::Fit, Fix::Flip, Fix::Fit]).expect("fit + flip + fit");
    assert_eq!(both.path_d(), reordered.path_d());
    // Fitting first would leave the neck 25 units off the left edge, so the order shows.
    let fitted = run(&png, &[Fix::Fit]).expect("fit");
    assert_eq!(codes(&fitted.lints().head), ["faces_left"]);
    let fit_then_flip = shape_alpha_with(&fitted, MIRROR);
    let score = iou(&intended, &fit_then_flip);
    assert!(score < 0.8, "Fit then Flip IoU {score:.4}");
    // One at a time, each leaves the other problem.
    let flipped = run(&png, &[Fix::Flip]).expect("flip");
    assert_eq!(codes(&flipped.lints().head), ["margins"]);
}

#[test]
fn reversed_tail_offers_flip() {
    let s = lint_case(
        "reversed tail",
        &svg_to_png(&transformed(AssetKind::Tail, "default", MIRROR), 512),
        &["faces_left"],
        &["tail_reversed"],
    );
    assert_eq!(s.lints().tail[0].fix(), Some(Fix::Flip));
}

#[test]
fn outline_only_drawing() {
    let inner = catalog_inner(AssetKind::Head, "default");
    let outline = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" width=\"100\" \
         height=\"100\" fill=\"none\" stroke=\"#000\" stroke-width=\"6\">{inner}</svg>"
    );
    let s = lint_case(
        "outline",
        &svg_to_png(&outline, 512),
        &["outline_only"],
        &["outline_only"],
    );
    assert!(s.metrics().hole_pct > 55.0, "{:?}", s.metrics());
}

#[test]
fn margins_suppress_the_neck_gap_they_explain() {
    let s = lint_case(
        "80% head",
        &svg_to_png(&transformed(AssetKind::Head, "default", SCALE_80), 512),
        &["margins"],
        &["margins"],
    );
    assert!(s.metrics().left_edge_pct < 85.0, "the gap is there");
    assert_eq!(s.lints().head[0].fix(), Some(Fix::Fit));
}

#[test]
fn nearly_empty_dot() {
    // A small dot: Fit would not close its neck gap, so both warnings stay.
    let dot = |x: f32, y: f32| (x - 50.0).powi(2) + (y - 50.0).powi(2) <= 10.0f32.powi(2);
    lint_case(
        "near-empty",
        &rgba_png(512, dot),
        &["nearly_empty", "neck_gap", "margins"],
        &["nearly_empty", "neck_gap", "margins"],
    );
}

#[test]
fn solid_square_is_only_wrong_for_heads() {
    lint_case(
        "solid square",
        &rgba_png(512, |_, _| true),
        &["solid_square"],
        &[],
    );
}

#[test]
fn official_shapes_pass() {
    let s = run(&svg_to_png(&head("smile"), 512), &[]).expect("smile");
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints());
    let s = run(&svg_to_png(&tail("block-bum"), 512), &[]).expect("block-bum");
    assert!(s.passes(AssetKind::Tail), "{:?}", s.lints());
    assert!(!s.passes(AssetKind::Head), "a solid square is not a head");
}

#[test]
fn input_facts() {
    // Non-square canvas: centred, with a tip.
    let (w, h) = (600u32, 300u32);
    let mut data = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            data.extend_from_slice(if x < 300 {
                &[0, 0, 0, 255]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    let s = run(&encode_png(w, h, png::ColorType::Rgba, &data), &[]).expect("non-square");
    assert_eq!(
        s.info(),
        vec![Lint::NonSquare {
            width: 600,
            height: 300
        }]
    );
    assert_eq!(s.info()[0].severity(), Severity::Tip);
    let b = s.metrics().bbox.expect("bbox");
    assert!(
        b[0] < 0.5 && (b[1] - 25.0).abs() < 0.6 && (b[2] - 50.0).abs() < 0.6,
        "{b:?}"
    );

    // Tiny image: low_resolution tip.
    let s = run(&svg_to_png(&head("default"), 96), &[]).expect("tiny");
    assert!(
        s.info().contains(&Lint::LowResolution {
            width: 96,
            height: 96
        }),
        "{:?}",
        s.info()
    );

    // Coloured drawing on white: flattened to one colour.
    let red = board_svg(&catalog_inner(AssetKind::Head, "default"), "#c0102a");
    let rgba = straight_rgba(&render(&red, 512));
    let s = run(
        &encode_png(512, 512, png::ColorType::Rgb, &over_white(&rgba)),
        &[],
    )
    .expect("red head");
    assert!(s.info().contains(&Lint::ColoursFlattened), "{:?}", s.info());
    assert_eq!(s.info()[0].severity(), Severity::Info);

    // White eye painted over a transparent-background head: the eye becomes a hole.
    let white_eye = board_svg(
        "<path d=\"M0 0H100V100H0Z\"/><circle cx=\"30\" cy=\"35\" r=\"12\" fill=\"#fff\"/>",
        "#000",
    );
    let mut rgba = straight_rgba(&render(&white_eye, 512));
    // Make the right third transparent so alpha decides.
    for y in 0..512usize {
        for x in 400..512usize {
            rgba[(y * 512 + x) * 4 + 3] = 0;
        }
    }
    let s = run(&encode_png(512, 512, png::ColorType::Rgba, &rgba), &[]).expect("eye");
    assert!(s.info().contains(&Lint::ColoursFlattened), "{:?}", s.info());
    assert_eq!(s.metrics().holes, 1, "{:?}", s.metrics());

    // Soft brush: semi-transparent ink.
    let soft = rgba_png(512, |x, y| x < 70.0 && y > 5.0 && y < 95.0);
    let mut dec = png::Decoder::new(std::io::Cursor::new(&soft[..]));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().expect("info");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("frame");
    for p in buf.as_chunks_mut::<4>().0.iter_mut() {
        if p[3] == 255 {
            p[3] = 200;
        }
    }
    let s = run(&encode_png(512, 512, png::ColorType::Rgba, &buf), &[]).expect("soft");
    assert!(s.info().contains(&Lint::SemiTransparent), "{:?}", s.info());
}

// ---------------------------------------------------------------------------------------
// 5. Fixes
// ---------------------------------------------------------------------------------------

/// Render `shape` with an extra SVG transform applied (e.g. to undo a fix).
fn shape_alpha_with(shape: &CleanShape, transform: &str) -> Vec<u8> {
    alpha(
        &format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><g \
             transform=\"{transform}\"><path fill-rule=\"{}\" d=\"{}\"/></g></svg>",
            shape.fill_rule().as_svg(),
            shape.path_d()
        ),
        400,
    )
}

#[test]
fn flip_fixes_a_mirrored_head() {
    let png = svg_to_png(&transformed(AssetKind::Head, "default", MIRROR), 512);
    let before = run(&png, &[]).expect("mirrored");
    let after = run(&png, &[Fix::Flip]).expect("flipped");
    assert_eq!(codes(&before.lints().head), ["faces_left"]);
    assert!(after.passes(AssetKind::Head), "{:?}", after.lints().head);
    // Flipping back gives the unfixed shape; and the fix gives the original head.
    let undone = shape_alpha_with(&after, MIRROR);
    let score = iou(&shape_alpha(&before, 400), &undone);
    assert!(score >= 0.99, "inverse flip IoU {score:.4}");
    let score = iou(&alpha(&head("default"), 400), &shape_alpha(&after, 400));
    assert!(score >= 0.97, "flipped vs original IoU {score:.4}");
}

#[test]
fn fit_fixes_a_padded_head() {
    let png = svg_to_png(&transformed(AssetKind::Head, "default", SCALE_80), 512);
    let before = run(&png, &[]).expect("80%");
    let after = run(&png, &[Fix::Fit]).expect("fitted");
    assert_eq!(codes(&before.lints().head), ["margins"]);
    assert!(after.passes(AssetKind::Head), "{:?}", after.lints().head);
    assert!(after.passes(AssetKind::Tail), "{:?}", after.lints().tail);
    assert!(
        after.metrics().left_edge_pct >= 95.0,
        "{:?}",
        after.metrics()
    );
    // Undo the fit using the unfixed bounds; compare with the unfixed shape.
    let [x0, y0, x1, y1] = before.metrics().bbox.expect("bbox");
    let s = (x1 - x0).max(y1 - y0) / 100.0;
    let undo = format!("translate({x0} {y0}) scale({s})");
    let score = iou(&shape_alpha(&before, 400), &shape_alpha_with(&after, &undo));
    assert!(score >= 0.97, "inverse fit IoU {score:.4}");
    let score = iou(&alpha(&head("default"), 400), &shape_alpha(&after, 400));
    assert!(score >= 0.97, "fitted vs original IoU {score:.4}");
}

// ---------------------------------------------------------------------------------------
// 6. Hostile rasters
// ---------------------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// A PNG whose IHDR claims `w`x`h`, with a tiny IDAT and IEND.
fn png_with_header(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let idat: &[u8] = &[0x78, 0x9c, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01];
    for (ty, body) in [(&b"IHDR"[..], &ihdr[4..]), (b"IDAT", idat), (b"IEND", &[])] {
        let mut chunk = ty.to_vec();
        chunk.extend_from_slice(body);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    }
    out
}

fn checkerboard(side: u32, square: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let ink = ((x / square) + (y / square)).is_multiple_of(2);
            data.extend_from_slice(&[0, 0, 0, if ink { 255 } else { 0 }]);
        }
    }
    encode_png(side, side, png::ColorType::Rgba, &data)
}

#[test]
fn hostile_rasters_fail_exactly() {
    // A 100000x100000 header is rejected from IHDR alone, without allocating.
    let t = std::time::Instant::now();
    assert_eq!(
        run(&png_with_header(100_000, 100_000), &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 100_000,
            height: 100_000,
            max_side: 2048
        })
    );
    assert!(t.elapsed().as_millis() < 200, "{:?}", t.elapsed());

    // Just over the side limit.
    assert!(matches!(
        run(&png_with_header(2049, 10), &[]),
        Err(ProcessError::ImageTooLarge { width: 2049, .. })
    ));

    // A JPEG header claiming 4000x3000 (SOF dimensions patched).
    let mut jpeg = encode_jpeg(16, 16, &[0; 16 * 16 * 3], 80);
    let sof = jpeg
        .windows(2)
        .position(|w| w[0] == 0xff && (w[1] == 0xc0 || w[1] == 0xc2))
        .expect("SOF marker");
    jpeg[sof + 5..sof + 7].copy_from_slice(&3000u16.to_be_bytes());
    jpeg[sof + 7..sof + 9].copy_from_slice(&4000u16.to_be_bytes());
    assert_eq!(
        run(&jpeg, &[]).err(),
        Some(ProcessError::ImageTooLarge {
            width: 4000,
            height: 3000,
            max_side: 2048
        })
    );

    // Bigger than the byte cap.
    let mut big = rgba_png(64, |x, _| x < 50.0);
    big.resize(4 * 1024 * 1024 + 1, 0);
    assert_eq!(
        run(&big, &[]).err(),
        Some(ProcessError::TooLarge {
            bytes: 4 * 1024 * 1024 + 1,
            max: 4 * 1024 * 1024
        })
    );

    // Truncated and zero-size images.
    let good = svg_to_png(&head("default"), 256);
    assert!(
        matches!(
            run(&good[..good.len() / 2], &[]),
            Err(ProcessError::InvalidImage(_))
        ),
        "{:?}",
        run(&good[..good.len() / 2], &[]).err()
    );
    assert!(
        matches!(
            run(&png_with_header(0, 100), &[]),
            Err(ProcessError::InvalidImage(_))
        ),
        "{:?}",
        run(&png_with_header(0, 100), &[]).err()
    );

    // A truncated JPEG is an error, not a drawing with grey filler rows.
    let jpeg = encode_jpeg(
        512,
        512,
        &over_white(&straight_rgba(&render(&head("smile"), 512))),
        80,
    );
    for cut in [jpeg.len() / 5, jpeg.len() / 2, jpeg.len() - 10] {
        assert!(
            matches!(run(&jpeg[..cut], &[]), Err(ProcessError::InvalidImage(_))),
            "cut at {cut}: {:?}",
            run(&jpeg[..cut], &[]).err()
        );
    }

    // Nothing drawn.
    assert_eq!(
        run(&rgba_png(256, |_, _| false), &[]).err(),
        Some(ProcessError::Empty { info: vec![] })
    );
    let white = encode_png(256, 256, png::ColorType::Rgb, &[255; 256 * 256 * 3]);
    assert_eq!(
        run(&white, &[]).err(),
        Some(ProcessError::Empty { info: vec![] })
    );
}

#[test]
fn noise_is_too_complex() {
    let side = 1024u32;
    let mut rng = Rng(12345);
    let mut data = Vec::with_capacity((side * side * 4) as usize);
    for _ in 0..side * side {
        data.extend_from_slice(&[0, 0, 0, if rng.next().is_multiple_of(2) { 255 } else { 0 }]);
    }
    let r = run(&encode_png(side, side, png::ColorType::Rgba, &data), &[]);
    assert!(
        matches!(r, Err(ProcessError::TooComplex(_))),
        "{:?}",
        r.err()
    );
}

#[test]
fn checkerboards_are_too_complex() {
    // 12 px squares: too many separate shapes. 24 px: the outline is too long.
    for square in [12, 24] {
        let r = run(&checkerboard(1024, square), &[]);
        let e = r.expect_err("checkerboard");
        assert!(
            matches!(e, ProcessError::TooComplex(_)),
            "{square}px: {e:?}"
        );
        assert_eq!(e.code(), "too_complex");
    }
}

/// A 1024 px greyscale PNG, black where `ink(x, y)`.
fn grey_png(ink: impl Fn(usize, usize) -> bool) -> Vec<u8> {
    let n = 1024;
    let mut data = Vec::with_capacity(n * n);
    for y in 0..n {
        for x in 0..n {
            data.push(if ink(x, y) { 0 } else { 255 });
        }
    }
    encode_png(n as u32, n as u32, png::ColorType::Grayscale, &data)
}

#[test]
fn crafted_rasters_are_rejected_before_tracing() {
    // Each of these is a few KB. Before the trace budget, the stripes and rings took 1-2
    // s of CPU in release and still came back `ok`, and the comb and the serpentine
    // panicked inside visioncortex (a u16 cluster index overflow; a "STUCK" outline walk
    // past 1,000,000 steps). The budget's message proves they were stopped before
    // visioncortex ran.
    let c = 511.5f64;
    let ring = |x: usize, y: usize| (x as f64 - c).abs().max((y as f64 - c).abs()) as usize;
    let diamond = |x: usize, y: usize| ((x as f64 - c).abs() + (y as f64 - c).abs()) as usize;
    let cases: [(&str, Vec<u8>); 6] = [
        (
            "diagonal stripes, period 4",
            grey_png(|x, y| (x + y) % 4 < 2),
        ),
        (
            "diagonal stripes, period 8",
            grey_png(|x, y| (x + y) % 8 < 4),
        ),
        ("square rings", grey_png(|x, y| ring(x, y) % 4 < 2)),
        ("diamond rings", grey_png(|x, y| diamond(x, y) % 4 < 2)),
        (
            "comb",
            grey_png(|x, y| (y % 3 == 1 && x % 2 == 0) || y % 3 == 2),
        ),
        (
            "1 px serpentine",
            grey_png(|x, y| y % 2 == 0 || (y % 4 == 1 && x == 1023) || (y % 4 == 3 && x == 0)),
        ),
    ];
    for (name, png) in cases {
        assert_eq!(
            run(&png, &[]).err(),
            Some(ProcessError::TooComplex("the outline is too long")),
            "{name}"
        );
    }

    // A short enough outline in huge bounding boxes: six thin concentric rings near the
    // canvas edge (each ring and the hole inside it span most of the canvas, about 11×
    // the canvas in total). The tracer rescans every box.
    let target = grey_png(|x, y| {
        let from_edge = 511usize.saturating_sub(ring(x, y));
        from_edge / 12 < 6 && from_edge % 12 < 4
    });
    assert_eq!(
        run(&target, &[]).err(),
        Some(ProcessError::TooComplex(
            "too many large or interleaved shapes"
        ))
    );
}

#[test]
fn errors_have_codes_and_messages() {
    let all = [
        ProcessError::EmptyFile,
        ProcessError::UnknownFormat,
        ProcessError::UnsupportedFormat(RejectedFormat::Gif),
        ProcessError::NotYetSupported(InputFormat::Svg),
        ProcessError::TooLarge { bytes: 9, max: 8 },
        ProcessError::ImageTooLarge {
            width: 4096,
            height: 10,
            max_side: 2048,
        },
        ProcessError::InvalidImage("x".into()),
        ProcessError::TooComplex("x"),
        ProcessError::Empty { info: vec![] },
        ProcessError::Internal("x"),
    ];
    let mut seen = std::collections::HashSet::new();
    for e in &all {
        assert!(seen.insert(e.code()), "duplicate code {}", e.code());
        assert!(!e.user_message().is_empty());
        assert_eq!(e.is_internal(), e.code() == "internal");
    }
    let msg = all[5].user_message();
    assert!(msg.contains("2048") && msg.contains("1000 × 1000"), "{msg}");
    // Only black is promised to work: dark colours near the template's are dropped.
    let msg = all[8].user_message();
    assert!(
        msg.contains("solid black") && !msg.contains("any dark"),
        "{msg}"
    );
}

// ---------------------------------------------------------------------------------------
// 7. Output invariants, property-style
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Blob {
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    ellipse: bool,
    hole: bool,
}

fn blob() -> impl Strategy<Value = Blob> {
    (
        0f32..100.0,
        0f32..100.0,
        2f32..50.0,
        2f32..50.0,
        any::<bool>(),
        prop::bool::weighted(0.25),
    )
        .prop_map(|(cx, cy, rx, ry, ellipse, hole)| Blob {
            cx,
            cy,
            rx,
            ry,
            ellipse,
            hole,
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn every_output_is_one_clean_path(
        blobs in prop::collection::vec(blob(), 1..7),
        opaque in any::<bool>(),
        fixes in prop_oneof![
            Just(vec![]),
            Just(vec![Fix::Flip]),
            Just(vec![Fix::Fit]),
            Just(vec![Fix::Fit, Fix::Flip]),
        ],
    ) {
        let inside = |b: &Blob, x: f32, y: f32| {
            let (dx, dy) = ((x - b.cx) / b.rx, (y - b.cy) / b.ry);
            if b.ellipse { dx * dx + dy * dy <= 1.0 } else { dx.abs() <= 1.0 && dy.abs() <= 1.0 }
        };
        let f = |x: f32, y: f32| {
            let mut ink = false;
            for b in &blobs {
                if inside(b, x, y) {
                    ink = !b.hole;
                }
            }
            ink
        };
        let png = if opaque {
            let side = 256u32;
            let mut rgb = Vec::with_capacity((side * side * 3) as usize);
            for y in 0..side {
                for x in 0..side {
                    let (u, v) = ((x as f32 + 0.5) / 2.56, (y as f32 + 0.5) / 2.56);
                    rgb.extend_from_slice(if f(u, v) { &[20, 20, 20] } else { &[245, 245, 245] });
                }
            }
            encode_png(side, side, png::ColorType::Rgb, &rgb)
        } else {
            rgba_png(256, f)
        };
        match process_upload(&png, &Limits::default(), &fixes) {
            Ok(shape) => {
                assert_clean(&shape);
                let svg = shape.to_svg();
                prop_assert!(svg.starts_with("<svg"));
                prop_assert!(!shape.path_d().is_empty());
            }
            Err(e) => prop_assert!(
                matches!(e, ProcessError::Empty { .. } | ProcessError::TooComplex(_)),
                "{e:?}"
            ),
        }
    }
}
