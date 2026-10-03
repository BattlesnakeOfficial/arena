//! design_kit SVG input: real-world exports, the template filter, input-fact lints,
//! hostile files with exact outcomes, and the big-stack runner.
//!
//! Hostile inputs that could recurse deeply inside usvg are run through
//! `process_on_big_stack`, as production does: on a 2 MiB test thread some of them
//! would overflow the stack and abort the whole test binary.

mod common;

use std::time::{Duration, Instant};

use arena::design_kit::{
    AssetKind, CleanShape, FillRule, Fix, InputFormat, Limits, Lint, ProcessError, Severity,
    Strategy, process_on_big_stack, process_upload,
};
use common::design_kit::*;

fn run(svg: &str) -> Result<CleanShape, ProcessError> {
    let r = process_upload(svg.as_bytes(), &Limits::default(), &[]);
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
fn a_figma_frame_clip_is_applied() {
    let svg = r##"<svg width="100" height="100" viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
      <g clip-path="url(#clip0_1_2)"><rect x="-20" y="-20" width="140" height="140" fill="#000"/>
      <circle cx="50" cy="50" r="10" fill="#000"/></g>
      <defs><clipPath id="clip0_1_2"><rect width="100" height="60" fill="white"/></clipPath></defs></svg>"##;
    let shape = run(svg).expect("figma");
    assert_eq!(info(&shape), vec!["clip_or_mask"]);
    assert!(filled_at(&shape, 50.0, 30.0));
    assert!(!filled_at(&shape, 50.0, 80.0), "the clipped area is empty");
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
    assert_eq!(
        run(&format!(
            "{SVG_OPEN}<text x=\"0\" y=\"50\" font-size=\"80\">S</text></svg>"
        ))
        .err(),
        Some(ProcessError::Empty {
            info: vec![Lint::TextIgnored]
        })
    );
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
    let fixed =
        process_upload(mirrored.as_bytes(), &Limits::default(), &[Fix::Flip]).expect("flipped");
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

#[test]
fn entity_declarations_are_invalid_xml() {
    let xxe = r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><svg xmlns="http://www.w3.org/2000/svg"><text>&xxe;</text><rect width="10" height="10"/></svg>"#;
    assert_eq!(
        run(xxe).err(),
        Some(ProcessError::InvalidXml(
            "entity declarations are not allowed".into()
        ))
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
    assert_eq!(
        run(&lol).err(),
        Some(ProcessError::InvalidXml(
            "entity declarations are not allowed".into()
        ))
    );
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
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
    ];
    for (name, svg) in cases {
        let (r, elapsed) = run_big(&svg);
        assert_eq!(r.err(), Some(ProcessError::TooComplex(loops)), "{name}");
        assert!(elapsed < Duration::from_secs(1), "{name}: {elapsed:?}");
    }
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
    assert_eq!(info(&shape), vec!["clip_or_mask"]);
    assert!(shape.metrics().fill_pct > 99.0);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
}

#[test]
fn other_bombs_are_too_complex() {
    // Node limit: 25k elements.
    let many = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
        "<path/>".repeat(25_000)
    );
    assert!(matches!(run_big(&many).0, Err(ProcessError::InvalidXml(_))));
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
    let e = process_upload(&latin1, &Limits::default(), &[]).expect_err("latin-1");
    assert!(matches!(e, ProcessError::InvalidSvg(_)), "{e:?}");
    assert_eq!(e.code(), "invalid_svg");
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
        // Dropped before the result was sent.
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
