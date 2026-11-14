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
    AssetKind, CleanShape, Limits, PROCESS_STACK_BYTES, ProcessError, Severity, Strategy,
    catalog_layers,
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

/// Every vendored asset with at least one accent candidate (an explicit
/// colour with luma < [`LIGHT_LUMA`] that isn't the snake placeholder),
/// asserted equal to what [`paint_colours`] derives over the corpus. Pinned
/// so a catalog update that adds multi-colour art fails the test until the
/// layer table is regenerated.
const MULTI_COLOUR_CANDIDATES: &[(AssetKind, &str)] = &[
    (AssetKind::Head, "beach-puffin-special"),
    (AssetKind::Head, "cosmic-horror-special"),
    (AssetKind::Head, "crystal-power"),
    (AssetKind::Head, "nr-rocket"),
    (AssetKind::Head, "pirate-special"),
    (AssetKind::Head, "pirate"),
    (AssetKind::Head, "shades"),
    (AssetKind::Head, "trans-rights-scarf"),
    (AssetKind::Tail, "beach-puffin-special"),
    (AssetKind::Tail, "crystal-power"),
    (AssetKind::Tail, "nr-booster"),
    (AssetKind::Tail, "pirate-special"),
    (AssetKind::Tail, "pirate"),
    (AssetKind::Tail, "weight"),
];

/// Exactly the `(kind, file)` set in `catalog_layers::CATALOG_LAYER_ART`
/// (assets whose accents all dropped, like `shades` and `weight`, have no
/// entry: their white details stay cut-outs in the base path).
const LAYERED_ASSETS: &[(AssetKind, &str)] = &[
    (AssetKind::Head, "beach-puffin-special"),
    (AssetKind::Head, "cosmic-horror-special"),
    (AssetKind::Head, "crystal-power"),
    (AssetKind::Head, "pirate-special"),
    (AssetKind::Head, "pirate"),
    (AssetKind::Head, "trans-rights-scarf"),
    (AssetKind::Tail, "beach-puffin-special"),
    (AssetKind::Tail, "crystal-power"),
    (AssetKind::Tail, "pirate-special"),
    (AssetKind::Tail, "pirate"),
];

/// Accent drops, exact and in both directions ([`DropReason`] lists the only
/// two legitimate reasons): a new drop means fix the extraction or extend
/// this with a justification; a missing drop means a stale entry.
const EXPECTED_ACCENT_DROPS: &[(AssetKind, &str, [u8; 3], DropReason, &str)] = &[
    // crystal-power head: the cry-1 background triangle and cry-11's inner
    // hexagon are fully covered by the crystal facets painted above them.
    (
        AssetKind::Head,
        "crystal-power",
        [153, 153, 153],
        DropReason::Empty,
        "cry-1's background triangle is painted first and fully covered by the \
         facets above it, so its isolate renders no ink",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [35, 35, 35],
        DropReason::Empty,
        "cry-11's inner hexagon is fully covered by the #3a3a3a ring and the \
         facet slices painted above it",
    ),
    // crystal-power head: the six lower facet rings each show only a sliver
    // past the facets above them (the #3a3a3a ring just above them survives
    // at 0.71%; these fall under the 0.5% floor).
    (
        AssetKind::Head,
        "crystal-power",
        [211, 113, 5],
        DropReason::BelowFloor,
        "facet ring (cry-13): almost fully covered by the facets above it, the \
         visible sliver is under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [186, 94, 2],
        DropReason::BelowFloor,
        "facet slice (cry-14): almost fully covered by the facets above it, \
         under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [130, 57, 7],
        DropReason::BelowFloor,
        "facet slice (cry-15): almost fully covered by the facets above it, \
         under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [112, 49, 6],
        DropReason::BelowFloor,
        "facet slice (cry-16): almost fully covered by the facets above it, \
         under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [96, 29, 2],
        DropReason::BelowFloor,
        "facet slice (cry-17): almost fully covered by the facets above it, \
         under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "crystal-power",
        [163, 79, 2],
        DropReason::BelowFloor,
        "facet slice (cry-18): almost fully covered by the facets above it, \
         under the 0.5% floor",
    ),
    (
        AssetKind::Head,
        "nr-rocket",
        [44, 44, 44],
        DropReason::Empty,
        "four of the five uses are the off-canvas negative-x artboard copies; \
         the fifth is a ~6×2-unit sliver at the neck that the body painted \
         over covers entirely",
    ),
    (
        AssetKind::Head,
        "shades",
        [0, 0, 0],
        DropReason::Empty,
        "the lens is rgba(0,0,0,0.4): effective alpha 102 is under the pipeline's \
         128/255 ink threshold, so the isolate renders nothing",
    ),
    (
        AssetKind::Tail,
        "nr-booster",
        [44, 44, 44],
        DropReason::Empty,
        "all three uses sit on the off-canvas negative-x artboard copies \
         (M-274.8, M-227.2, M-198.6)",
    ),
    (
        AssetKind::Tail,
        "pirate-special",
        [102, 102, 102],
        DropReason::BelowFloor,
        "the two small bolt fixtures near the tail tip: their total visible ink \
         is under the 0.5% floor (~2.4 px² at avatar size)",
    ),
    (
        AssetKind::Tail,
        "pirate-special",
        [165, 96, 14],
        DropReason::BelowFloor,
        "one thin flag sliver polygon (cls-9): visible ink under the 0.5% floor",
    ),
    (
        AssetKind::Tail,
        "pirate-special",
        [204, 204, 204],
        DropReason::BelowFloor,
        "four rope-stitch circles of radius 1.3–2 units (cls-10): combined ink \
         under the 0.5% floor",
    ),
    (
        AssetKind::Tail,
        "pirate",
        [204, 204, 204],
        DropReason::BelowFloor,
        "five rope-stitch circles of radius 1.3–2 units (cls-2): combined ink \
         under the 0.5% floor",
    ),
    (
        AssetKind::Tail,
        "weight",
        [35, 31, 32],
        DropReason::Empty,
        ".cls-1{fill:#231f20} is defined but never used — dead CSS, so the \
         isolate renders nothing",
    ),
];

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

/// Rec. 709 luma of an RGB triple, 0..=255. The pipeline's cut-out rule calls
/// ≥ [`LIGHT_LUMA`] light.
fn luma(c: [u8; 3]) -> u32 {
    (c[0] as u32 * 2126 + c[1] as u32 * 7152 + c[2] as u32 * 722) / 10_000
}

/// Light at 230 of 255 (0.9): the pipeline's cut-out rule.
const LIGHT_LUMA: u32 = 230;

/// A colour value as RGB plus its `rgba()` alpha part, or `None` when the
/// value is not a colour we can classify (`none`, `url(...)`, other
/// keywords). `value` must be trimmed and lowercased, as `recolour` and the
/// scans below prepare it.
fn parse_colour(value: &str) -> Option<([u8; 3], Option<&str>)> {
    let hex = |h: &str| u8::from_str_radix(h, 16).expect("hex digit");
    if let Some(h) = value.strip_prefix('#') {
        return match h.len() {
            3 => {
                let d = |i: usize| hex(&h[i..i + 1]) * 17;
                Some(([d(0), d(1), d(2)], None))
            }
            6 => Some(([hex(&h[0..2]), hex(&h[2..4]), hex(&h[4..6])], None)),
            _ => None,
        };
    }
    if let Some(args) = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
        .and_then(|a| a.strip_suffix(')'))
    {
        let parts: Vec<&str> = args.split(',').map(str::trim).collect();
        let ch = |i: usize| parts[i].parse::<u8>().expect("rgb channel");
        return Some(([ch(0), ch(1), ch(2)], parts.get(3).copied()));
    }
    match value {
        "white" => Some(([255; 3], None)),
        "black" => Some(([0; 3], None)),
        "gray" | "grey" => Some(([128; 3], None)),
        _ => None,
    }
}

fn recolour(value: &str) -> String {
    let v = value.trim().to_ascii_lowercase();
    let Some((c, alpha)) = parse_colour(&v) else {
        return value.to_string();
    };
    let shade = if luma(c) >= LIGHT_LUMA { 255 } else { 0 };
    match alpha {
        Some(a) => format!("rgba({shade},{shade},{shade},{a})"),
        None => format!("rgb({shade},{shade},{shade})"),
    }
}

/// The `#0f0` placeholder the catalog source uses for the snake-colour
/// regions: never a layer target.
const SNAKE_PLACEHOLDER: [u8; 3] = [0, 255, 0];

/// A layer below this fill percentage is dropped: 0.5 is 50 unit², about
/// 2.4 px² at the 22 px leaderboard avatar — under it a layer can't be seen.
/// (The pipeline's own `Empty` guard at 0.05% only removes specks.)
const MIN_LAYER_FILL_PCT: f32 = 0.5;

/// Every distinct explicit `fill`/`stroke`/`stop-color` colour in an asset's
/// inner markup, first-appearance order, excluding the `#0f0` snake
/// placeholder. Scans text exactly like [`two_tone`]; dead CSS and off-canvas
/// colours it finds are eliminated later, when their isolate renders nothing
/// (`Empty`) or falls under [`MIN_LAYER_FILL_PCT`].
fn paint_colours(inner: &str) -> Vec<[u8; 3]> {
    let mut out: Vec<[u8; 3]> = Vec::new();
    let mut rest = inner;
    'scan: while !rest.is_empty() {
        for prop in ["fill", "stroke", "stop-color"] {
            for sep in [":", "=\"", "='"] {
                let key = format!("{prop}{sep}");
                if rest.starts_with(&key) && !rest[key.len()..].starts_with('-') {
                    let after = &rest[key.len()..];
                    let end = after.find([';', '"', '\'', '}']).unwrap_or(after.len());
                    let v = after[..end].trim().to_ascii_lowercase();
                    if let Some((rgb, _)) = parse_colour(&v)
                        && rgb != SNAKE_PLACEHOLDER
                        && !out.contains(&rgb)
                    {
                        out.push(rgb);
                    }
                    rest = &after[end..];
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().expect("non-empty");
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// A recolour-isolate of an asset's inner markup: every colour where
/// `is_target` holds becomes black (alpha preserved), every other colour
/// white; `none`, `url(...)` and unknown keywords are unchanged. Rendered,
/// black is exactly the target colour's ink, with z-order, occlusion, clips
/// and opacity resolved the way the board resolves them.
fn isolate(inner: &str, is_target: &dyn Fn([u8; 3]) -> bool) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    'scan: while !rest.is_empty() {
        for prop in ["fill", "stroke", "stop-color"] {
            for sep in [":", "=\"", "='"] {
                let key = format!("{prop}{sep}");
                if rest.starts_with(&key) && !rest[key.len()..].starts_with('-') {
                    out.push_str(&key);
                    let after = &rest[key.len()..];
                    let end = after.find([';', '"', '\'', '}']).unwrap_or(after.len());
                    let value = &after[..end];
                    let v = value.trim().to_ascii_lowercase();
                    match parse_colour(&v) {
                        Some((rgb, alpha)) => {
                            let shade = if is_target(rgb) { 0 } else { 255 };
                            match alpha {
                                Some(a) => {
                                    out.push_str(&format!("rgba({shade},{shade},{shade},{a})"))
                                }
                                None => out.push_str(&format!("rgb({shade},{shade},{shade})")),
                            }
                        }
                        None => out.push_str(value),
                    }
                    rest = &after[end..];
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

/// The isolate as an upload: the board's wrapper with a white fill, so
/// unfilled (snake-colour) regions render background, not ink. The wrapper
/// is added after the scan, so it is never rewritten — load-bearing for the
/// white composite, whose wrapper must stay white.
fn layer_svg(kind: AssetKind, slug: &str, is_target: &dyn Fn([u8; 3]) -> bool) -> String {
    let inner = isolate(&catalog_inner(kind, slug), is_target);
    board_svg(&inner, "#ffffff")
}

/// A fixed-colour layer before it becomes a table row: its colour and the
/// clean shape of its isolate.
type CatalogLayerSpec = ([u8; 3], CleanShape);

/// The only two reasons an accent layer may drop out silently. Everything
/// else — a non-`Empty` error, an above-floor shape the oracle rejects, a
/// too-tinted near-white — is a [`Fatal`]: a bug to fix, never a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropReason {
    /// Nothing drawable survived: dead CSS, off-canvas geometry, or
    /// effective alpha below the pipeline's 128/255 ink threshold.
    Empty,
    /// Ink under [`MIN_LAYER_FILL_PCT`]: too small to matter at avatar size.
    BelowFloor,
}

/// An outcome that must never happen silently; the generator panics and the
/// corpus check fails, both naming asset, colour and detail.
struct Fatal {
    colour: [u8; 3],
    detail: String,
}

struct ExtractionOutcome {
    /// Accents in first-appearance order, then the white composite.
    layers: Vec<CatalogLayerSpec>,
    /// Accent drops (inventory entries; white-composite drops are not).
    drops: Vec<([u8; 3], DropReason)>,
    fatals: Vec<Fatal>,
}

/// The isolate's truth mask at 400 px: ink = at least half opaque and at
/// least half black (the oracle's pixel rule).
fn truth_mask(svg: &str) -> Vec<u8> {
    render(svg, 400)
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

/// The per-layer oracle: the clean path against its isolate's truth mask,
/// outline distance ≤ 1 unit and no lost or added detail. Failing it is a
/// [`Fatal`], never a drop.
fn layer_oracle(shape: &CleanShape, truth: &[u8]) -> Result<(), String> {
    const SIDE: usize = 400;
    let ours = shape_alpha(shape, SIDE as u32);
    let e = outline_error(truth, &ours);
    let as_bool = |m: &[u8]| m.iter().map(|&a| a >= 128).collect::<Vec<bool>>();
    let lost = missing_details(&as_bool(truth), &as_bool(&ours), SIDE, "lost");
    let added = missing_details(&as_bool(&ours), &as_bool(truth), SIDE, "added");
    if e > 1.0 {
        Err(format!("outline_error {e:.2} > 1.0"))
    } else if !lost.is_empty() || !added.is_empty() {
        Err(format!(
            "outline_error {e:.2}, lost {lost:?} added {added:?}"
        ))
    } else {
        Ok(())
    }
}

/// Extract one layer for `colour`: process its isolate, split the two
/// legitimate drops from everything else, and oracle whatever survives.
fn attempt_layer(
    asset: &Asset,
    colour: [u8; 3],
    is_target: &dyn Fn([u8; 3]) -> bool,
    drops: &mut Vec<([u8; 3], DropReason)>,
    fatals: &mut Vec<Fatal>,
) -> Option<CleanShape> {
    let svg = layer_svg(asset.kind, &asset.slug, is_target);
    let truth = truth_mask(&svg);
    if !truth.iter().any(|&a| a >= 128) {
        // Nothing of this colour renders ink (dead CSS, off-canvas geometry,
        // sub-threshold alpha). The pipeline can't always say `Empty` itself —
        // an isolate whose only explicit colour is white paints as ink in
        // single-colour mode — so the truth decides.
        drops.push((colour, DropReason::Empty));
        return None;
    }
    match process_upload(svg.as_bytes(), &Limits::default(), &[]) {
        Err(ProcessError::Empty { .. }) => {
            drops.push((colour, DropReason::Empty));
            None
        }
        Err(e) => {
            fatals.push(Fatal {
                colour,
                detail: format!("{e:?}"),
            });
            None
        }
        Ok(shape) => {
            if shape.metrics().fill_pct < MIN_LAYER_FILL_PCT {
                drops.push((colour, DropReason::BelowFloor));
                return None;
            }
            if let Err(detail) = layer_oracle(&shape, &truth) {
                fatals.push(Fatal { colour, detail });
                return None;
            }
            Some(shape)
        }
    }
}

/// A multi-colour asset's fixed-colour layers: one isolate per accent colour
/// (luma < [`LIGHT_LUMA`], excluding the snake placeholder), plus a white
/// composite (#ffffff) gated on at least one accent surviving — single-colour
/// art keeps its white details as cut-outs in the base path. Deterministic:
/// the generator and the corpus check both call this.
fn extract_layers(asset: &Asset) -> ExtractionOutcome {
    let inner = catalog_inner(asset.kind, &asset.slug);
    let all = paint_colours(&inner);
    let accents = all.iter().copied().filter(|&c| luma(c) < LIGHT_LUMA);
    let whites: Vec<[u8; 3]> = all
        .iter()
        .copied()
        .filter(|&c| luma(c) >= LIGHT_LUMA)
        .collect();
    let mut layers = Vec::new();
    let mut drops = Vec::new();
    let mut fatals = Vec::new();

    for colour in accents {
        if let Some(shape) =
            attempt_layer(asset, colour, &|rgb| rgb == colour, &mut drops, &mut fatals)
        {
            layers.push((colour, shape));
        }
    }

    if !layers.is_empty() && !whites.is_empty() {
        // Tinted off-whites can't be silently flattened into one #ffffff.
        for &w in &whites {
            if 255 - w.iter().min().copied().unwrap_or(255) > 16 {
                fatals.push(Fatal {
                    colour: w,
                    detail: format!(
                        "near-white too tinted to flatten: #{:02x}{:02x}{:02x}",
                        w[0], w[1], w[2]
                    ),
                });
            }
        }
        // The white composite: Empty/below-floor drops are legitimate but not
        // inventory entries — the white details then stay cut-outs. Assets
        // with no near-white colour have no composite to extract.
        let mut white_drops = Vec::new();
        if let Some(shape) = attempt_layer(
            asset,
            [255, 255, 255],
            &|rgb| luma(rgb) >= LIGHT_LUMA,
            &mut white_drops,
            &mut fatals,
        ) {
            layers.push(([255, 255, 255], shape));
        }
    }

    ExtractionOutcome {
        layers,
        drops,
        fatals,
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

    // Fixed-colour layers: multi-colour art must render with its accents.
    // Extraction runs `process_upload` (this worker already has the big
    // stack); white-only assets have no accent candidates, so it only scans.
    let outcome = extract_layers(asset);
    for f in &outcome.fatals {
        problems.push(format!(
            "layer #{:02x}{:02x}{:02x}: {}",
            f.colour[0], f.colour[1], f.colour[2], f.detail
        ));
    }
    let expected_drops: Vec<([u8; 3], DropReason)> = EXPECTED_ACCENT_DROPS
        .iter()
        .filter(|(k, s, _, _, _)| *k == asset.kind && *s == asset.slug)
        .map(|(_, _, c, r, _)| (*c, *r))
        .collect();
    if outcome.drops != expected_drops {
        let fmt_drop =
            |(c, r): &([u8; 3], DropReason)| format!("#{:02x}{:02x}{:02x}/{r:?}", c[0], c[1], c[2]);
        problems.push(format!(
            "accent drops [{}] != inventory [{}]: a new drop means fix the extraction \
             (or extend EXPECTED_ACCENT_DROPS with a justification if genuinely correct), a \
             missing drop means a stale inventory entry to remove",
            outcome
                .drops
                .iter()
                .map(fmt_drop)
                .collect::<Vec<_>>()
                .join(", "),
            expected_drops
                .iter()
                .map(fmt_drop)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let table = catalog_layers::find(asset.kind, &asset.slug);
    for entry in table {
        let hex = entry.fill.strip_prefix('#').unwrap_or_default();
        if hex.len() != 6
            || !hex
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            problems.push(format!(
                "layer fill {:?} is not lowercase #rrggbb",
                entry.fill
            ));
        }
    }
    if table.len() != outcome.layers.len()
        || outcome
            .layers
            .iter()
            .zip(table)
            .any(|((colour, shape), entry)| {
                entry.fill != format!("#{:02x}{:02x}{:02x}", colour[0], colour[1], colour[2])
                    || entry.d != shape.path_d()
                    || entry.fill_rule != shape.fill_rule()
            })
    {
        problems.push(format!(
            "layer table {} design_kit/catalog_layers.rs (regenerate: cargo test -p arena \
             --test design_kit_catalog -- --ignored write_catalog_layers)",
            if table.is_empty() && !outcome.layers.is_empty() {
                "missing from"
            } else {
                "stale in"
            }
        ));
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
    // The layer table is keyed into CATALOG_SHAPES, honours the white gate
    // (no entry without an accent layer), and the drop inventory points at
    // real assets whose colours really appear in them.
    let shapes: Vec<(AssetKind, &str)> = CATALOG_SHAPES.iter().map(|s| (s.kind, s.file)).collect();
    for art in catalog_layers::CATALOG_LAYER_ART {
        assert!(
            shapes.contains(&(art.kind, art.file)),
            "layer entry {}/{} has no CATALOG_SHAPES shape",
            kind_dir(art.kind),
            art.file
        );
        assert!(
            art.layers.iter().any(|l| l.fill != "#ffffff"),
            "layer entry {}/{} is only a white layer: the white gate",
            kind_dir(art.kind),
            art.file
        );
    }
    for (kind, slug, colour, _, reason) in EXPECTED_ACCENT_DROPS {
        assert!(
            assets.iter().any(|a| a.kind == *kind && a.slug == *slug),
            "drop inventory entry for a missing asset {slug}"
        );
        assert!(!reason.is_empty(), "unjustified drop for {slug}");
        assert!(
            paint_colours(&catalog_inner(*kind, slug)).contains(colour),
            "drop colour #{:02x}{:02x}{:02x} does not appear in {slug}'s source",
            colour[0],
            colour[1],
            colour[2]
        );
    }
    // Pinned sets: the accent candidates and the layered assets.
    let derived: Vec<(AssetKind, &str)> = assets
        .iter()
        .filter(|a| {
            paint_colours(&catalog_inner(a.kind, &a.slug))
                .iter()
                .any(|&c| luma(c) < LIGHT_LUMA)
        })
        .map(|a| (a.kind, a.slug.as_str()))
        .collect();
    assert_eq!(
        MULTI_COLOUR_CANDIDATES.to_vec(),
        derived,
        "the corpus's accent candidates moved; investigate before editing the pinned list"
    );
    let table_set: Vec<(AssetKind, &str)> = catalog_layers::CATALOG_LAYER_ART
        .iter()
        .map(|a| (a.kind, a.file))
        .collect();
    assert_eq!(
        LAYERED_ASSETS.to_vec(),
        table_set,
        "layer table drifted; regenerate (or update LAYERED_ASSETS after one)"
    );
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

/// Rewrite the layer table in `src/design_kit/catalog_layers.rs` (between its
/// `@generated` markers) from fresh extraction of every vendored catalog
/// file, and print the inventory: kept layers per asset plus every accent
/// drop, formatted as [`EXPECTED_ACCENT_DROPS`] entries (with a TODO
/// justification to fill in by hand).
#[test]
#[ignore = "rewrites src/design_kit/catalog_layers.rs; run by hand"]
fn write_catalog_layers() {
    use std::fmt::Write as _;

    let assets = catalog();
    // One thread with production's stack, like the corpus test's workers:
    // extraction calls `process_upload`, never on a default-stack thread.
    let outcomes: Vec<(Asset, ExtractionOutcome)> = std::thread::Builder::new()
        .stack_size(PROCESS_STACK_BYTES)
        .spawn(move || {
            assets
                .into_iter()
                .map(|a| {
                    let outcome = extract_layers(&a);
                    (a, outcome)
                })
                .collect()
        })
        .expect("generator thread")
        .join()
        .expect("generator thread");

    let mut fatals = String::new();
    for (asset, o) in &outcomes {
        for f in &o.fatals {
            writeln!(
                fatals,
                "{}/{}: #{:02x}{:02x}{:02x}: {}",
                kind_dir(asset.kind),
                asset.slug,
                f.colour[0],
                f.colour[1],
                f.colour[2],
                f.detail
            )
            .expect("write to string");
        }
    }
    assert!(fatals.is_empty(), "extraction fatals:\n{fatals}");

    let mut table = String::new();
    let mut inventory = String::new();
    for (asset, o) in &outcomes {
        if o.layers.is_empty() {
            continue;
        }
        writeln!(
            inventory,
            "// {}/{}: {} layers",
            kind_dir(asset.kind),
            asset.slug,
            o.layers.len()
        )
        .expect("write to string");
        write!(
            table,
            "    CatalogLayerArt {{\n        kind: AssetKind::{:?},\n        file: {:?},\n        layers: &[\n",
            asset.kind, asset.slug
        )
        .expect("write to string");
        for (colour, shape) in &o.layers {
            writeln!(
                inventory,
                "//   kept #{:02x}{:02x}{:02x} (fill {:.2}%, {:?})",
                colour[0],
                colour[1],
                colour[2],
                shape.metrics().fill_pct,
                shape.strategy()
            )
            .expect("write to string");
            write!(
                table,
                "            CatalogLayer {{\n                fill: \"#{:02x}{:02x}{:02x}\",\n                d: {:?},\n                fill_rule: FillRule::{:?},\n            }},\n",
                colour[0],
                colour[1],
                colour[2],
                shape.path_d(),
                shape.fill_rule()
            )
            .expect("write to string");
        }
        write!(table, "        ],\n    }},\n").expect("write to string");
    }
    for (asset, o) in &outcomes {
        for (colour, reason) in &o.drops {
            writeln!(
                inventory,
                "    (AssetKind::{:?}, {:?}, [{}, {}, {}], DropReason::{}, \"TODO: justify this drop\"),",
                asset.kind,
                asset.slug,
                colour[0],
                colour[1],
                colour[2],
                match reason {
                    DropReason::Empty => "Empty",
                    DropReason::BelowFloor => "BelowFloor",
                }
            )
            .expect("write to string");
        }
    }

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/design_kit/catalog_layers.rs");
    let source = std::fs::read_to_string(&path).expect("catalog_layers.rs");
    let (begin, end) = ("    // @generated begin\n", "    // @generated end\n");
    let start = source.find(begin).expect("begin marker") + begin.len();
    let stop = source.find(end).expect("end marker");
    let updated = format!("{}{table}{}", &source[..start], &source[stop..]);
    std::fs::write(&path, updated).expect("write catalog_layers.rs");
    println!(
        "wrote {} layered assets to {}",
        outcomes
            .iter()
            .filter(|(_, o)| !o.layers.is_empty())
            .count(),
        path.display()
    );
    println!("inventory (EXPECTED_ACCENT_DROPS entries need justifications):\n{inventory}");
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
