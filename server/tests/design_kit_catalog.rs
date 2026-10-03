//! The official catalog as a corpus: every one of the 184 head and tail SVGs, uploaded
//! as is, must come out clean.
//!
//! Per file:
//! * processing succeeds (as an artist's upload, through `process_upload`);
//! * IoU ≥ 0.99 against an independent render: resvg (a dev-dependency, never used at
//!   runtime) draws the original inside the board's wrapper in black, and near-white
//!   pixels count as cut-outs (how multi-colour assets draw eyes);
//! * for single-colour files, fill% and left-edge% within 2 points of
//!   `catalog/metrics_summary.csv` (rsvg renders from the DEV-1539 investigation, an
//!   oracle independent of both resvg and our metrics);
//! * no warn-level lint for its kind (the lint thresholds were set outside the
//!   catalog's range; this is the check that they stay there through the SVG pipeline),
//!   except the one documented in [`EXCEPTIONS`], which must fire exactly as listed.
//!
//! Failures are listed by slug. The files are processed in chunks across threads.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use arena::design_kit::{AssetKind, CleanShape, Limits, Severity, Strategy, process_upload};
use common::design_kit::*;

/// One row of the investigation's `metrics_summary.csv`.
struct Row {
    file: String,
    /// Coverage (0..1) of everything painted, snake colour or not.
    cov: f32,
    /// Coverage of explicitly coloured paint: non-zero means a multi-colour asset.
    cov_fixed: f32,
    /// Share of rows whose leftmost 1% strip is at least half filled.
    edge_left: f32,
}

fn summary() -> Vec<Row> {
    let csv = String::from_utf8(fixture("catalog/metrics_summary.csv")).expect("utf-8");
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().expect("header").split(',').collect();
    let col = |name: &str| {
        header
            .iter()
            .position(|h| *h == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (file, cov, cov_fixed, edge_left) =
        (col("file"), col("cov"), col("cov_fixed"), col("edgeL1"));
    lines
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            let num = |i: usize| f[i].parse::<f32>().expect("number");
            Row {
                file: f[file].to_string(),
                cov: num(cov),
                cov_fixed: num(cov_fixed),
                edge_left: num(edge_left),
            }
        })
        .collect()
}

/// Catalog assets that legitimately trip a warning once converted to one colour, with
/// the exact warnings they must get and why.
const EXCEPTIONS: [(AssetKind, &str, &[&str], &str); 1] = [(
    AssetKind::Head,
    "trans-rights-scarf",
    &["neck_gap"],
    "the scarf's middle stripe is explicit white (#fff), 20 units tall and 44 wide, and \
     crosses the neck at y 40-60. On the board it shows white; as one colour (the only \
     thing a single-path upload can be), white is a cut-out, so the shape really has a \
     notch at the neck: left edge 80%. That is exactly what neck_gap warns about. (It \
     used to be reported as faces_up_down, because the top and bottom are full width; \
     the direction lint now knows a full-width band isn't a quarter turn.)",
)];

struct Asset {
    kind: AssetKind,
    slug: String,
    path: PathBuf,
}

fn catalog() -> Vec<Asset> {
    let mut out = Vec::new();
    for kind in [AssetKind::Head, AssetKind::Tail] {
        let dir = fixture_path(&format!("catalog/{}", kind_dir(kind)));
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "svg"))
            .collect();
        files.sort();
        for path in files {
            let slug = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("utf-8 name")
                .to_string();
            out.push(Asset { kind, slug, path });
        }
    }
    out
}

/// The oracle mask: resvg's render of the asset in the board's wrapper (black), with
/// near-white colours as cut-outs.
///
/// The colours are classified, not the pixels: every colour value in the source is
/// rewritten to white when its luma is ≥ 0.9 and to black otherwise, and a pixel is ink
/// when it is at least half opaque and at least half black. Classifying rendered pixels
/// instead (ink = luma < 0.9) would count an edge pixel that is only 10% dark paint over
/// a white detail as ink, moving every cut-out boundary about 0.4 px into the cut-out at
/// 400 px, so that even an exact conversion scored below 0.99 on detailed multi-colour
/// heads (ghost: 0.977 for a `vector_exact` result).
fn oracle(kind: AssetKind, slug: &str, side: u32) -> Vec<u8> {
    let svg = two_tone(&catalog_svg(kind, slug));
    render(&svg, side)
        .pixels()
        .iter()
        .map(|p| {
            if p.alpha() >= 128 && p.demultiply().red() < 128 {
                255
            } else {
                0
            }
        })
        .collect()
}

/// Rewrite every `fill`/`stroke`/`stop-color` colour (attribute, `style` or CSS) to black,
/// or to white when its luma is at least 0.9. `none`, `url(...)` and keywords are kept;
/// an rgba() alpha is kept.
fn two_tone(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    'scan: while !rest.is_empty() {
        for prop in ["fill", "stroke", "stop-color"] {
            for sep in [":", "=\"", "='"] {
                let key = format!("{prop}{sep}");
                if rest.starts_with(&key) && !rest[key.len()..].starts_with('-') {
                    out.push_str(&key);
                    rest = &rest[key.len()..];
                    let end = rest.find([';', '"', '\'', '}']).unwrap_or(rest.len());
                    out.push_str(&recolour(&rest[..end]));
                    rest = &rest[end..];
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().expect("non-empty");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn recolour(value: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    let hex = |h: &str| u8::from_str_radix(h, 16).expect("hex digit");
    let rgb: Option<([u8; 3], Option<&str>)> = if let Some(h) = v.strip_prefix('#') {
        match h.len() {
            3 => {
                let d = |i: usize| hex(&h[i..i + 1]) * 17;
                Some(([d(0), d(1), d(2)], None))
            }
            6 => Some(([hex(&h[0..2]), hex(&h[2..4]), hex(&h[4..6])], None)),
            _ => None,
        }
    } else if let Some(args) = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|a| a.strip_suffix(')'))
    {
        let parts: Vec<&str> = args.split(',').map(str::trim).collect();
        let ch = |i: usize| parts[i].parse::<u8>().expect("rgb channel");
        Some(([ch(0), ch(1), ch(2)], parts.get(3).copied()))
    } else {
        match v.as_str() {
            "white" => Some(([255; 3], None)),
            "black" => Some(([0; 3], None)),
            "gray" | "grey" => Some(([128; 3], None)),
            _ => None,
        }
    };
    let Some((c, alpha)) = rgb else {
        return value.to_string();
    };
    let luma = (0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32) / 255.0;
    let shade = if luma >= 0.9 { 255 } else { 0 };
    match alpha {
        Some(a) => format!("rgba({shade},{shade},{shade},{a})"),
        None => format!("rgb({shade},{shade},{shade})"),
    }
}

struct Checked {
    name: String,
    strategy: Strategy,
    iou: f64,
    elapsed: Duration,
    info: Vec<&'static str>,
}

fn check(asset: &Asset, rows: &[Row]) -> Result<Checked, String> {
    let name = format!("{}/{}", kind_dir(asset.kind), asset.slug);
    let bytes = std::fs::read(&asset.path).map_err(|e| format!("{name}: {e}"))?;
    let t = Instant::now();
    let shape: CleanShape =
        process_upload(&bytes, &Limits::default(), &[]).map_err(|e| format!("{name}: {e:?}"))?;
    let elapsed = t.elapsed();
    assert_clean(&shape);
    let mut problems = Vec::new();

    let iou = iou(
        &oracle(asset.kind, &asset.slug, 400),
        &shape_alpha(&shape, 400),
    );
    if iou < 0.99 {
        problems.push(format!("IoU {iou:.4} < 0.99"));
    }

    let file = format!("svg/{}/{}.svg", kind_dir(asset.kind), asset.slug);
    let row = rows
        .iter()
        .find(|r| r.file == file)
        .ok_or_else(|| format!("{name}: no CSV row"))?;
    let m = shape.metrics();
    if row.cov_fixed == 0.0 {
        let fill = row.cov * 100.0;
        let edge = row.edge_left * 100.0;
        if (m.fill_pct - fill).abs() > 2.0 {
            problems.push(format!("fill {:.1}% vs oracle {fill:.1}%", m.fill_pct));
        }
        if (m.left_edge_pct - edge).abs() > 2.0 {
            problems.push(format!(
                "left edge {:.1}% vs oracle {edge:.1}%",
                m.left_edge_pct
            ));
        }
    }

    let warns: Vec<&str> = shape
        .lints()
        .for_kind(asset.kind)
        .iter()
        .filter(|l| l.severity() == Severity::Warn)
        .map(|l| l.code())
        .collect();
    let allowed: &[&str] = EXCEPTIONS
        .iter()
        .find(|(kind, slug, _, _)| *kind == asset.kind && *slug == asset.slug)
        .map_or(&[], |(_, _, warns, _)| warns);
    if warns != allowed {
        problems.push(format!("warnings {warns:?}, expected {allowed:?}"));
    }

    if problems.is_empty() {
        Ok(Checked {
            name,
            strategy: shape.strategy(),
            iou,
            elapsed,
            info: codes(shape.info()),
        })
    } else {
        Err(format!(
            "{name} ({:?}, fill {:.1}%, left edge {:.1}%): {}",
            shape.strategy(),
            m.fill_pct,
            m.left_edge_pct,
            problems.join("; ")
        ))
    }
}

#[test]
fn every_catalog_asset_is_processed_cleanly() {
    let assets = catalog();
    assert_eq!(assets.len(), 184, "all 101 heads and 83 tails are vendored");
    for (kind, slug, _, reason) in EXCEPTIONS {
        assert!(
            assets.iter().any(|a| a.kind == kind && a.slug == slug),
            "exception for a missing asset {slug}"
        );
        assert!(!reason.is_empty());
    }
    let rows = summary();
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .clamp(1, 4);
    let chunk = assets.len().div_ceil(threads);
    let results: Vec<Result<Checked, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = assets
            .chunks(chunk)
            .map(|part| s.spawn(|| part.iter().map(|a| check(a, &rows)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("corpus thread"))
            .collect()
    });
    assert_eq!(results.len(), 184);

    let ok: Vec<&Checked> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let failures: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    for c in &ok {
        println!(
            "{}: {:?} IoU {:.4} {:?} {:?}",
            c.name, c.strategy, c.iou, c.elapsed, c.info
        );
    }
    let exact = ok
        .iter()
        .filter(|c| c.strategy == Strategy::VectorExact)
        .count();
    let worst = ok.iter().map(|c| c.iou).fold(1.0, f64::min);
    let slowest = ok.iter().map(|c| c.elapsed).max().unwrap_or_default();
    println!(
        "{} ok ({exact} vector_exact, {} retraced), worst IoU {worst:.4}, slowest {slowest:?}",
        ok.len(),
        ok.len() - exact
    );
    assert!(
        failures.is_empty(),
        "{} of 184 failed:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
