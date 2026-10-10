//! The official catalog as a corpus: every one of the 184 head and tail SVGs, uploaded
//! as is, must come out clean.
//!
//! Per file:
//! * processing succeeds (as an artist's upload, through `process_upload`);
//! * IoU ≥ 0.99 against an independent render: resvg (a dev-dependency, never used at
//!   runtime) draws the original inside the board's wrapper in black, and near-white
//!   pixels count as cut-outs (how multi-colour assets draw eyes);
//! * no detail lost or added: the core of every ink region and every hole (see
//!   [`missing_details`]) in either render is mostly the same in the other (IoU alone
//!   lets a whole eye go: a 50 unit² detail is under 1% of a head);
//! * for single-colour files, fill% and left-edge% within 2 points of
//!   `catalog/metrics_summary.csv` (rsvg renders from the DEV-1539 investigation, an
//!   oracle independent of both resvg and our metrics);
//! * no warn-level lint for its kind (the lint thresholds were set outside the
//!   catalog's range; this is the check that they stay there through the SVG pipeline),
//!   except the one documented in [`EXCEPTIONS`], which must fire exactly as listed.
//!
//! It also checks `design_kit::catalog_shapes`, the generated table social cards draw
//! from, against each file's fresh output (and [`write_catalog_shapes`] regenerates it).
//!
//! Failures are listed by slug. The files are processed in chunks across threads, each
//! with the stack production uses (`PROCESS_STACK_BYTES`), so a deep file fails the test
//! instead of aborting the binary.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use arena::design_kit::{
    AssetKind, CleanShape, Limits, PROCESS_STACK_BYTES, Severity, Strategy,
    catalog_shapes::{self, CATALOG_SHAPES},
    process_upload,
};
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
/// rewritten to white when its luma is ≥ 0.9 (230 of 255) and to black otherwise, and a pixel is ink
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
/// or to white when its luma is at least 230 of 255 (0.9). `none`, `url(...)` and keywords are kept;
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
    // Rec. 709 luma, light at 230 of 255 (0.9): the pipeline's cut-out rule.
    let luma = (c[0] as u32 * 2126 + c[1] as u32 * 7152 + c[2] as u32 * 722) / 10_000;
    let shade = if luma >= 230 { 255 } else { 0 };
    match alpha {
        Some(a) => format!("rgba({shade},{shade},{shade},{a})"),
        None => format!("rgb({shade},{shade},{shade})"),
    }
}

/// 4-connected regions of `value` in a `side`x`side` mask, as pixel index lists.
fn regions(mask: &[bool], side: usize, value: bool) -> Vec<Vec<usize>> {
    let mut seen = vec![false; mask.len()];
    let mut out = Vec::new();
    for start in 0..mask.len() {
        if seen[start] || mask[start] != value {
            continue;
        }
        seen[start] = true;
        let (mut region, mut stack) = (Vec::new(), vec![start]);
        while let Some(i) = stack.pop() {
            region.push(i);
            let (x, y) = (i % side, i / side);
            let mut visit = |j: usize| {
                if !seen[j] && mask[j] == value {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < side {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - side);
            }
            if y + 1 < side {
                visit(i + side);
            }
        }
        out.push(region);
    }
    out
}

/// Details of `from` that are mostly gone in `to`. A detail is a region of ink or of
/// background; its core is the pixels whose 8 neighbours are in it too (outside the
/// raster counts as in), so a sliver one pixel thick has none, and anti-aliasing along
/// its edge can't decide the check. Regions whose core covers at least half a unit²
/// (details from about 1.5 unit²; the trace removes specks under 1 unit²) must keep at
/// least half of their core in `to`.
fn missing_details(from: &[bool], to: &[bool], side: usize, what: &str) -> Vec<String> {
    let px_per_unit = side / 100;
    let min_core = px_per_unit * px_per_unit / 2;
    let core = |i: usize| {
        let (x, y) = ((i % side) as i64, (i / side) as i64);
        (-1..=1).all(|dy| {
            (-1..=1).all(|dx| {
                let (nx, ny) = (x + dx, y + dy);
                let outside = nx < 0 || ny < 0 || nx >= side as i64 || ny >= side as i64;
                outside || from[ny as usize * side + nx as usize] == from[i]
            })
        })
    };
    let mut out = Vec::new();
    for value in [true, false] {
        for r in regions(from, side, value) {
            let core: Vec<usize> = r.iter().copied().filter(|&i| core(i)).collect();
            let kept = core.iter().filter(|&&i| to[i] == value).count();
            if core.len() >= min_core && kept * 2 < core.len() {
                let (x, y) = (core[0] % side, core[0] / side);
                out.push(format!(
                    "{what} {} of {:.1} unit² near ({}, {})",
                    if value { "ink" } else { "background" },
                    r.len() as f32 / (px_per_unit * px_per_unit) as f32,
                    x / px_per_unit,
                    y / px_per_unit
                ));
            }
        }
    }
    out
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

    let truth = oracle(asset.kind, &asset.slug, 400);
    let ours = shape_alpha(&shape, 400);
    let iou = iou(&truth, &ours);
    if iou < 0.99 {
        problems.push(format!("IoU {iou:.4} < 0.99"));
    }
    let (truth, ours): (Vec<bool>, Vec<bool>) = (
        truth.iter().map(|&a| a >= 128).collect(),
        ours.iter().map(|&a| a >= 128).collect(),
    );
    problems.extend(missing_details(&truth, &ours, 400, "lost"));
    problems.extend(missing_details(&ours, &truth, 400, "added"));

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

    match catalog_shapes::find(asset.kind, &asset.slug) {
        None => problems.push(regenerate("missing from")),
        Some(entry) if entry.d != shape.path_d() || entry.fill_rule != shape.fill_rule() => {
            problems.push(regenerate("stale in"));
        }
        Some(_) => {}
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

fn regenerate(what: &str) -> String {
    format!(
        "{what} design_kit/catalog_shapes.rs (regenerate: cargo test -p arena \
         --test design_kit_catalog -- --ignored write_catalog_shapes)"
    )
}

#[test]
fn every_catalog_asset_is_processed_cleanly() {
    let assets = catalog();
    assert_eq!(assets.len(), 184, "all 101 heads and 83 tails are vendored");
    // Each file is checked against its table entry below; this catches extras.
    assert_eq!(
        CATALOG_SHAPES.len(),
        assets.len(),
        "{}",
        regenerate("stale")
    );
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
            .map(|part| {
                std::thread::Builder::new()
                    .stack_size(PROCESS_STACK_BYTES)
                    .spawn_scoped(s, || {
                        part.iter().map(|a| check(a, &rows)).collect::<Vec<_>>()
                    })
                    .expect("corpus thread")
            })
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

/// Rewrite the table in `src/design_kit/catalog_shapes.rs` (between its `@generated`
/// markers) from fresh processing of every vendored catalog file.
#[test]
#[ignore = "rewrites src/design_kit/catalog_shapes.rs; run by hand"]
fn write_catalog_shapes() {
    use std::fmt::Write as _;

    let assets = catalog();
    // One thread with production's stack, like the corpus test's workers.
    let shapes: Vec<(Asset, CleanShape)> = std::thread::Builder::new()
        .stack_size(PROCESS_STACK_BYTES)
        .spawn(move || {
            assets
                .into_iter()
                .map(|asset| {
                    let bytes = std::fs::read(&asset.path).expect("catalog file");
                    let shape = process_upload(&bytes, &Limits::default(), &[])
                        .unwrap_or_else(|e| panic!("{}: {e:?}", asset.slug));
                    (asset, shape)
                })
                .collect()
        })
        .expect("generator thread")
        .join()
        .expect("generator thread");

    let mut table = String::new();
    for (asset, shape) in &shapes {
        write!(
            table,
            "    CatalogShape {{\n        kind: AssetKind::{:?},\n        file: {:?},\n        \
             d: {:?},\n        fill_rule: FillRule::{:?},\n    }},\n",
            asset.kind,
            asset.slug,
            shape.path_d(),
            shape.fill_rule()
        )
        .expect("write to string");
    }

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/design_kit/catalog_shapes.rs");
    let source = std::fs::read_to_string(&path).expect("catalog_shapes.rs");
    let (begin, end) = ("    // @generated begin\n", "    // @generated end\n");
    let start = source.find(begin).expect("begin marker") + begin.len();
    let stop = source.find(end).expect("end marker");
    let updated = format!("{}{table}{}", &source[..start], &source[stop..]);
    std::fs::write(&path, updated).expect("write catalog_shapes.rs");
    println!("wrote {} shapes to {}", shapes.len(), path.display());
}
