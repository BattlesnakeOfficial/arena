//! The template colour contract and the template-aware ink rule.
//!
//! The head/tail templates (PSD for Procreate, SVG for vector apps) draw their guides in
//! five light, saturated colours and the optional reference shapes in one light "ghost"
//! colour. These are chosen so that the studio can tell them apart from the artist's ink:
//! an artist who forgets to hide the guides before exporting still gets a clean result,
//! plus a `guides_visible` info lint.
//!
//! Every template colour has a luma above 0.5, so no guide pixel is ink by itself, and
//! the exclusion zone around each colour (see [`NEAR_DISTANCE`]) stays clear of the dark
//! inks artists draw with: navy, sapphire, royal blue, steel blue, crimson, raspberry.
//! (The first palette used dark blues and a crimson for its labels, and their exclusion
//! zones swallowed about a third of all saturated dark blues and half the crimsons.) The
//! text colours are the darkest, for about 3.2:1 contrast on white.
//!
//! The template generator must use exactly these colours. The guides layer uses the
//! Multiply blend mode, so guides that sit over the reference ghost come out as the
//! product of the two colours. The labels' products are just dark enough to be ink (luma
//! 0.42-0.43; the others are 0.53-0.67), so the products are only excluded when the
//! reference itself is clearly visible in the image (see [`GHOST_MIN_PERCENT`]), and then
//! only within the tighter [`PRODUCT_DISTANCE`] (the products are exact colours, so a
//! steel-blue or slate-blue drawing over a visible reference stays ink). The exclusion
//! also covers anti-aliased blends of each colour towards white (or of each product
//! towards the ghost and towards its guide colour).
//!
//! JPEG noise scatters single pixels across those boundaries: a label pixel turns just
//! dark enough to be ink, or a pixel inside a steel-blue fill lands in a label colour's
//! zone. A dark pixel whose call is close (see [`VOTE_MARGIN`]) goes with its neighbours
//! instead (see [`vote`]), so neither becomes a speck or a pinhole.

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
/// Canvas border and the horizontal centre (spine) line.
pub const GUIDE: Rgb = Rgb::new(0x9e, 0xb0, 0xf4);
/// Every label but the attach edge's, the direction arrow and the detail-size swatches.
pub const LABEL: Rgb = Rgb::new(0x7e, 0x8f, 0xb8);
/// The attach edge (neck/body joint) band and ticks.
pub const ATTACH: Rgb = Rgb::new(0xff, 0x94, 0xb8);
/// The attach edge label.
pub const ATTACH_LABEL: Rgb = Rgb::new(0xb4, 0x80, 0x8c);

/// The five guide colours, in template order.
pub const GUIDE_COLOURS: [Rgb; 5] = [GRID, GUIDE, LABEL, ATTACH, ATTACH_LABEL];

/// Fill of the optional reference heads/tails. Baked into the pixels (not layer opacity)
/// so it stays a light ghost in apps that ignore layer opacity.
pub const REFERENCE_GHOST: Rgb = Rgb::new(0xc8, 0xc2, 0xd4);

/// Pixels within this RGB distance of a template colour are never ink.
pub const NEAR_DISTANCE: u32 = 48;

/// With a visible reference, pixels within this RGB distance of a guide-over-ghost
/// product (or of a blend of one with the ghost or with its guide colour) are not ink
/// either. Tighter than [`NEAR_DISTANCE`]: the products sit among the inks people draw
/// with (steel blue is 42 from the blends of the labels' slate-blue product, medium
/// purple 35 and slate blue 48 from the border's), and a visible reference is the one
/// case where both are in the picture. The products are exact colours, within a few
/// levels in a PNG, and 32 still holds them through JPEG noise.
pub const PRODUCT_DISTANCE: u32 = 32;

/// A solid, dark pixel inside a template colour's zone, or within this RGB distance
/// beyond it (any chroma), is a close call, settled by its neighbours (see [`vote`]).
pub const VOTE_MARGIN: u32 = 24;

/// Pixels within this RGB distance of a guide colour (and visibly coloured, see
/// [`EVIDENCE_MIN_CHROMA`]) count as evidence that guides were exported. Tighter than
/// [`NEAR_DISTANCE`] so anti-aliased grey edges of black ink never count. The same
/// distance from the ghost makes a pixel ghost-coloured.
pub const EVIDENCE_DISTANCE: u32 = 24;

/// A guide-coloured pixel only counts as evidence when no pixel up to
/// [`EVIDENCE_FLAT_RADIUS`] away along its row or column is darker than it by more than
/// this (luma, 0..=255, composited over white).
///
/// Guides are flat colour with crisp edges, so the core of every line, letter and
/// swatch is as dark as anything around it. The light guide colours also lie on the
/// anti-aliased edges of darker inks of the same hue (a navy or royal-blue edge fading to
/// white passes right through the labels' slate blue and the border's periwinkle), but
/// an edge is a ramp, and a few pixels along it the colour is a good deal darker.
pub const EVIDENCE_MAX_DARKER_NEIGHBOUR: u32 = 16;

/// How far [`EVIDENCE_MAX_DARKER_NEIGHBOUR`] looks. A soft edge blurred over about 8 px
/// (σ ≈ 3.3) climbs about 18 levels a pixel at its steepest, but only 13 along a
/// diagonal step, so one pixel isn't enough; three is 39 even along the diagonal, and
/// covers edges about twice as soft.
pub const EVIDENCE_FLAT_RADIUS: usize = 3;

/// Minimum `max(r,g,b) - min(r,g,b)` for a pixel to count as guide evidence or as
/// ghost-coloured. The ghost has chroma 18; greys (anti-aliasing of black ink, pencil,
/// light-grey paper or backgrounds) have about 0.
pub const EVIDENCE_MIN_CHROMA: u8 = 12;

/// Share of all pixels that must be guide evidence (or the inside of a visible
/// reference, see [`ghost_core_pixels`]) before `guides_visible` fires (0.1%).
const EVIDENCE_MIN_PER_MILLE: usize = 1;

/// Share of all pixels that must be the inside of a visible reference before the
/// guide-over-ghost products stop being ink (1%). A visible reference covers tens of
/// percent; a navy drawing's anti-aliased edge, which passes close to the ghost's
/// colour, covers about 0.1% at the template's 1000 px and is never "inside" anyway.
pub const GHOST_MIN_PERCENT: usize = 1;

/// Every guide colour multiplied over the ghost: guides drawn over a visible reference.
/// Not ink either, but only when the ghost is visible (the labels' are dark).
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

/// ... and, when the ghost is visible, every guide-over-ghost product towards the ghost
/// (a guide's edge over the reference) and towards its own guide colour (a guide crossing
/// the reference's edge).
const GHOST_BLENDS: [(Rgb, Rgb); 10] = [
    (GHOST_PRODUCTS[0], REFERENCE_GHOST),
    (GHOST_PRODUCTS[1], REFERENCE_GHOST),
    (GHOST_PRODUCTS[2], REFERENCE_GHOST),
    (GHOST_PRODUCTS[3], REFERENCE_GHOST),
    (GHOST_PRODUCTS[4], REFERENCE_GHOST),
    (GHOST_PRODUCTS[0], GRID),
    (GHOST_PRODUCTS[1], GUIDE),
    (GHOST_PRODUCTS[2], LABEL),
    (GHOST_PRODUCTS[3], ATTACH),
    (GHOST_PRODUCTS[4], ATTACH_LABEL),
];

fn dist_sq(a: Rgb, b: Rgb) -> u32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).unsigned_abs();
    d(a.r, b.r).pow(2) + d(a.g, b.g).pow(2) + d(a.b, b.b).pow(2)
}

/// Squared distance to the nearest guide colour (or guide-over-ghost product, when the
/// reference is visible). The ghost itself is judged by [`ghost_core_pixels`].
fn guide_dist_sq(c: Rgb, ghost: bool) -> u32 {
    let products: &[Rgb] = if ghost { &GHOST_PRODUCTS } else { &[] };
    GUIDE_COLOURS
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

/// Within [`NEAR_DISTANCE`] of a template colour or of an anti-aliased blend of one (or,
/// with a visible reference, within [`PRODUCT_DISTANCE`] of a guide-over-ghost product
/// or a blend of one).
///
/// Greys (chroma below [`EVIDENCE_MIN_CHROMA`]) are only checked against the ghost's
/// fade to white: the light greys (from luma 0.69) a reference's soft edge passes
/// through. No guide colour is grey, but grey is what the anti-aliased edge of black ink
/// is, and what a pencil or a grey brush draws. The labels' slate blue and dusty rose are
/// within 48 of greys from 133 to 175, so without this a black drawing's soft edge would
/// lose part of its coverage, and a mid-grey drawing on a transparent canvas would vanish.
fn near_template(c: Rgb, ghost: bool) -> bool {
    let near = NEAR_DISTANCE.pow(2) as f32;
    if chroma(c) < EVIDENCE_MIN_CHROMA {
        return segment_dist_sq(c, REFERENCE_GHOST, WHITE) <= near;
    }
    within(c, ghost, near, PRODUCT_DISTANCE.pow(2) as f32)
}

/// Within `near` (squared) of a blend in [`BLENDS`] or, with a visible reference, within
/// `product` (squared) of one in [`GHOST_BLENDS`], whatever the chroma.
fn within(c: Rgb, ghost: bool, near: f32, product: f32) -> bool {
    BLENDS
        .iter()
        .any(|&(p, q)| segment_dist_sq(c, p, q) <= near)
        || (ghost
            && GHOST_BLENDS
                .iter()
                .any(|&(p, q)| segment_dist_sq(c, p, q) <= product))
}

/// Within [`VOTE_MARGIN`] beyond a template colour's zone, of any chroma (JPEG's
/// chroma subsampling greys out thin coloured lines).
fn close_to_template(c: Rgb, ghost: bool) -> bool {
    let near = (NEAR_DISTANCE + VOTE_MARGIN).pow(2) as f32;
    let product = (PRODUCT_DISTANCE + VOTE_MARGIN).pow(2) as f32;
    within(c, ghost, near, product)
}

fn chroma(c: Rgb) -> u8 {
    c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b)
}

/// Rec. 709 luma on the encoded values, 0..=255.
pub(crate) fn luma(c: Rgb) -> u32 {
    (c.r as u32 * 2126 + c.g as u32 * 7152 + c.b as u32 * 722) / 10_000
}

/// "Visibly coloured" for the `colours_flattened` lint.
const COLOURED_MIN_CHROMA: u8 = 64;

/// A visibly coloured paint (not black, white or grey): the `colours_flattened` rule,
/// shared by raster and SVG input.
pub(crate) fn coloured(c: Rgb) -> bool {
    chroma(c) >= COLOURED_MIN_CHROMA
}

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
    /// Solid and close to a (chromatic) guide colour or, with a visible reference, a
    /// guide-over-ghost product. [`ink`] also requires it to sit in a flat core (see
    /// [`EVIDENCE_MAX_DARKER_NEIGHBOUR`]).
    evidence: bool,
    /// Ink in a clear colour rather than black/grey.
    coloured: bool,
    /// Solid but near white, so it became a hole (alpha rasters only).
    light_dropped: bool,
    /// Ink at least 10% opaque (alpha rasters only).
    visible: bool,
    /// Ink between 10% and 90% opaque (alpha rasters only).
    soft: bool,
    /// What [`vote`] needs to know: `VOTE_*` flags.
    vote: u8,
}

/// [`Class::vote`]: not ink because of its colour (near a template colour).
const VOTE_TEMPLATE: u8 = 1;
/// [`Class::vote`]: ink, solid (opaque raster: at least half dark, luma below 128; alpha
/// raster: at least half opaque).
const VOTE_INK: u8 = 2;
/// [`Class::vote`]: a close call that [`vote`] settles: a dark pixel near a template
/// colour (so not ink), or ink within [`VOTE_MARGIN`] of being near one.
const VOTE_CLOSE: u8 = 4;

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
        guide_dist_sq(c, ghost) <= EVIDENCE_DISTANCE.pow(2) && chroma(c) >= EVIDENCE_MIN_CHROMA
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
        // Close calls are judged on solid, dark colours, as in an opaque raster.
        let dark = solid && luma(rgb) < 128;
        if near_template(rgb, ghost) {
            return Class {
                evidence: solid && evidence(rgb),
                vote: VOTE_TEMPLATE | if dark { VOTE_CLOSE } else { 0 },
                ..Class::default()
            };
        }
        let vote = match (solid, dark && close_to_template(rgb, ghost)) {
            (true, true) => VOTE_INK | VOTE_CLOSE,
            (true, false) => VOTE_INK,
            _ => 0,
        };
        Class {
            coverage: a,
            coloured: solid && coloured(rgb),
            visible: a >= 26,
            soft: (26..=229).contains(&a),
            vote,
            ..Class::default()
        }
    } else {
        let rgb = over_white(rgb, a);
        let dark = luma(rgb) < 128;
        if near_template(rgb, ghost) {
            return Class {
                evidence: evidence(rgb),
                vote: VOTE_TEMPLATE | if dark { VOTE_CLOSE } else { 0 },
                ..Class::default()
            };
        }
        let vote = match (dark, dark && close_to_template(rgb, ghost)) {
            (true, true) => VOTE_INK | VOTE_CLOSE,
            (true, false) => VOTE_INK,
            _ => 0,
        };
        Class {
            coverage: (255 - luma(rgb)) as u8,
            coloured: coloured(rgb),
            vote,
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

/// Solid, visibly coloured and within [`EVIDENCE_DISTANCE`] of the ghost.
fn ghost_coloured(c: Rgb, solid: bool) -> bool {
    solid
        && chroma(c) >= EVIDENCE_MIN_CHROMA
        && dist_sq(c, REFERENCE_GHOST) <= EVIDENCE_DISTANCE.pow(2)
}

/// Pixels inside a visible reference: ghost-coloured, with their whole 3x3 neighbourhood
/// ghost-coloured too.
///
/// Colour alone isn't enough. Light greys from about 190 to 213 (a soft black edge, a
/// light-grey background) are within 24 of the ghost; the chroma floor rules them out.
/// Part of the anti-aliased edge of a dark-blue drawing on white is within 24 too, and
/// coloured (chroma about 27), but that edge is a pixel or two wide, so it never fills a
/// 3x3 block.
fn ghost_core_pixels(px: &Pixels, alpha_mode: bool) -> usize {
    let (w, h, c) = (px.width, px.height, px.channels);
    if w < 3 || h < 3 {
        return 0;
    }
    // Ghost-coloured flags of the last three rows (row y lives in rows[y % 3]).
    let mut rows = [vec![false; w], vec![false; w], vec![false; w]];
    let mut last: Option<((Rgb, u8), bool)> = None;
    let mut core = 0usize;
    for (y, line) in px.data.chunks_exact(w * c).take(h).enumerate() {
        let row = &mut rows[y % 3];
        for (flag, p) in row.iter_mut().zip(line.chunks_exact(c)) {
            let key = Pixels::pixel(p);
            *flag = match last {
                Some((k, g)) if k == key => g,
                _ => {
                    let (rgb, solid) = judged(key.0, key.1, alpha_mode);
                    let g = ghost_coloured(rgb, solid);
                    last = Some((key, g));
                    g
                }
            };
        }
        if y >= 2 {
            // Count the middle row's pixels whose 3x3 block is all ghost: runs of columns
            // that are ghost-coloured in all three rows.
            let [r0, r1, r2] = &rows;
            let mut run = 0usize;
            for ((&a, &b), &c) in r0.iter().zip(r1).zip(r2) {
                if a && b && c {
                    run += 1;
                    core += usize::from(run >= 3);
                } else {
                    run = 0;
                }
            }
        }
    }
    core
}

/// Luma of pixel `i` composited over white: how dark it looks.
fn darkness_luma(px: &Pixels, i: usize) -> u32 {
    let c = px.channels;
    let (rgb, a) = Pixels::pixel(&px.data[i * c..i * c + c]);
    luma(over_white(rgb, a))
}

/// A pixel up to [`EVIDENCE_FLAT_RADIUS`] away from pixel `i`, along its row or column,
/// is darker than it by more than [`EVIDENCE_MAX_DARKER_NEIGHBOUR`]: it sits on a ramp
/// (an anti-aliased or soft edge), not in the flat core of a guide.
fn has_darker_neighbour(px: &Pixels, i: usize) -> bool {
    let (w, h) = (px.width, px.height);
    let (x, y) = (i % w, i / w);
    let own = darkness_luma(px, i);
    (1..=EVIDENCE_FLAT_RADIUS).any(|d| {
        let left = (x >= d).then(|| i - d);
        let right = (x + d < w).then(|| i + d);
        let up = (y >= d).then(|| i - d * w);
        let down = (y + d < h).then(|| i + d * w);
        [left, right, up, down]
            .into_iter()
            .flatten()
            .any(|n| darkness_luma(px, n) + EVIDENCE_MAX_DARKER_NEIGHBOUR < own)
    })
}

/// Settle the close calls in row `y` by their 3x3 neighbourhood
/// (`rows` holds the [`Class::vote`] flags of rows `y - 1`, `y` and `y + 1` at index
/// `row % 3`; rows outside the image don't count).
///
/// * A dark pixel near a template colour becomes ink when most of its neighbours are
///   ink (at least 4, and more ink than template): JPEG noise inside a steel-blue fill,
///   which would otherwise be a pinhole.
/// * Dark ink near a template colour stops being ink when its neighbours are mostly
///   template (at least 4, and more than twice as many as ink): a label pixel that JPEG
///   ringing darkened past its colour's zone, which would otherwise be a speck.
///
/// Both need a clear majority, so the anti-aliased edge of an ink (half ink, half
/// background or template) keeps the call its colour gets, and black ink, far from
/// every template colour, is never a close call.
fn vote(px: &Pixels, alpha_mode: bool, coverage: &mut [u8], rows: &[Vec<u8>; 3], y: usize) {
    let (w, h) = (px.width, px.height);
    let row = &rows[y % 3];
    for (x, &f) in row.iter().enumerate() {
        if f & VOTE_CLOSE == 0 {
            continue;
        }
        let (mut template, mut ink) = (0u32, 0u32);
        let x0 = x.saturating_sub(1);
        for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
            let window = &rows[ny % 3][x0..=(x + 1).min(w - 1)];
            for (nx, &n) in (x0..).zip(window) {
                if (nx, ny) != (x, y) {
                    template += u32::from(n & VOTE_TEMPLATE != 0);
                    ink += u32::from(n & VOTE_INK != 0);
                }
            }
        }
        let i = y * w + x;
        if f & VOTE_TEMPLATE != 0 {
            if ink >= 4 && ink > template {
                coverage[i] = if alpha_mode {
                    px.data[i * px.channels + px.channels - 1]
                } else {
                    (255 - darkness_luma(px, i)) as u8
                };
            }
        } else if template >= 4 && template > 2 * ink {
            coverage[i] = 0;
        }
    }
}

/// The template-aware ink rule.
///
/// * **Alpha raster** (at least 0.5% transparent and 0.1% opaque pixels): coverage is the
///   pixel's alpha, unless its colour is near a template colour or near white (white
///   details become holes).
/// * **Opaque raster** (everything else, including all JPEGs): composite over white, then
///   coverage is the darkness `255 - luma`, unless the colour is near a template colour.
///   After resampling, coverage is thresholded at 50%, i.e. luma < 0.5 is ink.
///
/// In both, solid dark pixels whose call is close go with their neighbours (see [`vote`]).
///
/// The guide-over-ghost products count as template colours only when at least
/// [`GHOST_MIN_PERCENT`] of the canvas is inside a visible reference.
///
/// `guides_visible` counts guide-coloured pixels in the flat core of a guide (no
/// neighbour much darker, see [`EVIDENCE_MAX_DARKER_NEIGHBOUR`]), plus the inside of a
/// visible reference.
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
    // The reference counts as visible when at least 1% of the canvas is inside it.
    let ghost_core = ghost_core_pixels(px, alpha_mode);
    let ghost = ghost_core > 0 && ghost_core * 100 >= total * GHOST_MIN_PERCENT;

    let (w, h) = (px.width, px.height);
    if total == 0 {
        return Ink {
            coverage: Vec::new(),
            guides_visible: false,
            colours_flattened: false,
            semi_transparent: false,
        };
    }
    let mut coverage = Vec::with_capacity(total);
    let (mut evidence_px, mut coloured_px, mut light_dropped_px) = (0usize, 0usize, 0usize);
    let (mut soft_px, mut visible_px) = (0usize, 0usize);
    // The vote flags of the last three rows (row y lives in rows[y % 3]).
    let mut rows = [vec![0u8; w], vec![0u8; w], vec![0u8; w]];
    // Drawings are mostly long runs of identical pixels; classify each colour once per run.
    let mut last: Option<((Rgb, u8), Class)> = None;
    for (y, line) in px.data.chunks_exact(w * c).take(h).enumerate() {
        for (x, p) in line.chunks_exact(c).enumerate() {
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
            rows[y % 3][x] = class.vote;
            evidence_px += usize::from(class.evidence && !has_darker_neighbour(px, y * w + x));
            coloured_px += usize::from(class.coloured);
            light_dropped_px += usize::from(class.light_dropped);
            visible_px += usize::from(class.visible);
            soft_px += usize::from(class.soft);
        }
        // Row y - 1 has all its neighbours now.
        if y >= 1 {
            vote(px, alpha_mode, &mut coverage, &rows, y - 1);
        }
    }
    vote(px, alpha_mode, &mut coverage, &rows, h - 1);

    let per_mille = |n: usize| n * 1000 >= total;
    // Guides, or the inside of a reference (even a mostly hidden one), left visible.
    let template_px = evidence_px + ghost_core;
    Ink {
        coverage,
        guides_visible: template_px > 0 && template_px * 1000 >= total * EVIDENCE_MIN_PER_MILLE,
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

    /// Within 24 of the labels' guide-over-ghost products (slate blue and mauve), but far
    /// from every template colour.
    const SLATE: Rgb = Rgb::new(90, 100, 150);
    const MAUVE: Rgb = Rgb::new(135, 85, 110);

    #[test]
    fn every_template_colour_is_lighter_than_ink() {
        // Luma above 0.5 with room to spare: a guide pixel is never ink by itself, and
        // nothing near one is dark ink.
        for c in GUIDE_COLOURS.iter().chain([&REFERENCE_GHOST]) {
            assert!(luma(*c) >= 138, "{} has luma {}", c.to_hex(), luma(*c));
        }
    }

    #[test]
    fn colours_near_the_products_are_ink_unless_the_ghost_shows() {
        assert_eq!(GHOST_PRODUCTS[2], Rgb::new(99, 109, 153));
        assert_eq!(GHOST_PRODUCTS[4], Rgb::new(141, 97, 116));
        for c in [SLATE, MAUVE] {
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
            // Greys darker than the ghost's soft edge are ink like any other, judged by
            // luma (or alpha), even where a label colour is close.
            assert_eq!(near_template(c, true), g >= 176, "grey {g}");
            if g < 176 {
                assert_eq!(classify(c, 255, false, true).coverage, 255 - g, "grey {g}");
                assert_eq!(classify(c, 200, true, true).coverage, 200, "grey {g}");
            }
        }
        assert!(chroma(REFERENCE_GHOST) >= EVIDENCE_MIN_CHROMA);
        for c in GUIDE_COLOURS {
            assert!(chroma(c) >= EVIDENCE_MIN_CHROMA, "{}", c.to_hex());
        }
    }

    fn rgb_pixels(w: usize, h: usize, at: impl Fn(usize, usize) -> Rgb) -> Pixels {
        let mut data = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let c = at(x, y);
                data.extend_from_slice(&[c.r, c.g, c.b]);
            }
        }
        Pixels {
            width: w,
            height: h,
            channels: 3,
            data,
        }
    }

    #[test]
    fn only_the_inside_of_a_ghost_coloured_area_is_a_visible_reference() {
        const NAVY: Rgb = Rgb::new(20, 70, 130);
        // A navy edge blended 70% towards white: 19 from the ghost, and coloured.
        let edge = Rgb::new(184, 200, 221);
        assert!(ghost_coloured(edge, true));
        // Lines of it two pixels wide (a soft edge) are never inside anything...
        let lines = rgb_pixels(100, 100, |x, _| if x % 10 < 2 { edge } else { NAVY });
        assert_eq!(ghost_core_pixels(&lines, false), 0);
        let ink_lines = ink(&lines);
        assert!(ink_lines.coverage.iter().filter(|&&c| c > 128).count() >= 8000);
        assert!(!ink_lines.guides_visible);
        // ...and light grey isn't the ghost, however much of it there is.
        let grey = rgb_pixels(100, 100, |x, _| {
            if x < 50 {
                Rgb::new(204, 204, 204)
            } else {
                NAVY
            }
        });
        assert_eq!(ghost_core_pixels(&grey, false), 0);
        let ink_grey = ink(&grey);
        assert!(ink_grey.coverage[99] > 128 && !ink_grey.guides_visible);
        // A 3x3 block has one inside pixel; a 5x4 block has six.
        let block = |x0: usize, y0: usize, bw: usize, bh: usize| {
            rgb_pixels(10, 10, move |x, y| {
                if (x0..x0 + bw).contains(&x) && (y0..y0 + bh).contains(&y) {
                    REFERENCE_GHOST
                } else {
                    WHITE
                }
            })
        };
        assert_eq!(ghost_core_pixels(&block(2, 2, 3, 3), false), 1);
        assert_eq!(ghost_core_pixels(&block(0, 3, 5, 4), false), 6);
        assert_eq!(ghost_core_pixels(&block(0, 0, 10, 10), false), 64);
    }

    #[test]
    fn products_are_ink_until_one_percent_of_the_canvas_is_inside_the_reference() {
        // 100x100 slate blue with a ghost block; the block's inside is (side - 2)².
        let canvas = |side: usize| {
            rgb_pixels(100, 100, move |x, y| {
                if x < side && y < side {
                    REFERENCE_GHOST
                } else {
                    SLATE
                }
            })
        };
        // 11x11: 81 inside pixels, 0.81%. The slate is ink, and the reference is
        // reported.
        let small = ink(&canvas(11));
        assert_eq!(small.coverage[99 * 100 + 99], 255 - luma(SLATE) as u8);
        assert!(small.guides_visible);
        // 12x12: 100 inside pixels, 1%. The slate is now a guide over the ghost.
        let big = ink(&canvas(12));
        assert_eq!(big.coverage[99 * 100 + 99], 0);
        assert!(big.guides_visible);
        // 4x4 (4 inside pixels, 0.04%): too little to report.
        assert!(!ink(&canvas(4)).guides_visible);
    }

    #[test]
    fn guides_are_reported_from_a_tenth_of_a_percent_of_the_canvas() {
        let labels = |n: usize| {
            rgb_pixels(
                100,
                100,
                move |x, y| if y * 100 + x < n { LABEL } else { WHITE },
            )
        };
        assert!(ink(&labels(10)).guides_visible);
        assert!(!ink(&labels(9)).guides_visible);
    }

    #[test]
    fn an_ink_edge_through_a_guide_colour_is_not_guide_evidence() {
        // Navy fading to white over 8 px: part of the ramp is within 24 of the labels'
        // slate blue, but every step is about 24 levels darker on the navy side.
        const NAVY: Rgb = Rgb::new(20, 70, 130);
        let ramp = |x: usize| {
            let u = (x.saturating_sub(40) as f32 / 8.0).min(1.0);
            let ch = |a: u8| (a as f32 + (255.0 - a as f32) * u).round() as u8;
            Rgb::new(ch(NAVY.r), ch(NAVY.g), ch(NAVY.b))
        };
        let edges = rgb_pixels(100, 100, |x, _| ramp(x));
        let near_label = (40..=48)
            .filter(|&x| classify(ramp(x), 255, false, false).evidence)
            .count();
        assert!(near_label > 0, "the ramp passes the label colour");
        assert!(!ink(&edges).guides_visible);
        // A label stroke as thin as one pixel, on white, is.
        let stroke = rgb_pixels(
            100,
            100,
            |x, y| {
                if x == 70 && y < 20 { LABEL } else { ramp(x) }
            },
        );
        assert!(ink(&stroke).guides_visible);
    }

    #[test]
    fn black_ink_is_ink_and_guides_are_not() {
        let mut data = Vec::new();
        // The guides sit more than EVIDENCE_FLAT_RADIUS from the black, so the ones in a
        // flat core (the labels: no neighbour much darker) are reported.
        for c in [
            Rgb::new(0, 0, 0),
            WHITE,
            WHITE,
            WHITE,
            LABEL,
            ATTACH_LABEL,
            GUIDE,
            REFERENCE_GHOST,
        ] {
            data.extend_from_slice(&[c.r, c.g, c.b]);
        }
        let ink = ink(&Pixels {
            width: 8,
            height: 1,
            channels: 3,
            data,
        });
        assert_eq!(ink.coverage, vec![255, 0, 0, 0, 0, 0, 0, 0]);
        assert!(ink.guides_visible);
        assert!(!ink.semi_transparent && !ink.colours_flattened);
    }
}
