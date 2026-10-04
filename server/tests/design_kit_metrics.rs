//! Anchors the metric implementation to an independent oracle.
//!
//! The lint thresholds were derived from rsvg renders of the official catalog
//! (`catalog/metrics_detail.csv`, from the DEV-1539 investigation: fill and edges from
//! `metrics.py`, centroid and hole fraction as in `lint2.py` but at pixel centres, bounds
//! from `metrics.json`). Here the same assets are rendered with resvg at the same 200 px
//! and measured with `Metrics::from_alpha`. Every metric a lint reads must agree:
//! fill% and edge% within 2 points, the centroid within 0.5 units, the hole fraction
//! within 1 point and the bounds within 1 unit.
//!
//! The hole fraction in the CSV fills holes with scipy's default 4-connected structure,
//! the same connectivity `Metrics` uses.

mod common;

use arena::design_kit::{AssetKind, METRIC_SIDE, Metrics};
use common::design_kit::*;

struct Row {
    kind: AssetKind,
    slug: String,
    /// Percentages: fill, then edges left, right, top, bottom.
    fill_and_edges: [f32; 5],
    centroid: [f32; 2],
    hole_pct: f32,
    bbox: [f32; 4],
}

fn oracle() -> Vec<Row> {
    let csv = String::from_utf8(fixture("catalog/metrics_detail.csv")).expect("utf-8");
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().expect("header").split(',').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (kind, slug) = (col("kind"), col("slug"));
    let shares = ["cov", "edgeL1", "edgeR1", "edgeT1", "edgeB1"].map(col);
    let centroid = ["cx", "cy"].map(col);
    let holefrac = col("holefrac");
    let bbox = ["bbox_x0", "bbox_y0", "bbox_x1", "bbox_y1"].map(col);
    lines
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let num = |i: usize| f[i].parse::<f32>().expect("number");
            Row {
                kind: if f[kind] == "head" {
                    AssetKind::Head
                } else {
                    AssetKind::Tail
                },
                slug: f[slug].to_string(),
                fill_and_edges: shares.map(|i| num(i) * 100.0),
                centroid: centroid.map(num),
                hole_pct: num(holefrac) * 100.0,
                bbox: bbox.map(num),
            }
        })
        .collect()
}

#[test]
fn metrics_match_the_rsvg_catalog_oracle() {
    let rows = oracle();
    assert_eq!(
        rows.len(),
        HEADS.len() + TAILS.len(),
        "one CSV row per sample"
    );
    let mut failures = Vec::new();
    let mut checked = 0;
    for row in &rows {
        let svg = catalog_svg(row.kind, &row.slug);
        let m = Metrics::from_alpha(&alpha(&svg, METRIC_SIDE), METRIC_SIDE as usize);
        let centroid = m.centroid.expect("centroid");
        let bbox = m.bbox.expect("bbox");
        let f = row.fill_and_edges;
        let ours = [
            ("fill%", m.fill_pct, f[0], 2.0),
            ("left edge%", m.left_edge_pct, f[1], 2.0),
            ("right edge%", m.right_edge_pct, f[2], 2.0),
            ("top edge%", m.top_edge_pct, f[3], 2.0),
            ("bottom edge%", m.bottom_edge_pct, f[4], 2.0),
            ("centroid x", centroid[0], row.centroid[0], 0.5),
            ("centroid y", centroid[1], row.centroid[1], 0.5),
            ("hole%", m.hole_pct, row.hole_pct, 1.0),
            ("bbox x0", bbox[0], row.bbox[0], 1.0),
            ("bbox y0", bbox[1], row.bbox[1], 1.0),
            ("bbox x1", bbox[2], row.bbox[2], 1.0),
            ("bbox y1", bbox[3], row.bbox[3], 1.0),
        ];
        for (what, got, want, tolerance) in ours {
            checked += 1;
            println!(
                "{}/{} {what}: ours {got:.2} oracle {want:.2} (diff {:+.2})",
                kind_dir(row.kind),
                row.slug,
                got - want
            );
            if (got - want).abs() > tolerance {
                failures.push(format!(
                    "{}/{} {what}: ours {got:.2} vs oracle {want:.2}",
                    kind_dir(row.kind),
                    row.slug
                ));
            }
        }
    }
    assert_eq!(checked, 12 * 12);
    assert!(failures.is_empty(), "{failures:#?}");
}
