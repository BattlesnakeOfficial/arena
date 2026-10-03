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

fn run(bytes: &[u8], fix: Option<Fix>) -> Result<CleanShape, ProcessError> {
    let r = process_upload(bytes, &Limits::default(), fix);
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
            assert_eq!(run(&bytes, None).err(), Some(e), "{name}");
        }
    }

    // SVG is recognised but not processed until PR 2.
    let e = run(svg, None).expect_err("svg is not supported yet");
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

fn check_round_trip(kind: AssetKind, slug: &str, side: u32, how: Export) -> CleanShape {
    let svg = catalog_svg(kind, slug);
    let label = format!("{}/{slug} @{side} {how:?}", kind_dir(kind));
    let shape = run(&export(&svg, side, how), None).unwrap_or_else(|e| panic!("{label}: {e:?}"));
    let score = iou(&alpha(&svg, 400), &shape_alpha(&shape, 400));
    println!(
        "{label}: IoU {score:.4}, left edge {:.1}%, d {} B",
        shape.metrics.left_edge_pct,
        shape.path_d.len()
    );
    assert!(score >= 0.97, "{label}: IoU {score:.4}");
    assert!(
        shape.metrics.left_edge_pct >= 95.0,
        "{label}: left edge {:?}",
        shape.metrics
    );
    assert!(
        shape.passes(kind),
        "{label}: official assets pass every check, got {:?}",
        shape.lints
    );
    assert!(
        !shape.info.contains(&Lint::GuidesVisible),
        "{label}: {:?}",
        shape.info
    );
    shape
}

#[test]
fn every_sample_round_trips_at_1024_with_transparency() {
    for slug in HEADS {
        check_round_trip(AssetKind::Head, slug, 1024, Export::Alpha);
    }
    for slug in TAILS {
        check_round_trip(AssetKind::Tail, slug, 1024, Export::Alpha);
    }
}

#[test]
fn round_trips_at_512_and_opaque() {
    for (kind, slug) in [
        (AssetKind::Head, "default"),
        (AssetKind::Head, "bendr"),
        (AssetKind::Tail, "curled"),
        (AssetKind::Tail, "pixel"),
    ] {
        for how in [Export::Alpha, Export::Opaque] {
            check_round_trip(kind, slug, 512, how);
        }
        check_round_trip(kind, slug, 1024, Export::Opaque);
    }
}

#[test]
fn round_trips_at_2048() {
    for (kind, slug) in [(AssetKind::Head, "smile"), (AssetKind::Tail, "curled")] {
        for how in [Export::Alpha, Export::Opaque] {
            check_round_trip(kind, slug, 2048, how);
        }
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
                .info
                .iter()
                .any(|l| matches!(l, Lint::SpecksRemoved { count } if *count > 0)),
            "{slug}: {:?}",
            shape.info
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
        assert_eq!(shape.input, InputFormat::Jpeg);
        assert!(
            !shape.info.contains(&Lint::ColoursFlattened),
            "{:?}",
            shape.info
        );
    }
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

    let clean = run(&export_stack(&draw, side), None).expect("clean drawing");
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
                let shape = match run(&export_stack(&stack, side), None) {
                    Ok(s) => s,
                    Err(e) => {
                        failures.push(format!("{label}: {e:?}"));
                        continue;
                    }
                };
                let score = iou(&clean_alpha, &shape_alpha(&shape, 400));
                println!("{label}: IoU vs clean {score:.4}, info {:?}", shape.info);
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
                if shape.info != want {
                    failures.push(format!("{label}: info {:?}", shape.info));
                }
                if !shape.passes(kind) {
                    failures.push(format!("{label}: {:?}", shape.lints));
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
    let e = run(&guides, None).expect_err("guides alone are not a drawing");
    assert_eq!(
        e,
        ProcessError::Empty {
            info: vec![Lint::GuidesVisible]
        }
    );
    assert!(e.user_message().contains("guides"), "{}", e.user_message());
}

// ---------------------------------------------------------------------------------------
// 4. Lints
// ---------------------------------------------------------------------------------------

fn lint_case(name: &str, png: &[u8], want_head: &[&str], want_tail: &[&str]) -> CleanShape {
    let shape = run(png, None).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(
        (codes(&shape.lints.head), codes(&shape.lints.tail)),
        (want_head.to_vec(), want_tail.to_vec()),
        "{name}: metrics {:?}",
        shape.metrics
    );
    for l in shape.lints.head.iter().chain(&shape.lints.tail) {
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
    let gaps = &s.metrics.left_edge_gaps;
    assert_eq!(gaps.len(), 2, "{gaps:?}");
    assert!(gaps[0][0] == 0.0 && gaps[1][1] == 100.0, "{gaps:?}");
}

#[test]
fn mirrored_head_faces_left_and_offers_flip() {
    let s = lint_case(
        "mirrored head",
        &svg_to_png(&transformed(AssetKind::Head, "default", MIRROR), 512),
        &["faces_left"],
        &["tail_reversed"],
    );
    assert_eq!(s.lints.head[0].fix(), Some(Fix::Flip));
    assert_eq!(
        s.lints.tail[0],
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
    assert_eq!(s.lints.head[0].fix(), None);
    assert_eq!(
        s.lints.tail[0],
        Lint::TailReversed {
            attach_edge: Edge::Top
        }
    );
    assert_eq!(s.lints.tail[0].fix(), None, "Flip can't fix a rotation");
}

#[test]
fn reversed_tail_offers_flip() {
    let s = lint_case(
        "reversed tail",
        &svg_to_png(&transformed(AssetKind::Tail, "default", MIRROR), 512),
        &["faces_left"],
        &["tail_reversed"],
    );
    assert_eq!(s.lints.tail[0].fix(), Some(Fix::Flip));
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
    assert!(s.metrics.hole_pct > 55.0, "{:?}", s.metrics);
}

#[test]
fn margins_suppress_the_neck_gap_they_explain() {
    let s = lint_case(
        "80% head",
        &svg_to_png(&transformed(AssetKind::Head, "default", SCALE_80), 512),
        &["margins"],
        &["margins"],
    );
    assert!(s.metrics.left_edge_pct < 85.0, "the gap is there");
    assert_eq!(s.lints.head[0].fix(), Some(Fix::Fit));
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
    let s = run(&svg_to_png(&head("smile"), 512), None).expect("smile");
    assert!(s.passes(AssetKind::Head), "{:?}", s.lints);
    let s = run(&svg_to_png(&tail("block-bum"), 512), None).expect("block-bum");
    assert!(s.passes(AssetKind::Tail), "{:?}", s.lints);
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
    let s = run(&encode_png(w, h, png::ColorType::Rgba, &data), None).expect("non-square");
    assert_eq!(
        s.info,
        vec![Lint::NonSquare {
            width: 600,
            height: 300
        }]
    );
    assert_eq!(s.info[0].severity(), Severity::Tip);
    let b = s.metrics.bbox.expect("bbox");
    assert!(
        b[0] < 0.5 && (b[1] - 25.0).abs() < 0.6 && (b[2] - 50.0).abs() < 0.6,
        "{b:?}"
    );

    // Tiny image: low_resolution tip.
    let s = run(&svg_to_png(&head("default"), 96), None).expect("tiny");
    assert!(
        s.info.contains(&Lint::LowResolution {
            width: 96,
            height: 96
        }),
        "{:?}",
        s.info
    );

    // Coloured drawing on white: flattened to one colour.
    let red = board_svg(&catalog_inner(AssetKind::Head, "default"), "#c0102a");
    let rgba = straight_rgba(&render(&red, 512));
    let s = run(
        &encode_png(512, 512, png::ColorType::Rgb, &over_white(&rgba)),
        None,
    )
    .expect("red head");
    assert!(s.info.contains(&Lint::ColoursFlattened), "{:?}", s.info);
    assert_eq!(s.info[0].severity(), Severity::Info);

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
    let s = run(&encode_png(512, 512, png::ColorType::Rgba, &rgba), None).expect("eye");
    assert!(s.info.contains(&Lint::ColoursFlattened), "{:?}", s.info);
    assert_eq!(s.metrics.holes, 1, "{:?}", s.metrics);

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
    let s = run(&encode_png(512, 512, png::ColorType::Rgba, &buf), None).expect("soft");
    assert!(s.info.contains(&Lint::SemiTransparent), "{:?}", s.info);
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
            shape.fill_rule.as_svg(),
            shape.path_d
        ),
        400,
    )
}

#[test]
fn flip_fixes_a_mirrored_head() {
    let png = svg_to_png(&transformed(AssetKind::Head, "default", MIRROR), 512);
    let before = run(&png, None).expect("mirrored");
    let after = run(&png, Some(Fix::Flip)).expect("flipped");
    assert_eq!(codes(&before.lints.head), ["faces_left"]);
    assert!(after.passes(AssetKind::Head), "{:?}", after.lints.head);
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
    let before = run(&png, None).expect("80%");
    let after = run(&png, Some(Fix::Fit)).expect("fitted");
    assert_eq!(codes(&before.lints.head), ["margins"]);
    assert!(after.passes(AssetKind::Head), "{:?}", after.lints.head);
    assert!(after.passes(AssetKind::Tail), "{:?}", after.lints.tail);
    assert!(after.metrics.left_edge_pct >= 95.0, "{:?}", after.metrics);
    // Undo the fit using the unfixed bounds; compare with the unfixed shape.
    let [x0, y0, x1, y1] = before.metrics.bbox.expect("bbox");
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
        run(&png_with_header(100_000, 100_000), None).err(),
        Some(ProcessError::ImageTooLarge {
            width: 100_000,
            height: 100_000,
            max_side: 2048
        })
    );
    assert!(t.elapsed().as_millis() < 200, "{:?}", t.elapsed());

    // Just over the side limit.
    assert!(matches!(
        run(&png_with_header(2049, 10), None),
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
        run(&jpeg, None).err(),
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
        run(&big, None).err(),
        Some(ProcessError::TooLarge {
            bytes: 4 * 1024 * 1024 + 1,
            max: 4 * 1024 * 1024
        })
    );

    // Truncated and zero-size images.
    let good = svg_to_png(&head("default"), 256);
    assert!(
        matches!(
            run(&good[..good.len() / 2], None),
            Err(ProcessError::InvalidImage(_))
        ),
        "{:?}",
        run(&good[..good.len() / 2], None).err()
    );
    assert!(
        matches!(
            run(&png_with_header(0, 100), None),
            Err(ProcessError::InvalidImage(_))
        ),
        "{:?}",
        run(&png_with_header(0, 100), None).err()
    );

    // Nothing drawn.
    assert_eq!(
        run(&rgba_png(256, |_, _| false), None).err(),
        Some(ProcessError::Empty { info: vec![] })
    );
    let white = encode_png(256, 256, png::ColorType::Rgb, &[255; 256 * 256 * 3]);
    assert_eq!(
        run(&white, None).err(),
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
    let r = run(&encode_png(side, side, png::ColorType::Rgba, &data), None);
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
        let r = run(&checkerboard(1024, square), None);
        let e = r.expect_err("checkerboard");
        assert!(
            matches!(e, ProcessError::TooComplex(_)),
            "{square}px: {e:?}"
        );
        assert_eq!(e.code(), "too_complex");
    }
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
        fix in prop_oneof![Just(None), Just(Some(Fix::Flip)), Just(Some(Fix::Fit))],
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
        match process_upload(&png, &Limits::default(), fix) {
            Ok(shape) => {
                assert_clean(&shape);
                let svg = shape.to_svg();
                prop_assert!(svg.starts_with("<svg"));
                prop_assert!(!shape.path_d.is_empty());
            }
            Err(e) => prop_assert!(
                matches!(e, ProcessError::Empty { .. } | ProcessError::TooComplex(_)),
                "{e:?}"
            ),
        }
    }
}
