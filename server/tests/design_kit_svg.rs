//! design_kit SVG input: real-world exports, the template filter, input-fact lints,
//! hostile files with exact outcomes, and the big-stack runner.
//!
//! Every SVG here is processed through `process_on_big_stack`, as production does: on a
//! 2 MiB test thread a hostile one could overflow the stack and abort the whole test
//! binary instead of failing.

mod common;

use std::time::{Duration, Instant};

use arena::design_kit::{
    AssetKind, CleanShape, FillRule, Fix, InputFormat, Limits, Lint, ProcessError, RejectedFormat,
    Severity, Strategy, process_on_big_stack, process_upload,
};
use common::design_kit::*;

fn run(svg: &str) -> Result<CleanShape, ProcessError> {
    run_bytes(svg.as_bytes(), &[])
}

fn run_bytes(bytes: &[u8], fixes: &[Fix]) -> Result<CleanShape, ProcessError> {
    let r = process_on_big_stack(bytes.to_vec(), &Limits::default(), fixes, ())
        .blocking_recv()
        .expect("the processing thread sends a result");
    if let Ok(s) = &r {
        assert_clean(s);
        assert_eq!(s.input(), InputFormat::Svg);
    }
    r
}

/// Run on the dedicated big-stack thread, as production does. Returns the time taken.
fn run_big(svg: &str) -> (Result<CleanShape, ProcessError>, Duration) {
    let t = Instant::now();
    let r = process_on_big_stack(svg.as_bytes().to_vec(), &Limits::default(), &[], ())
        .blocking_recv()
        .expect("the processing thread sends a result");
    let elapsed = t.elapsed();
    if let Ok(s) = &r {
        assert_clean(s);
    }
    (r, elapsed)
}

/// Is the clean shape filled at (x, y) in 0..100 units?
fn filled_at(shape: &CleanShape, x: f32, y: f32) -> bool {
    let a = shape_alpha(shape, 400);
    let (px, py) = ((x * 4.0) as usize, (y * 4.0) as usize);
    a[py * 400 + px] >= 128
}

fn info(shape: &CleanShape) -> Vec<&'static str> {
    codes(shape.info())
}

const SVG_OPEN: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">";

/// The box around every point of a clean path (`d` is absolute, numbers in pairs).
fn d_bounds(d: &str) -> [f32; 4] {
    let nums: Vec<f32> = d
        .split(|c: char| c.is_ascii_alphabetic() || c == ' ')
        .filter(|t| !t.is_empty())
        .map(|t| t.parse().expect("a number"))
        .collect();
    nums.chunks(2).fold(
        [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ],
        |b, p| {
            [
                b[0].min(p[0]),
                b[1].min(p[1]),
                b[2].max(p[0]),
                b[3].max(p[1]),
            ]
        },
    )
}

/// The clean path stays within half a unit of the square.
fn assert_inside_square(shape: &CleanShape) {
    let b = d_bounds(shape.path_d());
    assert!(
        b[0] >= -0.5 && b[1] >= -0.5 && b[2] <= 100.5 && b[3] <= 100.5,
        "{b:?}"
    );
}

const SMILE_D: &str = "M75.58 58.33L64 69.91 53.42 59.33H46l-2.17-2h10.42L64 67.09l10.75-10.76H100V28.55L0 0v100l100-22.33V58.33zM12.52 37.8a9.26 9.26 0 1 1 9.26-9.26 9.26 9.26 0 0 1-9.26 9.26z";

// ---------------------------------------------------------------------------------------
// Real-world exports
// ---------------------------------------------------------------------------------------

#[test]
fn illustrator_export_with_a_1000_unit_viewbox_doctype_and_css() {
    let svg = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Generator: Adobe Illustrator 27.0.0, SVG Export Plug-In . SVG Version: 6.00 Build 0)  -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg version="1.1" id="Layer_1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" x="0px" y="0px"
	 viewBox="0 0 1000 1000" style="enable-background:new 0 0 1000 1000;" xml:space="preserve">
<style type="text/css">
	.st0{{fill:#231F20;}}
</style>
<g transform="scale(10)"><path class="st0" d="{SMILE_D}"/></g>
</svg>"#
    );
    let shape = run(&svg).expect("illustrator export");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    let score = iou(
        &alpha(&catalog_svg(AssetKind::Head, "smile"), 400),
        &shape_alpha(&shape, 400),
    );
    assert!(score > 0.995, "{score}");
    assert!(shape.info().is_empty(), "{:?}", shape.info());
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());
}

#[test]
fn illustrator_clipping_mask_symbol_and_gradient() {
    // Illustrator's structure for a clipped, gradient-filled shape: CSS classes carry
    // the clip-path and the fill, the clip uses a `<use>` of a shape in `<defs>`, and
    // the gradient sits next to the shape inside the clipped group. None of it is a loop.
    let svg = format!(
        r##"<?xml version="1.0" encoding="utf-8"?>
<!-- Generator: Adobe Illustrator 28.0.0 -->
<svg version="1.1" id="Layer_1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" x="0px" y="0px"
	 viewBox="0 0 100 100" style="enable-background:new 0 0 100 100;" xml:space="preserve">
<style type="text/css">
	.st0{{clip-path:url(#SVGID_2_);}}
	.st1{{fill:url(#SVGID_3_);}}
</style>
<symbol id="Eye" viewBox="0 0 10 10"><circle cx="5" cy="5" r="5"/></symbol>
<g>
	<defs><rect id="SVGID_1_" width="100" height="100"/></defs>
	<clipPath id="SVGID_2_"><use xlink:href="#SVGID_1_" style="overflow:visible;"/></clipPath>
	<g class="st0">
		<linearGradient id="SVGID_3_" gradientUnits="userSpaceOnUse" x1="0" y1="50" x2="100" y2="50">
			<stop offset="0" style="stop-color:#000000"/>
			<stop offset="1" style="stop-color:#333333"/>
		</linearGradient>
		<path class="st1" d="{SMILE_D}"/>
	</g>
</g>
</svg>"##
    );
    let shape = run(&svg).expect("illustrator clip and gradient");
    assert_eq!(info(&shape), vec!["gradient", "clip_or_mask"]);
    let score = iou(
        &alpha(&catalog_svg(AssetKind::Head, "smile"), 400),
        &shape_alpha(&shape, 400),
    );
    assert!(score > 0.995, "{score}");
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());
}

#[test]
fn illustrator_save_as_with_editing_data() {
    // Illustrator's legacy File > Save As > SVG ("Preserve Illustrator Editing
    // Capabilities"): namespace URIs declared as entities in the DOCTYPE, and a
    // `<switch>` whose first branch is a foreignObject pointing at Illustrator's private
    // data. The entities are plain text, so they are accepted; the switch branch isn't
    // web content, so nothing is reported as removed.
    let svg = fixture("illustrator/save-as-editing-data.svg");
    let shape = run_bytes(&svg, &[]).expect("illustrator save as");
    assert!(shape.info().is_empty(), "{:?}", shape.info());
    let score = iou(
        &alpha(&catalog_svg(AssetKind::Head, "smile"), 400),
        &shape_alpha(&shape, 400),
    );
    assert!(score > 0.995, "{score}");
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());
    // HTML in such a branch is still web content.
    let html = String::from_utf8(svg).expect("utf-8").replace(
        "<i:aipgfRef",
        "<iframe xmlns=\"http://www.w3.org/1999/xhtml\"/><i:aipgfRef",
    );
    let shape = run(&html).expect("with html");
    assert_eq!(info(&shape), vec!["active_content_removed"]);
}

#[test]
fn shapes_groups_transforms_and_a_px_size_without_a_viewbox() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200px" height="200px">
      <g transform="translate(0 0)">
        <rect x="0" y="0" width="100" height="200"/>
        <g transform="translate(100 100) rotate(45)"><rect x="-40" y="-40" width="80" height="80"/></g>
        <polygon points="100,0 200,30 100,60"/>
        <circle cx="160" cy="170" r="25"/>
        <ellipse cx="150" cy="100" rx="10" ry="5" fill="none"/>
      </g></svg>"#;
    let shape = run(svg).expect("shapes");
    let score = iou(&alpha(svg, 400), &shape_alpha(&shape, 400));
    assert!(score > 0.99, "{score}");
    assert!(shape.metrics().left_edge_pct > 99.0);
}

#[test]
fn a_non_square_canvas_is_centred_with_a_tip() {
    let shape = run(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 200 100\"><rect width=\"200\" \
         height=\"100\"/></svg>",
    )
    .expect("wide");
    assert_eq!(
        shape.info(),
        vec![Lint::NonSquare {
            width: 200,
            height: 100
        }]
    );
    let b = shape.metrics().bbox.expect("bbox");
    assert!(
        (b[1] - 25.0).abs() < 0.01 && (b[3] - 75.0).abs() < 0.01,
        "{b:?}"
    );
}

#[test]
fn an_evenodd_hole_is_kept() {
    // Both subpaths wind the same way: only even-odd makes the hole.
    let shape = run(&format!(
        "{SVG_OPEN}<path style=\"fill:#000;fill-rule:evenodd\" d=\"M0 0H100V100H0Z \
         M40 40H60V60H40Z\"/></svg>"
    ))
    .expect("evenodd");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    assert_eq!(shape.fill_rule(), FillRule::EvenOdd);
    assert!(!filled_at(&shape, 50.0, 50.0));
    assert!(filled_at(&shape, 20.0, 20.0));
}

#[test]
fn overlapping_shapes_of_opposite_winding_stay_filled() {
    // The second rectangle winds the other way. Concatenated as is, the overlap would
    // be a hole with either fill rule; each shape is turned to wind the same way first.
    let svg = format!("{SVG_OPEN}<path d=\"M0 0H60V100H0Z\"/><path d=\"M40 20V80H90V20Z\"/></svg>");
    let shape = run(&svg).expect("overlap");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    assert_eq!(shape.fill_rule(), FillRule::NonZero);
    assert!(filled_at(&shape, 50.0, 50.0), "the overlap stays filled");
    let score = iou(&alpha(&svg, 400), &shape_alpha(&shape, 400));
    assert!(score > 0.995, "{score}");
}

#[test]
fn white_details_become_cut_outs() {
    // On top of a dark shape: a hole. Under one, showing through its hole: not ink.
    // On nothing (a white background): not ink.
    let svg = format!(
        "{SVG_OPEN}<rect width=\"100\" height=\"100\" fill=\"#fff\"/>\
         <rect width=\"80\" height=\"100\" fill=\"#111\"/><circle cx=\"40\" cy=\"30\" r=\"10\" \
         fill=\"white\"/></svg>"
    );
    let shape = run(&svg).expect("white details");
    assert!(!filled_at(&shape, 40.0, 30.0), "the eye is a hole");
    assert!(filled_at(&shape, 10.0, 80.0));
    assert!(
        !filled_at(&shape, 90.0, 50.0),
        "the white background is dropped"
    );
    assert_eq!(info(&shape), vec!["colours_flattened"]);
    assert_eq!(shape.metrics().holes, 1);
}

#[test]
fn layered_details_that_no_fill_rule_reproduces_are_retraced() {
    // Black head, white eye, black pupil, and a white highlight half outside the pupil
    // (cosmic-horror's eyes): with even-odd the highlight's outer half would be ink.
    let svg = format!(
        "{SVG_OPEN}<path d=\"M0 0H100V100H0Z\"/><circle cx=\"40\" cy=\"40\" r=\"20\" \
         fill=\"#fff\"/><circle cx=\"45\" cy=\"40\" r=\"10\"/><circle cx=\"52\" cy=\"35\" \
         r=\"5\" fill=\"#fff\"/></svg>"
    );
    let shape = run(&svg).expect("layers");
    assert_eq!(shape.strategy(), Strategy::Retraced);
    assert!(!filled_at(&shape, 25.0, 40.0), "white of the eye");
    assert!(filled_at(&shape, 42.0, 43.0), "pupil");
    assert!(
        !filled_at(&shape, 55.0, 33.0),
        "highlight outside the pupil"
    );
    assert!(filled_at(&shape, 90.0, 90.0));
}

#[test]
fn small_holes_and_marks_are_never_lost_on_the_vector_path() {
    // A detail far smaller than the tolerance on differing pixels (0.1%, about 10 unit²)
    // used to be dropped silently: the first candidate within it won. Each of these is
    // at least 2 unit², which the retrace keeps (it removes details under 1 unit²).

    // A 3x3 even-odd hole in a square: nonzero fills it in, even-odd keeps it.
    let shape = run(&format!(
        "{SVG_OPEN}<path fill-rule=\"evenodd\" d=\"M0 0H100V100H0Z M68 28h3v3h-3Z\"/></svg>"
    ))
    .expect("3x3 hole");
    assert_eq!(
        (shape.strategy(), shape.fill_rule()),
        (Strategy::VectorExact, FillRule::EvenOdd)
    );
    assert!(!filled_at(&shape, 69.5, 29.5), "the hole is kept");
    assert_eq!(shape.metrics().holes, 1);

    // A white eye of radius 1.7 (35 px on the 1000 px template) on a black head.
    let shape = run(&format!(
        "{SVG_OPEN}<path d=\"M0 0H70L100 50L70 100H0Z\"/><circle cx=\"40\" cy=\"30\" \
         r=\"1.7\" fill=\"#fff\"/></svg>"
    ))
    .expect("small eye");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    assert!(!filled_at(&shape, 40.0, 30.0), "the eye is a hole");
    assert_eq!(shape.metrics().holes, 1);

    // Two 2x2 white nostrils on the head and a 3x3 white sparkle beside it: the
    // nostrils are holes and the sparkle is nothing. No fill rule gives both, so it is
    // retraced.
    let shape = run(&format!(
        "{SVG_OPEN}<path d=\"M0 0H80V100H0Z\"/><rect x=\"60\" y=\"40\" width=\"2\" \
         height=\"2\" fill=\"#fff\"/><rect x=\"60\" y=\"56\" width=\"2\" height=\"2\" \
         fill=\"#fff\"/><rect x=\"88\" y=\"20\" width=\"3\" height=\"3\" fill=\"#fff\"/></svg>"
    ))
    .expect("nostrils and sparkle");
    assert_eq!(shape.strategy(), Strategy::Retraced);
    assert!(!filled_at(&shape, 61.0, 41.0) && !filled_at(&shape, 61.0, 57.0));
    assert!(!filled_at(&shape, 89.5, 21.5), "the sparkle isn't ink");
    assert!(filled_at(&shape, 40.0, 50.0));
    assert_eq!(shape.metrics().holes, 2);
}

#[test]
fn geometry_off_the_square_never_reaches_the_output() {
    // A stray shape wholly off the square is dropped from the exact path; before, it
    // stayed in `d` and Fit measured it, so Fit did nothing.
    let stray = format!(
        "{SVG_OPEN}<path d=\"M0 15H50L70 50L50 85H0Z\"/><rect x=\"150\" y=\"5\" \
         width=\"10\" height=\"90\"/></svg>"
    );
    let shape = run(&stray).expect("stray");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    assert_inside_square(&shape);
    assert_eq!(info(&shape), vec!["outside_canvas"]);
    assert!(codes(&shape.lints().head).contains(&"margins"));
    let fitted = run_bytes(stray.as_bytes(), &[Fix::Fit]).expect("fitted");
    assert!(
        !codes(&fitted.lints().head).contains(&"margins"),
        "{:?}",
        fitted.lints()
    );
    let b = fitted.metrics().bbox.expect("bbox");
    assert!(b[1] < 1.0 && b[3] > 99.0, "{b:?}");

    // A shape that crosses the edge is retraced (the trace only sees the square), and
    // reported on either path.
    let crossing = format!("{SVG_OPEN}<path d=\"M0 0H90L130 50L90 100H0Z\"/></svg>");
    let shape = run(&crossing).expect("crossing");
    assert_eq!(shape.strategy(), Strategy::Retraced);
    assert_inside_square(&shape);
    assert_eq!(info(&shape), vec!["outside_canvas"]);
    let layered = format!(
        "{SVG_OPEN}<path d=\"M0 0H90L130 50L90 100H0Z\"/><circle cx=\"40\" cy=\"40\" \
         r=\"20\" fill=\"#fff\"/><circle cx=\"45\" cy=\"40\" r=\"10\"/><circle \
         cx=\"52\" cy=\"35\" r=\"5\" fill=\"#fff\"/></svg>"
    );
    let shape = run(&layered).expect("layered");
    assert_eq!(shape.strategy(), Strategy::Retraced);
    assert_eq!(info(&shape), vec!["colours_flattened", "outside_canvas"]);

    // The artist's own clip already cut the overflow (a Figma frame): nothing is
    // outside, and the clip's cut is what comes out.
    let framed = r##"<svg width="100" height="100" viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
      <g clip-path="url(#clip0_1_2)"><path d="M0 0H130V100H0Z" fill="black"/>
      <circle cx="40" cy="30" r="6" fill="white"/></g>
      <defs><clipPath id="clip0_1_2"><rect width="100" height="100" fill="white"/></clipPath></defs></svg>"##;
    let shape = run(framed).expect("framed");
    assert_inside_square(&shape);
    assert_eq!(info(&shape), vec!["colours_flattened", "clip_or_mask"]);
    assert!(!filled_at(&shape, 40.0, 30.0));
    assert!(shape.metrics().fill_pct > 97.0, "{:?}", shape.metrics());
}

#[test]
fn a_figma_frame_clip_is_applied() {
    let svg = r##"<svg width="100" height="100" viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
      <g clip-path="url(#clip0_1_2)"><rect x="-20" y="-20" width="140" height="140" fill="#000"/>
      <circle cx="50" cy="50" r="10" fill="#000"/></g>
      <defs><clipPath id="clip0_1_2"><rect width="100" height="60" fill="white"/></clipPath></defs></svg>"##;
    let shape = run(svg).expect("figma");
    let clip_only = Lint::ClipOrMask {
        clipped: true,
        masked: false,
    };
    assert_eq!(shape.info(), vec![clip_only.clone()]);
    // Figma clips every frame; there is no mask to warn about.
    assert!(
        !clip_only.message().to_lowercase().contains("mask"),
        "{}",
        clip_only.message()
    );
    assert!(filled_at(&shape, 50.0, 30.0));
    assert!(!filled_at(&shape, 50.0, 80.0), "the clipped area is empty");
}

/// A placed picture as Figma exports it: a shape filled with a pattern that holds only
/// a `<use>` of the `<image>` (Sketch and Penpot write the same, without the `<use>`).
fn figma_image_fill(attrs: &str) -> String {
    format!(
        "<rect x=\"10\" y=\"10\" width=\"80\" height=\"80\" fill=\"url(#pattern0_1_2)\"{attrs}/>\
         <defs><pattern id=\"pattern0_1_2\" patternContentUnits=\"objectBoundingBox\" \
         width=\"1\" height=\"1\"><use xlink:href=\"#image0_1_2\" transform=\"scale(0.25)\"/>\
         </pattern><image id=\"image0_1_2\" width=\"4\" height=\"4\" \
         xlink:href=\"data:image/png;base64,iVBORw0KGgo=\"/></defs>"
    )
}

const SVG_XLINK: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" \
     xmlns:xlink=\"http://www.w3.org/1999/xlink\" viewBox=\"0 0 100 100\">";

#[test]
fn pictures_placed_as_pattern_fills_are_ignored_like_images() {
    // A pattern whose content paints nothing (the image is never loaded) is no paint:
    // the picture's box used to be painted as solid black, with a tip saying the image
    // was ignored.
    let only = run(&format!("{SVG_XLINK}{}</svg>", figma_image_fill(""))).expect_err("nothing");
    assert_eq!(
        only,
        ProcessError::Empty {
            info: vec![Lint::ImageIgnored { count: 1 }]
        }
    );
    assert!(
        only.user_message().contains("pictures"),
        "{}",
        only.user_message()
    );
    // Penpot and Sketch: the image directly in the pattern, in user space, in a group;
    // as a stroke too.
    let penpot = format!(
        "{SVG_XLINK}<rect width=\"80\" height=\"80\" fill=\"url(#p)\" stroke=\"url(#p)\" \
         stroke-width=\"9\"/><defs><pattern id=\"p\" patternUnits=\"userSpaceOnUse\" \
         width=\"80\" height=\"80\"><g><image width=\"80\" height=\"80\" \
         href=\"data:image/png;base64,iVBORw0KGgo=\"/></g></pattern></defs></svg>"
    );
    assert_eq!(
        run(&penpot).err(),
        Some(ProcessError::Empty {
            info: vec![Lint::ImageIgnored { count: 1 }]
        })
    );

    // A reference photo left visible above or below the drawing changes nothing.
    let drawing = "<path d=\"M0 0H70L100 50L70 100H0Z\"/><circle cx=\"60\" cy=\"30\" r=\"7\" \
                   fill=\"white\"/>";
    let alone = run(&format!("{SVG_XLINK}{drawing}</svg>")).expect("drawing");
    assert_eq!(alone.metrics().holes, 1);
    for (name, svg) in [
        (
            "on top at 50%",
            format!(
                "{SVG_XLINK}{drawing}{}</svg>",
                figma_image_fill(" fill-opacity=\"0.5\"")
            ),
        ),
        (
            "underneath",
            format!("{SVG_XLINK}{}{drawing}</svg>", figma_image_fill("")),
        ),
    ] {
        let shape = run(&svg).expect(name);
        assert_eq!(shape.path_d(), alone.path_d(), "{name}");
        assert_eq!(shape.strategy(), alone.strategy(), "{name}");
        let mut want = info(&alone);
        want.push("image_ignored");
        assert_eq!(info(&shape), want, "{name}");
    }

    // A pattern with something drawn in it still paints (in one colour).
    let real = format!(
        "{SVG_OPEN}<defs><pattern id=\"p\" width=\"10\" height=\"10\" \
         patternUnits=\"userSpaceOnUse\"><rect width=\"5\" height=\"5\"/></pattern></defs>\
         <rect width=\"50\" height=\"50\" fill=\"url(#p)\"/></svg>"
    );
    let shape = run(&real).expect("texture");
    assert_eq!(info(&shape), vec!["gradient"]);
    assert!(filled_at(&shape, 25.0, 25.0));
}

#[test]
fn input_facts_are_reported() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
      <defs><linearGradient id="g"><stop offset="0" stop-color="#000"/><stop offset="1" stop-color="#333"/></linearGradient>
      <filter id="f"><feGaussianBlur stdDeviation="5"/></filter>
      <mask id="m"><rect width="50" height="50" fill="white"/></mask></defs>
      <rect width="100" height="100" fill="url(#g)" filter="url(#f)"/>
      <path d="M10 10L90 90" stroke="#000" stroke-width="4"/>
      <rect x="90" y="0" width="30" height="10" fill="#000" opacity="0.4" mask="url(#m)"/>
      <rect x="95" y="90" width="10" height="5" fill="#000"/>
      <text x="10" y="50">hi</text>
      <image href="data:image/png;base64,iVBORw0KGgo=" width="10" height="10"/>
    </svg>"##;
    let shape = run(svg).expect("facts");
    assert_eq!(
        info(&shape),
        vec![
            "colours_flattened",
            "strokes_converted",
            "gradient",
            "semi_transparent",
            "clip_or_mask",
            "filters_ignored",
            "image_ignored",
            "text_ignored",
            "outside_canvas",
        ]
    );
    assert!(shape.info().contains(&Lint::StrokesConverted { count: 1 }));
    assert!(shape.info().contains(&Lint::ImageIgnored { count: 1 }));
    // A mask, and no clip path: the message is about the mask only.
    assert!(shape.info().contains(&Lint::ClipOrMask {
        clipped: false,
        masked: true
    }));
    for lint in shape.info() {
        assert!(!lint.message().is_empty());
        assert!(lint.guide_anchor().starts_with('#'));
        let tip = matches!(lint, Lint::ImageIgnored { .. } | Lint::TextIgnored);
        let want = if tip { Severity::Tip } else { Severity::Info };
        assert_eq!(lint.severity(), want, "{}", lint.code());
    }
}

#[test]
fn a_stroke_only_drawing_is_outlined() {
    let shape = run(&format!(
        "{SVG_OPEN}<path d=\"M0 50H90\" stroke=\"black\" stroke-width=\"20\" fill=\"none\"/></svg>"
    ))
    .expect("stroke");
    assert_eq!(info(&shape), vec!["strokes_converted"]);
    assert!(filled_at(&shape, 45.0, 50.0));
    assert!(!filled_at(&shape, 45.0, 20.0));
}

#[test]
fn nothing_drawable_is_empty_with_a_reason() {
    let text = run(&format!(
        "{SVG_OPEN}<text x=\"0\" y=\"50\" font-size=\"80\">S</text></svg>"
    ))
    .expect_err("text only");
    assert_eq!(
        text,
        ProcessError::Empty {
            info: vec![Lint::TextIgnored]
        }
    );
    // The message says text isn't supported and how to outline it, but only if the
    // drawing is text: the text may be hidden, or a label beside a drawing that isn't
    // ink, and outlining it would make it the drawing.
    let msg = text.user_message();
    assert!(
        msg.contains("if your drawing is text") && msg.contains("outlines"),
        "{msg}"
    );
    assert!(msg.contains("Otherwise, draw in solid black"), "{msg}");
    let image = run(&format!(
        "{SVG_OPEN}<image href=\"drawing.png\" width=\"100\" height=\"100\"/></svg>"
    ))
    .expect_err("image only");
    assert_eq!(
        image,
        ProcessError::Empty {
            info: vec![Lint::ImageIgnored { count: 1 }]
        }
    );
    let msg = image.user_message();
    assert!(msg.contains("PNG") && !msg.contains("solid black"), "{msg}");
    assert_eq!(
        run("<svg xmlns=\"http://www.w3.org/2000/svg\"/>").err(),
        Some(ProcessError::Empty { info: vec![] })
    );
    // Hidden content isn't the drawing.
    assert_eq!(
        run(&format!(
            "{SVG_OPEN}<rect width=\"50\" height=\"50\" style=\"display:none\"/><rect \
             width=\"50\" height=\"50\" visibility=\"hidden\"/></svg>"
        ))
        .err(),
        Some(ProcessError::Empty { info: vec![] })
    );
}

#[test]
fn fixes_apply_to_svg_input() {
    let mirrored = board_svg(
        &format!(
            "<g transform=\"scale(-1,1) translate(-100,0)\">{}</g>",
            catalog_inner(AssetKind::Head, "smile")
        ),
        "#000",
    );
    let shape = run(&mirrored).expect("mirrored");
    assert!(codes(&shape.lints().head).contains(&"faces_left"));
    let fixed = run_bytes(mirrored.as_bytes(), &[Fix::Flip]).expect("flipped");
    assert!(fixed.passes(AssetKind::Head), "{:?}", fixed.lints());
    let score = iou(
        &alpha(&catalog_svg(AssetKind::Head, "smile"), 400),
        &shape_alpha(&fixed, 400),
    );
    assert!(score > 0.995, "{score}");
}

// ---------------------------------------------------------------------------------------
// The SVG template (fixtures/design_kit/template/head-template.svg, from the template
// generator): draw-here, guides, references, and exports that strip ids.
// ---------------------------------------------------------------------------------------

fn template() -> String {
    String::from_utf8(fixture("template/head-template.svg")).expect("utf-8")
}

/// The template with `inner` drawn into its `draw-here` layer.
fn template_with(inner: &str) -> String {
    let t = template();
    let layer = t.find("id=\"draw-here\"").expect("draw-here layer");
    let open_end = layer + t[layer..].find('>').expect("tag end") + 1;
    format!("{}{inner}{}", &t[..open_end], &t[open_end..])
}

/// Remove every `id` attribute (Figma and some exporters do).
fn strip_ids(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(i) = rest.find(" id=\"") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 5..];
        rest = &after[after.find('"').expect("closing quote") + 1..];
    }
    out.push_str(rest);
    out
}

fn head_iou(shape: &CleanShape, slug: &str) -> f64 {
    iou(
        &alpha(&catalog_svg(AssetKind::Head, slug), 400),
        &shape_alpha(shape, 400),
    )
}

#[test]
fn the_empty_template_is_empty() {
    // Only guides: the message asks to draw on "Draw here" and hide the guides.
    let e = run(&template()).expect_err("nothing drawn");
    assert_eq!(
        e,
        ProcessError::Empty {
            info: vec![Lint::GuidesVisible]
        }
    );
    assert!(
        e.user_message().contains("Draw here"),
        "{}",
        e.user_message()
    );
}

#[test]
fn a_drawing_in_draw_here_is_used_alone() {
    let shape = run(&template_with(&catalog_inner(AssetKind::Head, "bendr"))).expect("bendr");
    let score = head_iou(&shape, "bendr");
    assert!(score >= 0.99, "{score}");
    assert_eq!(shape.strategy(), Strategy::VectorExact);
    // Only draw-here was looked at, so the guides didn't need ignoring.
    assert!(shape.info().is_empty(), "{:?}", shape.info());
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());
}

#[test]
fn stripped_ids_with_visible_guides_still_give_the_drawing() {
    let svg = strip_ids(&template_with(&catalog_inner(AssetKind::Head, "bendr")));
    assert!(!svg.contains(" id="));
    let shape = run(&svg).expect("no ids");
    let score = head_iou(&shape, "bendr");
    assert!(score >= 0.99, "{score}");
    assert_eq!(info(&shape), vec!["guides_visible"]);
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());

    // References made visible too: the ghost colour is dropped as well.
    let shown = svg.replace("style=\"display:none\"", "");
    let shape = run(&shown).expect("references shown");
    let score = head_iou(&shape, "bendr");
    assert!(score >= 0.99, "{score}");
    assert_eq!(info(&shape), vec!["guides_visible"]);
}

#[test]
fn a_drawing_on_a_new_layer_beside_an_empty_draw_here_is_found() {
    let t = template();
    let close = t.rfind("</svg>").expect("root end");
    let svg = format!(
        "{}<g id=\"Layer 1\">{}</g>{}",
        &t[..close],
        catalog_inner(AssetKind::Head, "smile"),
        &t[close..]
    );
    let shape = run(&svg).expect("new layer");
    let score = head_iou(&shape, "smile");
    assert!(score >= 0.99, "{score}");
    assert_eq!(info(&shape), vec!["guides_visible"]);
}

#[test]
fn layers_left_out_beside_draw_here_are_reported() {
    // The body in draw-here and a horn on a new layer: only draw-here is used, so the
    // horn is left out, and she is told.
    let with_layer = |layer: &str| {
        let t = template_with("<rect width=\"60\" height=\"100\"/>");
        let close = t.rfind("</svg>").expect("root end");
        format!("{}{layer}{}", &t[..close], &t[close..])
    };
    let shape = run(&with_layer(
        "<g id=\"Layer 2\"><rect x=\"60\" y=\"20\" width=\"40\" height=\"60\" fill=\"#000\"/></g>",
    ))
    .expect("two layers");
    assert!(
        (shape.metrics().fill_pct - 60.0).abs() < 0.5,
        "{:?}",
        shape.metrics()
    );
    assert_eq!(info(&shape), vec!["outside_draw_here_ignored"]);
    let lint = &shape.info()[0];
    assert_eq!(lint.severity(), Severity::Tip);
    assert!(lint.message().contains("Draw here"), "{}", lint.message());
    // A white background layer, or a faint sketch, isn't ink: nothing to report.
    for harmless in [
        "<g id=\"Background\"><rect width=\"100\" height=\"100\" fill=\"#fff\"/></g>",
        "<g id=\"Sketch\" opacity=\"0.3\"><rect x=\"60\" width=\"40\" height=\"100\"/></g>",
    ] {
        let shape = run(&with_layer(harmless)).expect("harmless layer");
        assert!(shape.info().is_empty(), "{harmless}: {:?}", shape.info());
    }
    // Draw-here holds ink, all of it off the square, and the horn is left out: nothing
    // is left, and the message says which layer we used.
    let t = template_with("<rect x=\"200\" width=\"60\" height=\"100\"/>");
    let close = t.rfind("</svg>").expect("root end");
    let e = run(&format!(
        "{}<g id=\"Layer 2\"><rect x=\"60\" y=\"20\" width=\"40\" height=\"60\"/></g>{}",
        &t[..close],
        &t[close..]
    ))
    .expect_err("nothing in the square");
    assert_eq!(
        e,
        ProcessError::Empty {
            info: vec![Lint::OutsideDrawHereIgnored]
        }
    );
    assert!(
        e.user_message().contains("Draw here") && !e.user_message().contains("solid black"),
        "{}",
        e.user_message()
    );
    // Text in draw-here: the text is the reason, not the visible guides.
    let e = run(&template_with("<text x=\"10\" y=\"50\">Hi</text>")).expect_err("text");
    assert_eq!(
        e,
        ProcessError::Empty {
            info: vec![Lint::GuidesVisible, Lint::TextIgnored]
        }
    );
    assert!(
        e.user_message().contains("outlines"),
        "{}",
        e.user_message()
    );
}

#[test]
fn clips_around_draw_here_apply_to_it() {
    // Figma exports wrap layers in a clipped frame. The clip of a group around
    // draw-here applies to the drawing, as in the artist's app.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
      <defs><clipPath id="half"><rect width="100" height="50"/></clipPath></defs>
      <g id="template" clip-path="url(#half)">
        <g id="guides"><path d="M0 50H100" stroke="#2f8fd6"/></g>
        <g id="draw-here"><rect width="100" height="100"/></g>
      </g></svg>"##;
    let shape = run(svg).expect("clipped draw-here");
    assert!(
        (shape.metrics().fill_pct - 50.0).abs() < 0.5,
        "{:?}",
        shape.metrics()
    );
    assert!(filled_at(&shape, 50.0, 25.0) && !filled_at(&shape, 50.0, 75.0));
    assert_eq!(
        shape.info(),
        vec![Lint::ClipOrMask {
            clipped: true,
            masked: false
        }]
    );
    // The same file without the template's names (everything is the drawing) agrees.
    let plain = run(&svg.replace("draw-here", "layer-1")).expect("plain");
    assert_eq!(plain.path_d(), shape.path_d());
}

// ---------------------------------------------------------------------------------------
// Hostile SVGs, each with its exact outcome
// ---------------------------------------------------------------------------------------

#[test]
fn active_content_never_passes_through() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 100 100" onload="alert(1)">
      <script>alert(document.cookie)</script>
      <foreignObject width="100" height="100"><iframe xmlns="http://www.w3.org/1999/xhtml" src="https://evil.example"/></foreignObject>
      <a href="javascript:alert(1)"><rect width="50" height="100" onclick="alert(2)"/></a>
      <set attributeName="href" to="javascript:alert(3)"/>
      <use href="https://evil.example/sprite.svg#x"/>
      <style>@import url(https://evil.example/x.css); rect { fill: black }</style>
    </svg>"##;
    let shape = run(svg).expect("active content");
    assert_eq!(info(&shape), vec!["active_content_removed"]);
    let out = shape.to_svg();
    for bad in [
        "script",
        "alert",
        "onload",
        "onclick",
        "evil",
        "foreignObject",
        "iframe",
        "href",
        "style",
        "javascript",
        "set",
    ] {
        assert!(!out.contains(bad), "{bad} leaked: {out}");
    }
    assert!(filled_at(&shape, 25.0, 50.0));
    assert!(!filled_at(&shape, 75.0, 50.0));
}

#[test]
fn local_files_named_by_href_are_not_read() {
    // usvg's default image resolver reads files named by `<image href>` (relative to the
    // working directory when there is no base directory). Prove that the default does,
    // so the test means something, then that ours doesn't.
    let dir = std::env::temp_dir().join(format!("design-kit-lfi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let secret = dir.join("secret.svg");
    std::fs::write(
        &secret,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>"#,
    )
    .expect("secret");
    let svg = format!(
        "{SVG_OPEN}<rect width=\"10\" height=\"100\"/><image href=\"{}\" width=\"100\" \
         height=\"100\"/><use href=\"{}#x\"/></svg>",
        secret.display(),
        secret.display()
    );
    let default_tree =
        resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("default parse");
    fn has_image(g: &resvg::usvg::Group) -> bool {
        g.children().iter().any(|n| match n {
            resvg::usvg::Node::Image(_) => true,
            resvg::usvg::Node::Group(g) => has_image(g),
            _ => false,
        })
    }
    assert!(
        has_image(default_tree.root()),
        "expected usvg's default resolver to read the local file"
    );

    let shape = run(&svg).expect("ours");
    let fill = shape.metrics().fill_pct;
    assert!((fill - 10.0).abs() < 0.5, "only the 10-wide rect: {fill}");
    assert_eq!(
        info(&shape),
        vec!["image_ignored", "active_content_removed"]
    );
    let _ = std::fs::remove_dir_all(dir);
}

const ENTITIES_REJECTED: &str = "entity declarations other than short plain text are not allowed";

#[test]
fn entity_declarations_are_invalid_xml() {
    let rejected = Some(ProcessError::InvalidXml(ENTITIES_REJECTED.into()));
    let xxe = r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><svg xmlns="http://www.w3.org/2000/svg"><text>&xxe;</text><rect width="10" height="10"/></svg>"#;
    let e = run(xxe).expect_err("xxe");
    assert_eq!(Some(e.clone()), rejected);
    assert!(
        e.user_message().contains("Export As"),
        "{}",
        e.user_message()
    );
    let mut lol = String::from("<?xml version=\"1.0\"?><!DOCTYPE svg [<!ENTITY lol0 \"lol\">");
    for i in 1..10 {
        lol.push_str(&format!(
            "<!ENTITY lol{i} \"{}\">",
            format!("&lol{};", i - 1).repeat(10)
        ));
    }
    lol.push_str("]><svg xmlns=\"http://www.w3.org/2000/svg\"><text>&lol9;</text></svg>");
    let t = Instant::now();
    assert_eq!(run(&lol).err(), rejected);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    // Plain-text entities as Illustrator writes them are accepted (see
    // `illustrator_save_as_with_editing_data`), but not parameter entities, markup in a
    // value, or references that expand to too much text.
    let doc = |decls: &str, body: &str| {
        format!(
            "<!DOCTYPE svg [{decls}]><svg xmlns=\"http://www.w3.org/2000/svg\" \
             viewBox=\"0 0 100 100\">{body}<rect width=\"50\" height=\"100\"/></svg>"
        )
    };
    for bad in [
        doc("<!ENTITY % p \"x\">", ""),
        doc("<!ENTITY a \"<g/>\">", "<g>&a;</g>"),
        doc("<!ENTITY a PUBLIC \"x\" \"http://evil.example/x\">", ""),
        doc(
            &format!("<!ENTITY a \"{}\">", "x".repeat(200)),
            &"<desc>&a;</desc>".repeat(2_000),
        ),
        doc(&format!("<!ENTITY a \"{}\">", "x".repeat(300)), ""),
    ] {
        assert_eq!(run(&bad).err(), rejected, "{}", &bad[..80]);
    }
    assert!(run(&doc("<!ENTITY a \"#000\">", "<desc>&a;</desc>")).is_ok());
    // At most 32 declarations (Illustrator writes 8).
    let decls = |n: usize| -> String { (0..n).map(|i| format!("<!ENTITY e{i} \"x\">")).collect() };
    assert!(run(&doc(&decls(32), "")).is_ok());
    assert_eq!(run(&doc(&decls(33), "")).err(), rejected);
    // A broken document is invalid XML too. (A plain DOCTYPE, as Illustrator writes,
    // is fine: see the Illustrator export test.)
    assert!(matches!(
        run("<svg xmlns=\"http://www.w3.org/2000/svg\"><g></svg>"),
        Err(ProcessError::InvalidXml(_))
    ));
    assert_eq!(ProcessError::InvalidXml("x".into()).code(), "invalid_xml");
}

#[test]
fn deep_nesting_is_too_complex_before_parsing() {
    let deep = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\">{}<rect width=\"9\" height=\"9\"/>{}</svg>",
        "<g>".repeat(5000),
        "</g>".repeat(5000)
    );
    assert_eq!(
        run(&deep).err(),
        Some(ProcessError::TooComplex("elements are nested too deeply"))
    );
    // Before roxmltree parses anything: the same nesting in a file roxmltree would reject
    // (an unterminated comment at the end) is still too complex, not invalid XML.
    assert_eq!(
        run(&format!("{deep}<!-- unterminated")).err(),
        Some(ProcessError::TooComplex("elements are nested too deeply"))
    );
    // The byte scan's limit is the element limit exactly: one level deeper is refused
    // before roxmltree runs; the deepest allowed nesting reaches roxmltree, which then
    // rejects the unterminated comment.
    let nested = |levels: usize| {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}{}</svg><!-- unterminated",
            "<g>".repeat(levels),
            "</g>".repeat(levels)
        )
    };
    assert_eq!(
        run(&nested(64)).err(),
        Some(ProcessError::TooComplex("elements are nested too deeply"))
    );
    assert!(matches!(run(&nested(63)), Err(ProcessError::InvalidXml(_))));
    // Quotes and comments can't hide nesting from the byte scan.
    let tricky = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\">{}<rect width=\"9\" height=\"9\"/>{}</svg>",
        "<g data-x=\"/>\" ><!-- <g/> -->".repeat(200),
        "</g>".repeat(200)
    );
    assert_eq!(
        run(&tricky).err(),
        Some(ProcessError::TooComplex("elements are nested too deeply"))
    );
    // Nor can the DOCTYPE: a quoted `[` once made the scan swallow the whole file (and
    // 19,000 nested groups overflowed a 2 MiB stack in roxmltree). roxmltree ends an
    // ATTLIST at its first `>`, quoted or not, so the scan must too.
    for doctype in [
        "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"[\">]>",
        "<!DOCTYPE svg [<!ATTLIST svg a CDATA \">]>",
    ] {
        let bypass = format!(
            "<?xml version=\"1.0\"?>{doctype}<svg xmlns=\"http://www.w3.org/2000/svg\">{}\
             <rect width=\"9\" height=\"9\"/>{}</svg>",
            "<g>".repeat(19_000),
            "</g>".repeat(19_000)
        );
        assert_eq!(
            run(&bypass).err(),
            Some(ProcessError::TooComplex("elements are nested too deeply")),
            "{doctype}"
        );
    }
    // A DOCTYPE the scan can't follow (roxmltree can't either) is refused.
    assert!(matches!(
        run(
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"x\"]><svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        ),
        Err(ProcessError::InvalidXml(_))
    ));
    // The deepest allowed (63 groups in the root) with a `<use>` deep inside is fine.
    let ok = format!(
        "{SVG_OPEN}{}<rect id=\"r\" width=\"100\" height=\"100\"/><use href=\"#r\"/>{}</svg>",
        "<g transform=\"translate(0 0)\">".repeat(62),
        "</g>".repeat(62)
    );
    assert!(run(&ok).is_ok(), "{:?}", run(&ok).err());
}

/// A def chain of `links` patterns, masks or pattern/`<use>` pairs, each wrapping its
/// content in `nest` groups (the critique's probe: under every per-element cap, but
/// nesting multiplies along the chain).
fn chain(kind: &str, links: usize, nest: usize) -> String {
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

#[test]
fn reference_chains_times_nested_groups_never_abort() {
    // Each of these is 10-50 KB and under every per-element cap (64 deep, 64
    // definitions). Before the nesting budget, the pattern, mask and pattern/<use>
    // variants aborted the process with a stack overflow on a 2 MiB thread. Those within
    // the budget (about 3,600 levels) are processed on the big stack; the rest are
    // stopped by the budget. Either way, the test binary is still alive at the end.
    let nests = ProcessError::TooComplex(
        "references nest too deeply (patterns, masks, clip paths, markers or <use>)",
    );
    // `None`: processed, a full square (patterns paint as one colour, masks are
    // ignored).
    let cases = [
        ("pattern", 60, 58, None),
        ("mask", 60, 58, None),
        // A clipPath may only contain shapes, so the groups make every clip empty.
        ("clip", 60, 58, Some(ProcessError::Empty { info: vec![] })),
        // The deepest chains the caps allow: 64 definitions, each 60 groups deep (about
        // 3,970 levels).
        ("pattern", 64, 60, None),
        ("mask", 64, 60, None),
        // A pattern and a `<use>` per link doubles the nesting.
        ("patuse", 33, 58, None),
        ("patuse", 60, 58, Some(nests)),
    ];
    for (kind, links, nest, want) in cases {
        let svg = chain(kind, links, nest);
        let (r, elapsed) = run_big(&svg);
        println!(
            "{kind} {links}x{nest} ({} bytes): {:?} in {elapsed:?}",
            svg.len(),
            r.as_ref().map(|s| s.strategy())
        );
        match want {
            Some(e) => assert_eq!(r.err(), Some(e), "{kind} {links}x{nest}"),
            None => {
                let shape = r.unwrap_or_else(|e| panic!("{kind} {links}x{nest}: {e:?}"));
                assert!(
                    shape.metrics().fill_pct > 99.0,
                    "{kind}: {:?}",
                    shape.metrics()
                );
            }
        }
    }
}

#[test]
fn reference_cycles_are_too_complex() {
    // usvg only breaks one- and two-step cycles; these recurse forever and abort the
    // process even on a 64 MiB stack (verified against usvg 0.48.1 directly).
    let loops = "patterns, masks, clip paths, markers, filters or <use> refer to each other \
                 in a loop";
    let hdr = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><defs>";
    let pat = |id: &str, next: &str| {
        format!(
            "<pattern id=\"{id}\" width=\"10\" height=\"10\" patternUnits=\"userSpaceOnUse\">\
             <rect width=\"5\" height=\"5\" fill=\"url(#{next})\"/></pattern>"
        )
    };
    let mask = |id: &str, next: &str| {
        format!(
            "<mask id=\"{id}\"><rect width=\"100\" height=\"100\" fill=\"white\" \
             mask=\"url(#{next})\"/></mask>"
        )
    };
    let clip = |id: &str, next: &str| {
        format!(
            "<clipPath id=\"{id}\"><rect width=\"100\" height=\"100\" \
             clip-path=\"url(#{next})\"/></clipPath>"
        )
    };
    let filter = |id: &str, next: &str| {
        format!(
            "<filter id=\"{id}\"><feImage href=\"#r{id}\"/></filter><rect id=\"r{id}\" \
             width=\"5\" height=\"5\" filter=\"url(#{next})\"/>"
        )
    };
    // A pattern holding a `<use>` of `#e` that carries `paint`.
    let use_pat = |id: &str, paint: &str| {
        format!(
            "<pattern id=\"{id}\" width=\"10\" height=\"10\" patternUnits=\"userSpaceOnUse\">\
             <use href=\"#e\" {paint}/></pattern>"
        )
    };
    let three = |f: &dyn Fn(&str, &str) -> String, attr: &str| {
        format!(
            "{hdr}{}{}{}</defs><rect width=\"100\" height=\"100\" {attr}=\"url(#A)\"/></svg>",
            f("A", "B"),
            f("B", "C"),
            f("C", "A")
        )
    };
    let cases = [
        ("pattern", three(&pat, "fill")),
        ("mask", three(&mask, "mask")),
        ("clip", three(&clip, "clip-path")),
        ("filter", three(&filter, "filter")),
        (
            "inherited fill",
            format!(
                "{hdr}</defs><g fill=\"url(#A)\"><pattern id=\"A\" width=\"10\" height=\"10\" \
                 patternUnits=\"userSpaceOnUse\"><rect width=\"5\" height=\"5\"/></pattern>\
                 <rect width=\"100\" height=\"100\"/></g></svg>"
            ),
        ),
        (
            "stylesheet",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><style>\
             .a{fill:url(#B)} .b{fill:url(#C)} .c{fill:url(#A)}</style><defs><pattern id=\"A\" \
             width=\"10\" height=\"10\" patternUnits=\"userSpaceOnUse\"><rect class=\"a\" \
             width=\"5\" height=\"5\"/></pattern><pattern id=\"B\" width=\"10\" height=\"10\" \
             patternUnits=\"userSpaceOnUse\"><rect class=\"b\" width=\"5\" height=\"5\"/>\
             </pattern><pattern id=\"C\" width=\"10\" height=\"10\" \
             patternUnits=\"userSpaceOnUse\"><rect class=\"c\" width=\"5\" height=\"5\"/>\
             </pattern></defs><rect width=\"100\" height=\"100\" fill=\"url(#A)\"/></svg>"
                .to_string(),
        ),
        (
            "style attribute",
            format!(
                "{hdr}{}{}</defs><rect width=\"100\" height=\"100\" style=\"fill:url(#A)\"/></svg>",
                pat("A", "B"),
                pat("B", "A").replace("fill=\"url(#A)\"", "style=\"fill: url('#A')\"")
            ),
        ),
        (
            "xml:id",
            three(&pat, "fill").replace("pattern id=", "pattern xml:id="),
        ),
        (
            // usvg reads presentation attributes in the XML namespace too.
            "xml:fill",
            three(&pat, "fill").replace("height=\"5\" fill=", "height=\"5\" xml:fill="),
        ),
        (
            // usvg reads every `style` element, whatever its namespace.
            "stylesheet in another namespace",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:o=\"urn:other\" \
             viewBox=\"0 0 100 100\"><o:style>.a{fill:url(#B)} .b{fill:url(#C)} \
             .c{fill:url(#A)}</o:style><defs><pattern id=\"A\" width=\"10\" height=\"10\" \
             patternUnits=\"userSpaceOnUse\"><rect class=\"a\" width=\"5\" height=\"5\"/>\
             </pattern><pattern id=\"B\" width=\"10\" height=\"10\" \
             patternUnits=\"userSpaceOnUse\"><rect class=\"b\" width=\"5\" height=\"5\"/>\
             </pattern><pattern id=\"C\" width=\"10\" height=\"10\" \
             patternUnits=\"userSpaceOnUse\"><rect class=\"c\" width=\"5\" height=\"5\"/>\
             </pattern></defs><rect width=\"100\" height=\"100\" fill=\"url(#A)\"/></svg>"
                .to_string(),
        ),
        (
            // `inherit` on a non-inherited property takes the parent's value.
            "clip-path: inherit",
            format!(
                "{hdr}<g clip-path=\"url(#B)\"><clipPath id=\"A\" clip-path=\"inherit\">\
                 <rect width=\"100\" height=\"100\"/></clipPath></g>{}{}</defs><rect \
                 width=\"100\" height=\"100\" clip-path=\"url(#A)\"/></svg>",
                clip("B", "C").replace(
                    "<clipPath id=\"B\">",
                    "<clipPath id=\"B\" clip-path=\"url(#C)\">"
                ),
                clip("C", "A").replace(
                    "<clipPath id=\"C\">",
                    "<clipPath id=\"C\" clip-path=\"url(#A)\">"
                ),
            ),
        ),
        (
            // Ids ending in a tab (a character reference): svgtypes keeps the tab.
            "tab in ids",
            three(&pat, "fill")
                .replace("#A)", "#A&#9;)")
                .replace("#B)", "#B&#9;)")
                .replace("#C)", "#C&#9;)")
                .replace("id=\"A\"", "id=\"A&#9;\"")
                .replace("id=\"B\"", "id=\"B&#9;\"")
                .replace("id=\"C\"", "id=\"C&#9;\""),
        ),
        (
            "use",
            format!(
                "{hdr}<g id=\"A\"><use href=\"#B\"/></g><g id=\"B\"><use href=\"#C\"/></g><g \
                 id=\"C\"><use href=\"#A\"/></g></defs><use href=\"#A\"/><rect width=\"9\" \
                 height=\"9\"/></svg>"
            ),
        ),
        // usvg parses every child of a gradient, stop or filter primitive and finds ids
        // anywhere, and its `<use>` copies elements from inside anything, so a
        // definition's parent hides nothing.
        (
            "pattern inside a gradient, inheriting its fill",
            format!(
                "{hdr}<linearGradient id=\"G\" fill=\"url(#A)\"><pattern id=\"A\" width=\"10\" \
                 height=\"10\" patternUnits=\"userSpaceOnUse\"><rect width=\"5\" height=\"5\"/>\
                 </pattern></linearGradient></defs><rect width=\"100\" height=\"100\" \
                 fill=\"url(#A)\"/></svg>"
            ),
        ),
        (
            "pattern inside a stop",
            format!(
                "{hdr}<linearGradient id=\"G\"><stop offset=\"0\">{}</stop></linearGradient>\
                 {}{}</defs><rect width=\"100\" height=\"100\" fill=\"url(#A)\"/></svg>",
                pat("A", "B"),
                pat("B", "C"),
                pat("C", "A")
            ),
        ),
        (
            "mask inside a filter primitive",
            format!(
                "{hdr}<filter id=\"F\"><feFlood>{}</feFlood></filter>{}{}</defs><rect \
                 width=\"100\" height=\"100\" mask=\"url(#A)\"/></svg>",
                mask("A", "B"),
                mask("B", "C"),
                mask("C", "A")
            ),
        ),
        (
            "clip path inside a gradient",
            format!(
                "{hdr}<radialGradient id=\"G\">{}</radialGradient>{}{}</defs><rect \
                 width=\"100\" height=\"100\" clip-path=\"url(#A)\"/></svg>",
                clip("A", "B"),
                clip("B", "C"),
                clip("C", "A")
            ),
        ),
        // usvg converts a `<use>`'s own fill and stroke once, whatever the copy holds:
        // loops through `<use>`s of an empty group or of text (rejected at 141a415, then
        // missed when paint was weighed by the copy's shapes).
        (
            "<use> of an empty group, filled",
            format!(
                "{hdr}<g id=\"e\"/>{}{}{}</defs><rect width=\"100\" height=\"100\" \
                 fill=\"url(#A)\"/></svg>",
                use_pat("A", "fill=\"url(#B)\""),
                use_pat("B", "fill=\"url(#C)\""),
                use_pat("C", "fill=\"url(#A)\""),
            ),
        ),
        (
            "<use> of text, stroked from a stylesheet, a style and an attribute",
            format!(
                "{hdr}<style>.s{{stroke:url(#B)}}</style><text id=\"e\">hi</text>{}{}{}\
                 </defs><rect width=\"100\" height=\"100\" fill=\"url(#A)\"/></svg>",
                use_pat("A", "class=\"s\""),
                use_pat("B", "style=\"stroke:url(#C)\""),
                use_pat("C", "stroke=\"url(#A)\""),
            ),
        ),
        (
            // usvg copies the unprefixed `href` whatever the order; this decoy names
            // nothing.
            "<use> with a decoy xlink:href first",
            format!(
                "{}{}{}{}</defs><rect width=\"100\" height=\"100\" fill=\"url(#A)\"/></svg>",
                hdr.replace(
                    "<svg ",
                    "<svg xmlns:xlink=\"http://www.w3.org/1999/xlink\" "
                ) + "<g id=\"e\"><rect width=\"5\" height=\"5\"/></g>",
                use_pat("A", "fill=\"url(#B)\"").replace("<use ", "<use xlink:href=\"#nope\" "),
                use_pat("B", "fill=\"url(#C)\"").replace("<use ", "<use xlink:href=\"#nope\" "),
                use_pat("C", "fill=\"url(#A)\"").replace("<use ", "<use xlink:href=\"#nope\" "),
            ),
        ),
        (
            "group inside a foreignObject, copied by <use>",
            format!(
                "{hdr}<pattern id=\"A\" width=\"10\" height=\"10\" \
                 patternUnits=\"userSpaceOnUse\"><use href=\"#x\"/></pattern>{}{}</defs>\
                 <foreignObject width=\"1\" height=\"1\"><g id=\"x\"><rect width=\"5\" \
                 height=\"5\" fill=\"url(#B)\"/></g></foreignObject><rect width=\"100\" \
                 height=\"100\" fill=\"url(#A)\"/></svg>",
                pat("B", "C"),
                pat("C", "A")
            ),
        ),
    ];
    for (name, svg) in cases {
        let (r, elapsed) = run_big(&svg);
        assert_eq!(r.err(), Some(ProcessError::TooComplex(loops)), "{name}");
        assert!(elapsed < Duration::from_secs(1), "{name}: {elapsed:?}");
    }
    // A pattern kept inside a gradient, with no loop, is drawn as usual.
    let (r, _) = run_big(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><linearGradient \
         id=\"G\"><stop offset=\"0\"/><pattern id=\"P\" width=\"10\" height=\"10\" patternUnits=\"userSpaceOnUse\">\
         <rect width=\"5\" height=\"5\" fill=\"url(#G)\"/></pattern></linearGradient><rect \
         x=\"20\" y=\"20\" width=\"60\" height=\"60\" fill=\"url(#P)\"/></svg>",
    );
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn clip_and_mask_chains_over_the_cap_are_too_complex() {
    let defs = "too many clip paths, masks, patterns, gradients or filters";
    for kind in ["clip", "mask", "pattern"] {
        let (r, _) = run_big(&chain(kind, 2000, 0));
        assert_eq!(r.err(), Some(ProcessError::TooComplex(defs)), "{kind}");
    }
    // Within the caps, a 60-long clip chain is applied.
    let mut ok = format!("{SVG_OPEN}<defs>");
    for i in 0..60 {
        ok.push_str(&format!(
            "<clipPath id=\"c{i}\" clip-path=\"url(#c{})\"><rect width=\"100\" \
             height=\"50\"/></clipPath>",
            i + 1
        ));
    }
    ok.push_str("</defs><rect width=\"100\" height=\"100\" clip-path=\"url(#c0)\"/></svg>");
    let (r, _) = run_big(&ok);
    let shape = r.expect("clip chain");
    assert!(
        (shape.metrics().fill_pct - 50.0).abs() < 1.0,
        "{:?}",
        shape.metrics()
    );
}

#[test]
fn use_bombs_are_too_complex() {
    let expands = "references expand to too many shapes (<use>, patterns or markers)";
    // Exponential: 2^25 copies if expanded.
    let mut bomb = String::from(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\">\
         <defs><rect id=\"a0\" width=\"1\" height=\"1\"/>",
    );
    for i in 1..=25 {
        bomb.push_str(&format!(
            "<g id=\"a{i}\"><use xlink:href=\"#a{0}\"/><use xlink:href=\"#a{0}\"/></g>",
            i - 1
        ));
    }
    bomb.push_str("</defs><use xlink:href=\"#a25\"/></svg>");
    let (r, elapsed) = run_big(&bomb);
    assert_eq!(r.err(), Some(ProcessError::TooComplex(expands)));
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // Flat fan-out: 400 uses of a 100-element group.
    let mut fan = format!("{SVG_OPEN}<defs><g id=\"g\">");
    fan.push_str(&"<rect width=\"1\" height=\"1\"/>".repeat(100));
    fan.push_str("</g></defs>");
    fan.push_str(&"<use href=\"#g\"/>".repeat(400));
    fan.push_str("</svg>");
    assert_eq!(
        run_big(&fan).0.err(),
        Some(ProcessError::TooComplex(expands))
    );
    // Too many uses at all.
    let many = format!(
        "{SVG_OPEN}<rect id=\"r\" width=\"1\" height=\"1\"/>{}</svg>",
        "<use href=\"#r\"/>".repeat(501)
    );
    assert_eq!(
        run_big(&many).0.err(),
        Some(ProcessError::TooComplex("too many <use> elements"))
    );
    // Every path with an objectBoundingBox pattern gets its own copy of the pattern.
    let mut pat = format!("{SVG_OPEN}<defs><pattern id=\"p\" width=\"1\" height=\"1\">");
    pat.push_str(&"<rect width=\"1\" height=\"1\"/>".repeat(2000));
    pat.push_str("</pattern></defs>");
    pat.push_str(&"<rect width=\"9\" height=\"9\" fill=\"url(#p)\"/>".repeat(2000));
    pat.push_str("</svg>");
    assert_eq!(
        run_big(&pat).0.err(),
        Some(ProcessError::TooComplex(expands))
    );
    // And every vertex its own marker.
    let mut marker = format!("{SVG_OPEN}<marker id=\"m\" markerWidth=\"9\" markerHeight=\"9\">");
    marker.push_str(&"<rect width=\"1\" height=\"1\"/>".repeat(1000));
    marker.push_str("</marker><path marker-mid=\"url(#m)\" stroke=\"black\" d=\"M0 0");
    marker.push_str(&" 1 1".repeat(20_000));
    marker.push_str("\"/></svg>");
    assert_eq!(
        run_big(&marker).0.err(),
        Some(ProcessError::TooComplex(expands))
    );
}

#[test]
fn expansion_through_shapes_use_copies_and_arcs_is_too_complex_and_fast() {
    let expands = "references expand to too many shapes (<use>, patterns or markers)";
    // Markers on circles and rects: usvg draws them on every shape. Four levels of 10
    // reached 2.9 GB inside usvg and aborted.
    for shape in [
        "<circle cx=\"1\" cy=\"1\" r=\"1\"{m}/>",
        "<rect width=\"2\" height=\"2\"{m}/>",
    ] {
        let with = |m: String| shape.replace("{m}", &m);
        let mut svg = format!("{SVG_OPEN}<defs>");
        for i in 0..4 {
            let m = if i < 3 {
                format!(" style=\"marker:url(#M{})\"", i + 1)
            } else {
                String::new()
            };
            svg += &format!("<marker id=\"M{i}\">{}</marker>", with(m).repeat(10));
        }
        svg += &format!("</defs>{}</svg>", with(" style=\"marker:url(#M0)\"".into()));
        let (r, elapsed) = run_big(&svg);
        assert_eq!(r.err(), Some(ProcessError::TooComplex(expands)), "{shape}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
    // A fill inherited by `<use>` copies: every copied shape gets its own copy of an
    // objectBoundingBox pattern. 6 shapes, 8 levels: a 1 KB file that aborted. The same
    // with a decoy `xlink:href` before the `href` usvg copies reached 1 GB in 13 s.
    for link in [
        "<g fill=\"url(#{P})\"><use href=\"#grp\"/></g>",
        "<use xlink:href=\"#one\" href=\"#grp\" fill=\"url(#{P})\"/>",
    ] {
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\" viewBox=\"0 0 100 100\"><defs>\
             <rect id=\"one\" width=\"1\" height=\"1\"/><g id=\"grp\">{}</g>",
            "<rect width=\"1\" height=\"1\"/>".repeat(6)
        );
        for i in 0..8 {
            let content = if i < 7 {
                link.replace("{P}", &format!("P{}", i + 1))
            } else {
                "<use href=\"#grp\"/>".into()
            };
            svg += &format!("<pattern id=\"P{i}\" width=\"1\" height=\"1\">{content}</pattern>");
        }
        svg += "</defs><rect width=\"90\" height=\"90\" fill=\"url(#P0)\"/></svg>";
        let (r, elapsed) = run_big(&svg);
        assert_eq!(r.err(), Some(ProcessError::TooComplex(expands)), "{link}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
    // Copies of shapes the prescan must still weigh: a circle sized by a stylesheet
    // font size, or by a `<symbol>`'s own viewport, or kept in `<metadata>`; and a
    // polyline's points.
    let too_big = "a curve or circle is too big to draw (a huge radius)";
    let uses = "<use href=\"#c\"/>".repeat(20);
    for defs in [
        "<style>circle{font-size:3e37px}</style><defs><circle id=\"c\" r=\"1em\"/></defs>",
        "<defs><symbol id=\"c\"><circle r=\"50%\"/></symbol></defs>",
        "<metadata><circle id=\"c\" r=\"3e37\"/></metadata>",
    ] {
        let svg = format!("{SVG_OPEN}{defs}{uses}</svg>").replace(
            "<use href=\"#c\"/>",
            "<use href=\"#c\" width=\"3e37\" height=\"3e37\"/>",
        );
        let (r, elapsed) = run_big(&svg);
        assert_eq!(r.err(), Some(ProcessError::TooComplex(too_big)), "{defs}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
    let polyline = format!(
        "{SVG_OPEN}<defs><polyline id=\"p\" points=\"{}\"/></defs>{}</svg>",
        "1,1 ".repeat(5000),
        "<use href=\"#p\"/>".repeat(25)
    );
    assert_eq!(
        run_big(&polyline).0.err(),
        Some(ProcessError::TooComplex(expands))
    );
    // Arcs with huge radii: usvg would split them into billions of cubics.
    for d in ["M0 0A1e50 1e50 0 1 1 1e50 0Z", "M0 0A1e30 1e30 0 1 1 1 0Z"] {
        let (r, elapsed) = run_big(&format!("{SVG_OPEN}<path d=\"{d}\"/></svg>"));
        assert_eq!(r.err(), Some(ProcessError::TooComplex(too_big)), "{d}");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
    // Text copied by `<tref>`s.
    let tref = format!(
        "{SVG_OPEN}<g id=\"x\"><text>{}</text></g><text>{}</text></svg>",
        "A".repeat(100_000),
        "<tref href=\"#x\"/>".repeat(1_000)
    );
    assert_eq!(
        run_big(&tref).0.err(),
        Some(ProcessError::TooComplex("too much text is copied (<tref>)"))
    );
}

#[test]
fn long_runs_of_closes_and_arcs_are_too_complex() {
    // svgtypes' path simplifier calls itself once for every command that adds no segment
    // (a close after a close, an arc too small for kurbo to split): 200,000 closes in a
    // row, a 200 KB file, overflowed the 64 MiB stack and aborted.
    let in_a_row = "a path has too many closes or arcs in a row";
    let path = |tail: &str| format!("{SVG_OPEN}<path d=\"M10 10L90 10L50 90Z{tail}\"/></svg>");
    for (name, tail) in [
        ("200,000 closes", "Z".repeat(200_000)),
        (
            "25,000 tiny arcs",
            format!("M0 0{}", " a1 1 0 0 0 1e-20 0".repeat(25_000)),
        ),
    ] {
        let (r, elapsed) = run_big(&path(&tail));
        assert_eq!(r.err(), Some(ProcessError::TooComplex(in_a_row)), "{name}");
        assert!(elapsed < Duration::from_secs(1), "{name}: {elapsed:?}");
    }
    // The most the limit lets through, on the production stack: the drawing, then 1,000
    // closes in a row (999 add nothing), then 1,000 tiny arcs that add nothing.
    let (r, _) = run_big(&path(&format!(
        "{}M0 0{}",
        "Z".repeat(999),
        " a1 1 0 0 0 1e-20 0".repeat(1000)
    )));
    let shape = r.expect("within the limit");
    assert!(filled_at(&shape, 50.0, 30.0) && !filled_at(&shape, 20.0, 60.0));
}

#[test]
fn css_that_backtracks_is_too_complex_and_fast() {
    // simplecss tries every ancestor for every descendant combinator; usvg matches every
    // rule against every element. Ten combinators over 60 nested groups would be
    // ~10^12 steps.
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>x g g g g g g g g g g \
         {{fill:red}}</style>{}<rect width=\"9\" height=\"9\"/>{}</svg>",
        "<g>".repeat(60),
        "</g>".repeat(60)
    );
    let (r, elapsed) = run_big(&svg);
    assert_eq!(
        r.err(),
        Some(ProcessError::TooComplex("the CSS is too complex"))
    );
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // Thousands of simple rules against thousands of elements.
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>{}</style>{}</svg>",
        (0..10_000)
            .map(|i| format!(".c{i}{{fill:red}}"))
            .collect::<String>(),
        "<rect class=\"c1\" width=\"9\" height=\"9\"/>".repeat(3_000)
    );
    let (r, elapsed) = run_big(&svg);
    assert_eq!(
        r.err(),
        Some(ProcessError::TooComplex("the CSS is too complex"))
    );
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // A long `style` attribute on an element inside one usvg never parses (we still read
    // its references): 125,000 declarations took 25 s of CPU and were accepted.
    for (open, close) in [
        ("<metadata>", "</metadata>"),
        ("<title>", "</title>"),
        ("<foreignObject>", "</foreignObject>"),
    ] {
        let svg = format!(
            "{SVG_OPEN}{open}<g style=\"{}\"/>{close}<rect width=\"50\" height=\"50\"/></svg>",
            "a:b;".repeat(125_000)
        );
        let (r, elapsed) = run_big(&svg);
        assert_eq!(
            r.err(),
            Some(ProcessError::TooComplex("the CSS is too complex")),
            "{open}"
        );
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
}

#[test]
fn many_filter_primitives_are_ignored_quickly() {
    let mut svg =
        format!("{SVG_OPEN}<defs><filter id=\"f\" x=\"-10\" y=\"-10\" width=\"20\" height=\"20\">");
    for _ in 0..2000 {
        svg.push_str("<feMorphology operator=\"dilate\" radius=\"50\"/><feGaussianBlur stdDeviation=\"30\"/>");
    }
    svg.push_str("</filter></defs><rect width=\"60\" height=\"100\" filter=\"url(#f)\"/></svg>");
    let (r, elapsed) = run_big(&svg);
    let shape = r.expect("filters are ignored");
    assert_eq!(info(&shape), vec!["filters_ignored"]);
    assert!((shape.metrics().fill_pct - 60.0).abs() < 1.0);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn nested_oversized_masks_are_painted_unmasked() {
    // 1.5 KB that took resvg to 949 MB: each mask allocates a layer five times the
    // canvas. The painter ignores masks, so this is a plain rectangle.
    let mut svg = format!("{SVG_OPEN}<defs>");
    for i in 0..8 {
        let inner = if i < 7 {
            format!(" mask=\"url(#m{})\"", i + 1)
        } else {
            String::new()
        };
        svg.push_str(&format!(
            "<mask id=\"m{i}\" x=\"-50\" y=\"-50\" width=\"100\" height=\"100\"><rect \
             x=\"-500\" y=\"-500\" width=\"1000\" height=\"1000\" fill=\"white\"{inner}/></mask>"
        ));
    }
    svg.push_str("</defs><rect width=\"100\" height=\"100\" mask=\"url(#m0)\"/></svg>");
    let (r, elapsed) = run_big(&svg);
    let shape = r.expect("masks are ignored");
    assert_eq!(
        shape.info(),
        vec![Lint::ClipOrMask {
            clipped: false,
            masked: true
        }]
    );
    assert!(shape.metrics().fill_pct > 99.0);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
}

#[test]
fn other_bombs_are_too_complex() {
    // Node limit: 25k elements. Valid XML, so the advice is to simplify, not re-export.
    let many = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
        "<path/>".repeat(25_000)
    );
    let e = run_big(&many).0.expect_err("too many nodes");
    assert_eq!(
        e,
        ProcessError::TooComplex("too many elements (shapes, groups, text and comments)")
    );
    assert!(
        !e.user_message().contains("valid XML"),
        "{}",
        e.user_message()
    );
    // Path segments: the rasteriser's cost grows with them.
    let segments = format!(
        "{SVG_OPEN}<path d=\"M0 0{}Z\"/></svg>",
        " L100 100 L0 1".repeat(3000)
    );
    assert_eq!(
        run_big(&segments).0.err(),
        Some(ProcessError::TooComplex("too many path segments"))
    );
    // Rasterising work: 4,990 segments (under the segment cap) that each cross most of
    // the drawing. Before the travel budget this took 350 ms in release.
    let mut seed = 7u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 10_000) as f32 / 100.0
    };
    let mut d = String::from("M0 0");
    for _ in 0..4990 {
        d.push_str(&format!(" L{} {}", rnd(), rnd()));
    }
    let (r, elapsed) = run_big(&format!("{SVG_OPEN}<path d=\"{d}Z\"/></svg>"));
    assert_eq!(
        r.err(),
        Some(ProcessError::TooComplex(
            "the outlines are too long to draw (lines crossing the whole drawing)"
        ))
    );
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // Dashes: 0.001-unit dashes along a long outline.
    let dashes = format!(
        "{SVG_OPEN}<path d=\"M0 0H100V100H0Z\" stroke=\"black\" stroke-width=\"2\" \
         stroke-dasharray=\"0.001\" fill=\"none\"/></svg>"
    );
    let (r, elapsed) = run_big(&dashes);
    assert_eq!(
        r.err(),
        Some(ProcessError::TooComplex("dashed outlines are too fine"))
    );
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // Clip masks: each is a full-size raster.
    let mut clips = format!(
        "{SVG_OPEN}<defs><clipPath id=\"c\"><rect width=\"90\" height=\"90\"/></clipPath></defs>"
    );
    clips.push_str(&"<g clip-path=\"url(#c)\">".repeat(20));
    clips.push_str("<rect width=\"100\" height=\"100\"/>");
    clips.push_str(&"</g>".repeat(20));
    clips.push_str("</svg>");
    assert_eq!(
        run_big(&clips).0.err(),
        Some(ProcessError::TooComplex("too many clipping paths"))
    );
}

#[test]
fn junk_is_rejected_cleanly() {
    // Not UTF-8 (Latin-1 é in a comment).
    let mut latin1 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><svg xmlns=\"http://www.w3.org/2000/svg\"><!-- caf".to_vec();
    latin1.extend_from_slice(b"\xe9 --><rect width=\"9\" height=\"9\"/></svg>");
    let e = run_bytes(&latin1, &[]).expect_err("latin-1");
    assert!(matches!(e, ProcessError::InvalidSvg(_)), "{e:?}");
    assert_eq!(e.code(), "invalid_svg");
    // Illustrator's ISO-8859-1 encoding option: the advice says how to save as UTF-8.
    assert!(e.user_message().contains("UTF-8"), "{}", e.user_message());
    // 600 KiB.
    let mut big = format!("{SVG_OPEN}<rect width=\"9\" height=\"9\"/>");
    big.push_str(&" ".repeat(600 * 1024));
    big.push_str("</svg>");
    assert_eq!(
        run(&big).err(),
        Some(ProcessError::TooLarge {
            bytes: big.len(),
            max: 512 * 1024
        })
    );
    // Compressed (.svgz) and UTF-16 SVGs are recognised, with their own advice.
    let svgz = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03];
    assert_eq!(
        run_bytes(&svgz, &[]).err(),
        Some(ProcessError::UnsupportedFormat(RejectedFormat::Svgz))
    );
    let utf16: Vec<u8> = format!("\u{feff}{SVG_OPEN}<rect width=\"9\" height=\"9\"/></svg>")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(
        run_bytes(&utf16, &[]).err(),
        Some(ProcessError::UnsupportedFormat(RejectedFormat::Utf16))
    );
    // An HTML document that mentions <svg> isn't an SVG.
    assert!(matches!(
        run("<html><body><svg/></body></html>"),
        Err(ProcessError::InvalidSvg(_))
    ));
    // Absurd numbers don't panic.
    for svg in [
        format!("{SVG_OPEN}<path d=\"M1e38 1e38L-1e38 5L5 -1e38Z\"/><rect width=\"NaN\" height=\"1e999\"/></svg>"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 0 0\"><rect width=\"10\" height=\"10\"/></svg>".into(),
        format!("{SVG_OPEN}<circle r=\"1e30\" stroke=\"black\" stroke-width=\"1e30\"/></svg>"),
    ] {
        let r = run(&svg);
        assert!(!matches!(r, Err(ProcessError::Internal(_))), "{svg}: {r:?}");
    }
}

// ---------------------------------------------------------------------------------------
// The big-stack runner
// ---------------------------------------------------------------------------------------

struct Permit(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn the_permit_is_released_when_the_work_ends() {
    for svg in [
        catalog_svg(AssetKind::Head, "smile"),
        "not an svg".to_string(),
    ] {
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rx = process_on_big_stack(
            svg.into_bytes(),
            &Limits::default(),
            &[Fix::Flip],
            Permit(released.clone()),
        );
        let r = rx.blocking_recv().expect("a result");
        // Released by the time the result arrives. (That it is held until the work ends,
        // even when the caller stops waiting, is a unit test in design_kit/mod.rs.)
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
        if let Ok(shape) = r {
            assert!(codes(&shape.lints().head).contains(&"faces_left"));
        }
    }
}

#[test]
fn raster_worst_cases_also_run_on_the_big_stack() {
    // The same answers as on the caller's thread (design_kit_raster.rs).
    let side = 1024u32;
    let mut rng = Rng(12345);
    let mut noise = Vec::with_capacity((side * side * 4) as usize);
    for _ in 0..side * side {
        noise.extend_from_slice(&[0, 0, 0, if rng.next().is_multiple_of(2) { 255 } else { 0 }]);
    }
    let noise = encode_png(side, side, png::ColorType::Rgba, &noise);
    let mut stripes = Vec::with_capacity(1024 * 1024);
    for y in 0..1024usize {
        for x in 0..1024usize {
            stripes.push(if (x + y) % 4 < 2 { 0 } else { 255 });
        }
    }
    let stripes = encode_png(1024, 1024, png::ColorType::Grayscale, &stripes);
    let too_long = Some(ProcessError::TooComplex("the outline is too long"));
    for (name, bytes) in [("noise", noise), ("stripes", stripes)] {
        let t = Instant::now();
        let r = process_on_big_stack(bytes, &Limits::default(), &[], ())
            .blocking_recv()
            .expect("a result");
        println!("{name}: {:?} in {:?}", r.as_ref().err(), t.elapsed());
        assert_eq!(r.err(), too_long, "{name}");
    }
    let big = svg_to_png(&catalog_svg(AssetKind::Head, "bendr"), 2048);
    let direct = process_upload(&big, &Limits::default(), &[]).expect("a 2048 px head");
    let shape = process_on_big_stack(big, &Limits::default(), &[], ())
        .blocking_recv()
        .expect("a result")
        .expect("a 2048 px head");
    assert_eq!(shape.strategy(), Strategy::Traced);
    assert_eq!(shape.path_d(), direct.path_d());
    assert!(shape.passes(AssetKind::Head), "{:?}", shape.lints());
}

// ---------------------------------------------------------------------------------------
// The prescan models these crates' sources
// ---------------------------------------------------------------------------------------

#[test]
fn svg_parsing_crates_are_the_audited_versions() {
    // svg_scan.rs mirrors usvg's reference following, simplecss's selector matching,
    // svgtypes' IRI and path parsing, kurbo's arc splitting and roxmltree's tokenizer at
    // exactly these versions; a reference loop it misses aborts the process. Cargo.toml
    // pins usvg, simplecss and svgtypes exactly; this also catches a second copy, or a
    // bump of the crates usvg pulls in.
    // After re-checking svg_scan.rs against a new version's sources, update it here.
    let lock = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock"),
    )
    .expect("the workspace Cargo.lock");
    for (name, audited) in [
        ("usvg", "0.48.1"),
        ("simplecss", "0.2.2"),
        ("roxmltree", "0.21.1"),
        ("svgtypes", "0.16.1"),
        ("kurbo", "0.13.1"),
    ] {
        let versions: Vec<&str> = lock
            .split("[[package]]")
            .filter(|p| p.contains(&format!("\nname = \"{name}\"\n")))
            .filter_map(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .map(|v| v.trim_matches('"'))
            .collect();
        assert_eq!(
            versions,
            vec![audited],
            "{name}: re-check src/design_kit/svg_scan.rs against the new version"
        );
    }
}
