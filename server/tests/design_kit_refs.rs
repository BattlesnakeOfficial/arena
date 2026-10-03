//! `design_kit::refs` is a static table generated from the raw catalog SVGs vendored in
//! `fixtures/design_kit/refs/`. This checks it against fresh processing, so a pipeline
//! change that moves a reference shape fails here until the table is regenerated:
//!
//! ```text
//! cargo test -p arena --test design_kit_refs -- --ignored --nocapture print_refs_table
//! ```
//!
//! and paste the output between the `@generated` markers in `refs.rs`.

mod common;

use arena::design_kit::refs::{REFS, RefShape, find, of_kind};
use arena::design_kit::{AssetKind, CleanShape, Limits, Strategy, process_upload};
use common::design_kit::*;

/// The reference shapes, in table order, with their catalog display names
/// (`customizations::catalog`; a unit test there checks the table's names).
const SHAPES: [(AssetKind, &str, &str); 18] = [
    (AssetKind::Head, "default", "Default"),
    (AssetKind::Head, "beluga", "Beluga"),
    (AssetKind::Head, "bendr", "Bendr"),
    (AssetKind::Head, "evil", "Evil"),
    (AssetKind::Head, "fang", "Fang"),
    (AssetKind::Head, "smile", "Smile"),
    (AssetKind::Head, "pixel", "Pixel"),
    (AssetKind::Head, "sand-worm", "Sand Worm"),
    (AssetKind::Head, "tongue", "Tongue"),
    (AssetKind::Tail, "default", "Default"),
    (AssetKind::Tail, "curled", "Curled"),
    (AssetKind::Tail, "bolt", "Bolt"),
    (AssetKind::Tail, "round-bum", "Round Bum"),
    (AssetKind::Tail, "hook", "Hook"),
    (AssetKind::Tail, "block-bum", "Block Bum"),
    (AssetKind::Tail, "sharp", "Sharp"),
    (AssetKind::Tail, "pixel", "Pixel"),
    (AssetKind::Tail, "freckled", "Freckled"),
];

fn processed(kind: AssetKind, slug: &str) -> CleanShape {
    let bytes = fixture(&format!("refs/{}/{slug}.svg", kind_dir(kind)));
    let shape = process_upload(&bytes, &Limits::default(), &[])
        .unwrap_or_else(|e| panic!("{}/{slug}: {e:?}", kind_dir(kind)));
    assert_clean(&shape);
    shape
}

#[test]
fn refs_table_matches_fresh_processing() {
    assert_eq!(REFS.len(), SHAPES.len(), "regenerate refs.rs");
    let mut problems = Vec::new();
    for (r, &(kind, slug, name)) in REFS.iter().zip(&SHAPES) {
        let what = format!("{}/{slug}", kind_dir(kind));
        assert_eq!(
            (r.kind, r.slug, r.display_name),
            (kind, slug, name),
            "table order"
        );
        let shape = processed(kind, slug);
        if shape.strategy() != Strategy::VectorExact {
            problems.push(format!("{what}: {:?}", shape.strategy()));
        }
        if !shape.passes(kind) {
            problems.push(format!(
                "{what}: warnings {:?}",
                codes(shape.lints().for_kind(kind))
            ));
        }
        if r.d != shape.path_d() || r.fill_rule != shape.fill_rule() {
            problems.push(format!("{what}: the table differs from fresh output"));
        }
        // The table's path is the shape: rendered, it matches the source.
        let source = String::from_utf8(fixture(&format!("refs/{}/{slug}.svg", kind_dir(kind))))
            .expect("utf-8");
        let table_svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><path \
             fill-rule=\"{}\" d=\"{}\"/></svg>",
            r.fill_rule.as_svg(),
            r.d
        );
        let score = iou(&alpha(&source, 400), &alpha(&table_svg, 400));
        if score < 0.995 {
            problems.push(format!("{what}: IoU {score:.4} against the source"));
        }
    }
    assert!(
        problems.is_empty(),
        "{problems:#?}\nregenerate with print_refs_table"
    );
}

#[test]
fn lookups() {
    assert_eq!(of_kind(AssetKind::Head).count(), 9);
    assert_eq!(of_kind(AssetKind::Tail).count(), 9);
    let pixel = find(AssetKind::Tail, "pixel").expect("pixel tail");
    assert_eq!((pixel.kind, pixel.display_name), (AssetKind::Tail, "Pixel"));
    assert!(find(AssetKind::Head, "freckled").is_none());
    for r in REFS {
        assert!(
            r.d.chars()
                .all(|c| matches!(c, 'M' | 'L' | 'Q' | 'C' | 'Z' | '0'..='9' | '.' | '-' | ' ')),
            "{}",
            r.slug
        );
    }
}

/// Prints the table body for `refs.rs`.
#[test]
#[ignore]
fn print_refs_table() {
    let mut out = String::new();
    for (kind, slug, name) in SHAPES {
        let shape = processed(kind, slug);
        let r = RefShape {
            slug,
            kind,
            display_name: name,
            d: "",
            fill_rule: shape.fill_rule(),
        };
        out.push_str(&format!(
            "    RefShape {{\n        slug: {:?},\n        kind: AssetKind::{:?},\n        \
             display_name: {:?},\n        d: {:?},\n        fill_rule: FillRule::{:?},\n    }},\n",
            r.slug,
            r.kind,
            r.display_name,
            shape.path_d(),
            r.fill_rule
        ));
    }
    println!("{out}");
}
