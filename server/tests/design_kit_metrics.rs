//! Anchors the metric implementation to an independent oracle.
//!
//! The lint thresholds were derived from rsvg renders of the official catalog
//! (`catalog/metrics_summary.csv`, from the DEV-1539 investigation). Here the same assets
//! are rendered with resvg at the same 200 px and measured with `Metrics::from_alpha`;
//! fill% and every edge% must agree with the CSV within 2 points.

mod common;

use arena::design_kit::{AssetKind, METRIC_SIDE, Metrics};
use common::design_kit::*;

struct Row {
    kind: AssetKind,
    slug: String,
    cov: f32,
    edges: [f32; 4],
}

fn oracle() -> Vec<Row> {
    let csv = String::from_utf8(fixture("catalog/metrics_summary.csv")).expect("utf-8");
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().expect("header").split(',').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (kind, slug, cov) = (col("kind"), col("slug"), col("cov"));
    let edges = [col("edgeL1"), col("edgeR1"), col("edgeT1"), col("edgeB1")];
    lines
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let num = |i: usize| f[i].parse::<f32>().expect("number") * 100.0;
            Row {
                kind: if f[kind] == "head" {
                    AssetKind::Head
                } else {
                    AssetKind::Tail
                },
                slug: f[slug].to_string(),
                cov: num(cov),
                edges: edges.map(num),
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
    for row in &rows {
        let svg = catalog_svg(row.kind, &row.slug);
        let m = Metrics::from_alpha(&alpha(&svg, METRIC_SIDE), METRIC_SIDE as usize);
        let ours = [
            ("fill", m.fill_pct, row.cov),
            ("left", m.left_edge_pct, row.edges[0]),
            ("right", m.right_edge_pct, row.edges[1]),
            ("top", m.top_edge_pct, row.edges[2]),
            ("bottom", m.bottom_edge_pct, row.edges[3]),
        ];
        for (what, got, want) in ours {
            println!(
                "{}/{} {what}: ours {got:.2} oracle {want:.2} (diff {:+.2})",
                kind_dir(row.kind),
                row.slug,
                got - want
            );
            if (got - want).abs() > 2.0 {
                failures.push(format!(
                    "{}/{} {what}: ours {got:.2} vs oracle {want:.2}",
                    kind_dir(row.kind),
                    row.slug
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
