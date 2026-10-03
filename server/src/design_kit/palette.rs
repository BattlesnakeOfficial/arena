//! The template colour contract and the template-aware ink rule.
//!
//! The head/tail templates (PSD for Procreate, SVG for vector apps) draw their guides in
//! five saturated colours and the optional reference shapes in one light "ghost" colour.
//! These are chosen so that the studio can tell them apart from the artist's ink: an
//! artist who forgets to hide the guides before exporting still gets a clean result,
//! plus a `guides_visible` info lint.
//!
//! The template generator must use exactly these colours. The guides layer uses the
//! Multiply blend mode, so guides that sit over the reference ghost come out as the
//! product of the two colours. Those products are dark (navy and crimson), so they are
//! only excluded when the ghost itself is visible in the image; otherwise a navy or
//! crimson drawing would vanish. The exclusion also covers anti-aliased blends of each
//! colour towards white (or of each product towards the ghost).

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// Lower-case `#rrggbb`.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Multiply blend (what the template's Multiply guides layer does over a backdrop).
    const fn multiply(self, other: Rgb) -> Rgb {
        const fn mul(a: u8, b: u8) -> u8 {
            ((a as u16 * b as u16 + 127) / 255) as u8
        }
        Rgb::new(
            mul(self.r, other.r),
            mul(self.g, other.g),
            mul(self.b, other.b),
        )
    }
}

/// Grid lines (light blue).
pub const GRID: Rgb = Rgb::new(0xbf, 0xe3, 0xf7);
/// Canvas border, centre line, small labels.
pub const GUIDE: Rgb = Rgb::new(0x2f, 0x8f, 0xd6);
/// Main labels and the direction arrow.
pub const LABEL: Rgb = Rgb::new(0x1f, 0x6f, 0xb0);
/// The attach edge (neck/body joint) band and ticks.
pub const ATTACH: Rgb = Rgb::new(0xff, 0x4f, 0x86);
/// The attach edge label.
pub const ATTACH_LABEL: Rgb = Rgb::new(0xd4, 0x2a, 0x63);

/// The five guide colours, in template order.
pub const GUIDE_COLOURS: [Rgb; 5] = [GRID, GUIDE, LABEL, ATTACH, ATTACH_LABEL];

/// Fill of the optional reference heads/tails. Baked into the pixels (not layer opacity)
/// so it stays a light ghost in apps that ignore layer opacity.
pub const REFERENCE_GHOST: Rgb = Rgb::new(0xc8, 0xc2, 0xd4);

/// Pixels within this RGB distance of a template colour are never ink.
pub const NEAR_DISTANCE: u32 = 48;

/// Pixels within this RGB distance of a template colour (and visibly coloured, see
/// [`EVIDENCE_MIN_CHROMA`]) count as evidence that guides or a reference were exported.
/// Tighter than [`NEAR_DISTANCE`] so anti-aliased grey edges of black ink never count.
pub const EVIDENCE_DISTANCE: u32 = 24;

/// Minimum `max(r,g,b) - min(r,g,b)` for a pixel to count as guide evidence. The ghost
/// has chroma 18; greys (anti-aliasing, pencil, paper) have about 0.
pub const EVIDENCE_MIN_CHROMA: u8 = 12;

/// Share of all pixels that must be guide evidence before `guides_visible` fires (0.1%).
/// The same share of ghost-coloured pixels counts as a visible reference.
const EVIDENCE_MIN_PER_MILLE: usize = 1;

/// Colours that are never ink: the guide colours and the ghost.
pub(crate) const TEMPLATE: [Rgb; 6] = [GRID, GUIDE, LABEL, ATTACH, ATTACH_LABEL, REFERENCE_GHOST];

/// Every guide colour multiplied over the ghost: guides drawn over a visible reference.
/// Not ink either, but only when the ghost is visible (they are dark).
pub(crate) const GHOST_PRODUCTS: [Rgb; 5] = [
    GRID.multiply(REFERENCE_GHOST),
    GUIDE.multiply(REFERENCE_GHOST),
    LABEL.multiply(REFERENCE_GHOST),
    ATTACH.multiply(REFERENCE_GHOST),
    ATTACH_LABEL.multiply(REFERENCE_GHOST),
];

const WHITE: Rgb = Rgb::new(255, 255, 255);

/// Anti-aliased guide edges are blends between two colours, so the exclusion covers the
/// line segments between them: every template colour towards white (the white
/// background) ...
const BLENDS: [(Rgb, Rgb); 6] = [
    (GRID, WHITE),
    (GUIDE, WHITE),
    (LABEL, WHITE),
    (ATTACH, WHITE),
    (ATTACH_LABEL, WHITE),
    (REFERENCE_GHOST, WHITE),
];

/// ... and, when the ghost is visible, every guide-over-ghost product towards the ghost.
const GHOST_BLENDS: [(Rgb, Rgb); 5] = [
    (GHOST_PRODUCTS[0], REFERENCE_GHOST),
    (GHOST_PRODUCTS[1], REFERENCE_GHOST),
    (GHOST_PRODUCTS[2], REFERENCE_GHOST),
    (GHOST_PRODUCTS[3], REFERENCE_GHOST),
    (GHOST_PRODUCTS[4], REFERENCE_GHOST),
];

fn dist_sq(a: Rgb, b: Rgb) -> u32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).unsigned_abs();
    d(a.r, b.r).pow(2) + d(a.g, b.g).pow(2) + d(a.b, b.b).pow(2)
}

/// Squared distance to the nearest excluded colour.
fn min_dist_sq(c: Rgb, ghost: bool) -> u32 {
    let products: &[Rgb] = if ghost { &GHOST_PRODUCTS } else { &[] };
    TEMPLATE
        .iter()
        .chain(products)
        .map(|&p| dist_sq(c, p))
        .min()
        .unwrap_or(u32::MAX)
}

/// Squared distance from `c` to the segment between `p` and `q`.
fn segment_dist_sq(c: Rgb, p: Rgb, q: Rgb) -> f32 {
    let v = |x: Rgb| [x.r as f32, x.g as f32, x.b as f32];
    let (c, p, q) = (v(c), v(p), v(q));
    let pq = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
    let pc = [c[0] - p[0], c[1] - p[1], c[2] - p[2]];
    let len_sq = pq.iter().map(|x| x * x).sum::<f32>();
    let t = if len_sq > 0.0 {
        (pc.iter().zip(&pq).map(|(a, b)| a * b).sum::<f32>() / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (0..3).map(|i| (pc[i] - t * pq[i]).powi(2)).sum()
}

/// Within [`NEAR_DISTANCE`] of a template colour or of an anti-aliased blend of one.
fn near_template(c: Rgb, ghost: bool) -> bool {
    let near = NEAR_DISTANCE.pow(2) as f32;
    let ghost_blends: &[(Rgb, Rgb)] = if ghost { &GHOST_BLENDS } else { &[] };
    BLENDS
        .iter()
        .chain(ghost_blends)
        .any(|&(p, q)| segment_dist_sq(c, p, q) <= near)
}

fn chroma(c: Rgb) -> u8 {
    c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b)
}

/// Rec. 709 luma on the encoded values, 0..=255.
fn luma(c: Rgb) -> u32 {
    (c.r as u32 * 2126 + c.g as u32 * 7152 + c.b as u32 * 722) / 10_000
}

/// "Visibly coloured" for the `colours_flattened` lint.
const COLOURED_MIN_CHROMA: u8 = 64;

/// A decoded raster in 8-bit straight (non-premultiplied) channels, tightly packed.
pub(crate) struct Pixels {
    pub width: usize,
    pub height: usize,
    /// 1 = grey, 2 = grey+alpha, 3 = RGB, 4 = RGBA.
    pub channels: usize,
    pub data: Vec<u8>,
}

impl Pixels {
    fn has_alpha(&self) -> bool {
        self.channels == 2 || self.channels == 4
    }

    fn pixel(px: &[u8]) -> (Rgb, u8) {
        match px.len() {
            1 => (Rgb::new(px[0], px[0], px[0]), 255),
            2 => (Rgb::new(px[0], px[0], px[0]), px[1]),
            3 => (Rgb::new(px[0], px[1], px[2]), 255),
            _ => (Rgb::new(px[0], px[1], px[2]), px[3]),
        }
    }
}

/// Ink coverage (0..=255, 255 = shape) plus the facts the ink rule noticed.
pub(crate) struct Ink {
    /// Row-major, same size as the input.
    pub coverage: Vec<u8>,
    /// Enough guide/reference coloured pixels to say they were left visible.
    pub guides_visible: bool,
    /// Colours were collapsed into one silhouette (coloured ink, or light areas dropped).
    pub colours_flattened: bool,
    /// A lot of the drawing is semi-transparent (soft brushes, low layer opacity).
    pub semi_transparent: bool,
}

/// What the ink rule decided about one pixel.
#[derive(Debug, Clone, Copy, Default)]
struct Class {
    coverage: u8,
    /// Solid and close to a (chromatic) template colour.
    evidence: bool,
    /// Ink in a clear colour rather than black/grey.
    coloured: bool,
    /// Solid but near white, so it became a hole (alpha rasters only).
    light_dropped: bool,
    /// Ink at least 10% opaque (alpha rasters only).
    visible: bool,
    /// Ink between 10% and 90% opaque (alpha rasters only).
    soft: bool,
}

/// Composite over white (also flattens any non-meaningful alpha).
fn over_white(rgb: Rgb, a: u8) -> Rgb {
    let over =
        |ch: u8| -> u8 { ((ch as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8 };
    Rgb::new(over(rgb.r), over(rgb.g), over(rgb.b))
}

/// `ghost`: the reference ghost is visible, so guide-over-ghost products aren't ink.
fn classify(rgb: Rgb, a: u8, alpha_mode: bool, ghost: bool) -> Class {
    let near = NEAR_DISTANCE.pow(2);
    let evidence = |c: Rgb| {
        min_dist_sq(c, ghost) <= EVIDENCE_DISTANCE.pow(2) && chroma(c) >= EVIDENCE_MIN_CHROMA
    };
    if alpha_mode {
        if a == 0 {
            return Class::default();
        }
        let solid = a >= 128;
        if dist_sq(rgb, WHITE) <= near {
            return Class {
                light_dropped: solid,
                ..Class::default()
            };
        }
        if near_template(rgb, ghost) {
            return Class {
                evidence: solid && evidence(rgb),
                ..Class::default()
            };
        }
        Class {
            coverage: a,
            coloured: solid && chroma(rgb) >= COLOURED_MIN_CHROMA,
            visible: a >= 26,
            soft: (26..=229).contains(&a),
            ..Class::default()
        }
    } else {
        let rgb = over_white(rgb, a);
        if near_template(rgb, ghost) {
            return Class {
                evidence: evidence(rgb),
                ..Class::default()
            };
        }
        Class {
            coverage: (255 - luma(rgb)) as u8,
            coloured: chroma(rgb) >= COLOURED_MIN_CHROMA,
            ..Class::default()
        }
    }
}

/// The colour the ink rule judges a pixel by, and whether it is solid: the raw colour
/// in an alpha raster, the colour composited over white otherwise.
fn judged(rgb: Rgb, a: u8, alpha_mode: bool) -> (Rgb, bool) {
    if alpha_mode {
        (rgb, a >= 128)
    } else {
        (over_white(rgb, a), true)
    }
}

/// At least 0.1% of the pixels are solid and the ghost's colour: the reference layer was
/// left visible.
fn ghost_visible(px: &Pixels, alpha_mode: bool) -> bool {
    let total = px.width * px.height;
    let mut ghost_px = 0usize;
    let mut last: Option<((Rgb, u8), bool)> = None;
    for p in px.data.chunks_exact(px.channels) {
        let key = Pixels::pixel(p);
        let is_ghost = match last {
            Some((k, g)) if k == key => g,
            _ => {
                let (c, solid) = judged(key.0, key.1, alpha_mode);
                let g = solid && dist_sq(c, REFERENCE_GHOST) <= EVIDENCE_DISTANCE.pow(2);
                last = Some((key, g));
                g
            }
        };
        ghost_px += usize::from(is_ghost);
    }
    ghost_px > 0 && ghost_px * 1000 >= total * EVIDENCE_MIN_PER_MILLE
}

/// The template-aware ink rule.
///
/// * **Alpha raster** (at least 0.5% transparent and 0.1% opaque pixels): coverage is the
///   pixel's alpha, unless its colour is near a template colour or near white (white
///   details become holes).
/// * **Opaque raster** (everything else, including all JPEGs): composite over white, then
///   coverage is the darkness `255 - luma`, unless the colour is near a template colour.
///   After resampling, coverage is thresholded at 50%, i.e. luma < 0.5 is ink.
pub(crate) fn ink(px: &Pixels) -> Ink {
    let total = px.width * px.height;
    let c = px.channels;
    let alpha_mode = if px.has_alpha() {
        let (mut transparent, mut opaque) = (0usize, 0usize);
        for p in px.data.chunks_exact(c) {
            if p[c - 1] < 128 {
                transparent += 1;
            } else {
                opaque += 1;
            }
        }
        transparent * 200 >= total && opaque * 1000 >= total
    } else {
        false
    };
    let ghost = ghost_visible(px, alpha_mode);

    let mut coverage = Vec::with_capacity(total);
    let (mut evidence_px, mut coloured_px, mut light_dropped_px) = (0usize, 0usize, 0usize);
    let (mut soft_px, mut visible_px) = (0usize, 0usize);
    // Drawings are mostly long runs of identical pixels; classify each colour once per run.
    let mut last: Option<((Rgb, u8), Class)> = None;
    for p in px.data.chunks_exact(c) {
        let key = Pixels::pixel(p);
        let class = match last {
            Some((k, class)) if k == key => class,
            _ => {
                let class = classify(key.0, key.1, alpha_mode, ghost);
                last = Some((key, class));
                class
            }
        };
        coverage.push(class.coverage);
        evidence_px += usize::from(class.evidence);
        coloured_px += usize::from(class.coloured);
        light_dropped_px += usize::from(class.light_dropped);
        visible_px += usize::from(class.visible);
        soft_px += usize::from(class.soft);
    }

    let per_mille = |n: usize| n * 1000 >= total;
    Ink {
        coverage,
        guides_visible: evidence_px > 0 && evidence_px * 1000 >= total * EVIDENCE_MIN_PER_MILLE,
        // 1% of the canvas in a clear colour, or 0.1% of it white and dropped.
        colours_flattened: coloured_px * 100 >= total || per_mille(light_dropped_px),
        // More than 5% of the visible drawing, and at least 0.1% of the canvas, is
        // between 10% and 90% opaque (a stray anti-aliased pixel doesn't count).
        semi_transparent: soft_px * 20 > visible_px && per_mille(soft_px),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiply_products_are_dark_enough_to_need_excluding() {
        // Guides over the ghost come out darker than either colour; at least one is
        // below 50% luma, which is why the products are excluded when the ghost shows,
        // and why they must not be otherwise.
        let darkest = GHOST_PRODUCTS.iter().map(|&c| luma(c)).min().unwrap_or(255);
        assert!(darkest < 128, "{darkest}");
    }

    #[test]
    fn dark_navy_and_crimson_are_ink_unless_the_ghost_shows() {
        // Each is within 24 of a guide-over-ghost product but far from every template
        // colour.
        for c in [Rgb::new(20, 70, 130), Rgb::new(150, 25, 70)] {
            let alone = classify(c, 255, false, false);
            assert!(alone.coverage > 128 && !alone.evidence, "{}", c.to_hex());
            let with_ghost = classify(c, 255, false, true);
            assert!(
                with_ghost.coverage == 0 && with_ghost.evidence,
                "{}",
                c.to_hex()
            );
        }
    }

    #[test]
    fn greys_are_never_guide_evidence() {
        for g in 0..=255u8 {
            let c = Rgb::new(g, g, g);
            assert!(chroma(c) < EVIDENCE_MIN_CHROMA, "grey {g}");
        }
        assert!(chroma(REFERENCE_GHOST) >= EVIDENCE_MIN_CHROMA);
        for c in GUIDE_COLOURS {
            assert!(chroma(c) >= EVIDENCE_MIN_CHROMA, "{}", c.to_hex());
        }
    }

    #[test]
    fn black_ink_is_ink_and_guides_are_not() {
        let mut data = Vec::new();
        for c in [
            Rgb::new(0, 0, 0),
            LABEL,
            ATTACH_LABEL,
            GUIDE,
            REFERENCE_GHOST,
        ] {
            data.extend_from_slice(&[c.r, c.g, c.b]);
        }
        let ink = ink(&Pixels {
            width: 5,
            height: 1,
            channels: 3,
            data,
        });
        assert_eq!(ink.coverage, vec![255, 0, 0, 0, 0]);
        assert!(ink.guides_visible);
        assert!(!ink.semi_transparent && !ink.colours_flattened);
    }
}
