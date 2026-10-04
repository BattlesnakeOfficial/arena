//! The downloadable design kit in `server/static/design-kit/` (written by
//! `scripts/design-kit/generate.py`, committed) against the studio's template contract:
//! the template colours are exactly `design_kit::palette`, the layer ids the SVG filter
//! looks for exist, the test fixtures are copies of the committed files, and the "Try an
//! example" drawing passes as a head.

mod common;

use std::collections::BTreeSet;

use arena::design_kit::{
    AssetKind, Limits, ProcessError, RejectedFormat,
    palette::{self, Rgb},
    process_upload, sniff,
};
use common::design_kit::fixture;
use usvg::roxmltree;

fn design_kit_file(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("static/design-kit")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

const KINDS: [&str; 2] = ["head", "tail"];

fn hex(c: Rgb) -> String {
    c.to_hex()
}

/// Every `#rrggbb` an element paints with: `fill`, `stroke`, and colours in `style`.
fn paints(node: roxmltree::Node) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["fill", "stroke"] {
        if let Some(v) = node.attribute(attr) {
            out.push(v.to_ascii_lowercase());
        }
    }
    if let Some(style) = node.attribute("style") {
        for decl in style.split(';') {
            if let Some((k, v)) = decl.split_once(':')
                && matches!(k.trim(), "fill" | "stroke" | "color")
            {
                out.push(v.trim().to_ascii_lowercase());
            }
        }
    }
    out
}

/// The colours painted inside `layer` (`none` excluded).
fn layer_colours(layer: roxmltree::Node) -> BTreeSet<String> {
    layer
        .descendants()
        .flat_map(paints)
        .filter(|c| c != "none")
        .collect()
}

#[test]
fn the_svg_templates_use_exactly_the_palette_and_ids() {
    let guides: BTreeSet<String> = palette::GUIDE_COLOURS.into_iter().map(hex).collect();
    let ghost: BTreeSet<String> = [hex(palette::REFERENCE_GHOST)].into();
    assert_eq!(guides.len(), 5, "five distinct guide colours");
    assert!(!guides.contains(&hex(palette::REFERENCE_GHOST)));

    for kind in KINDS {
        let name = format!("battlesnake-{kind}-template.svg");
        let bytes = design_kit_file(&name);
        let text = String::from_utf8(bytes).expect("UTF-8");
        let doc = roxmltree::Document::parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let root = doc.root_element();
        assert_eq!(root.attribute("viewBox"), Some("0 0 100 100"), "{name}");

        // The layers are top-level groups.
        let layers: Vec<_> = root
            .children()
            .filter(|n| n.has_tag_name("g"))
            .map(|g| (g.attribute("id").unwrap_or_default().to_string(), g))
            .collect();
        let ids: Vec<&str> = layers.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"draw-here"), "{name}: {ids:?}");
        assert!(ids.contains(&"guides"), "{name}: {ids:?}");
        assert!(
            ids.iter().filter(|id| id.starts_with("reference-")).count() >= 1,
            "{name}: {ids:?}"
        );
        assert!(ids.contains(&"reference-default"), "{name}: {ids:?}");
        assert!(
            ids.iter()
                .all(|id| *id == "draw-here" || *id == "guides" || id.starts_with("reference-")),
            "{name}: unexpected layer in {ids:?}"
        );

        for (id, layer) in &layers {
            let used = layer_colours(*layer);
            match id.as_str() {
                // Empty, filled black: where the artist draws.
                "draw-here" => {
                    assert_eq!(layer.attribute("fill"), Some("#000000"), "{name}");
                    assert_eq!(layer.children().filter(|n| n.is_element()).count(), 0);
                }
                // Every guide colour, and nothing else (no white halo, no black).
                "guides" => {
                    assert_eq!(used, guides, "{name}: guides layer colours");
                    let style = layer.attribute("style").unwrap_or_default();
                    assert!(style.contains("mix-blend-mode:multiply"), "{name}: {style}");
                }
                // The ghost colour only, and hidden until the artist turns it on.
                _ => {
                    assert_eq!(used, ghost, "{name}: {id} colours");
                    let style = layer.attribute("style").unwrap_or_default();
                    assert!(style.contains("display:none"), "{name}: {id} {style}");
                }
            }
        }
    }
}

#[test]
fn an_untouched_svg_template_is_empty_with_the_guides_hint() {
    for kind in KINDS {
        let svg = design_kit_file(&format!("battlesnake-{kind}-template.svg"));
        let e = process_upload(&svg, &Limits::default(), &[]).expect_err("nothing drawn");
        assert!(matches!(e, ProcessError::Empty { .. }), "{kind}: {e:?}");
    }
}

#[test]
fn the_psd_templates_are_turned_away_with_advice() {
    for kind in KINDS {
        let psd = design_kit_file(&format!("battlesnake-{kind}-template.psd"));
        assert!(psd.starts_with(b"8BPS"), "{kind}: a PSD");
        assert!(psd.len() < 2 * 1024 * 1024, "{kind}: {} B", psd.len());
        assert_eq!(
            sniff(&psd),
            Err(ProcessError::UnsupportedFormat(RejectedFormat::Psd)),
            "{kind}: uploading the template itself explains what to do"
        );
    }
}

#[test]
fn the_test_fixtures_are_the_committed_templates() {
    // tests/fixtures/design_kit/template/ holds copies, so the raster and SVG tests run
    // against exactly what artists download. Regenerate, then copy them again.
    for (fixture_name, file) in [
        ("template/head-guide.png", "battlesnake-head-guide.png"),
        ("template/tail-guide.png", "battlesnake-tail-guide.png"),
        (
            "template/head-template.svg",
            "battlesnake-head-template.svg",
        ),
    ] {
        assert!(
            fixture(fixture_name) == design_kit_file(file),
            "tests/fixtures/design_kit/{fixture_name} differs from static/design-kit/{file}"
        );
    }
}

#[test]
fn the_example_drawing_is_a_clean_head() {
    let png = design_kit_file("example-drawing.png");
    assert!(png.len() <= 200 * 1024, "{} B", png.len());
    let shape = process_upload(&png, &Limits::default(), &[]).expect("the example processes");
    assert!(
        shape.lints().head.is_empty(),
        "no head warnings: {:?}",
        shape.lints().head
    );
    assert!(shape.passes(AssetKind::Head));
    assert!(shape.info().is_empty(), "no notes: {:?}", shape.info());
}
