//! The official Battlesnake wordmark, parsed once from the two vendored SVGs
//! (DEV-1990): `static/brand/logo-light.svg` (ink #1c1e21) and
//! `static/brand/logo-dark.svg` (ink #ffffff) — geometry identical, fills per
//! file. Geometry is the trademark — it is used exactly as vendored; only the
//! two fills vary by surface.
//!
//! `WORDMARK_PNG_PATH` and `WORDMARK_DARK_PNG_PATH` are embedded in outgoing
//! emails and must stay live public URLs forever — rename or auth-gate them
//! and every email ever sent loses its header.

use std::sync::LazyLock;

use color_eyre::eyre::{Context as _, eyre};
use tiny_skia::{Color, FillRule, Pixmap, Rect, Transform};

use crate::og::canvas::{Canvas, solid};

pub const WORDMARK_PNG_PATH: &str = "/brand/wordmark.png";
pub const WORDMARK_DARK_PNG_PATH: &str = "/brand/wordmark-dark.png";
/// Email header display sizes (attributes); each PNG is 3× its display size.
pub const EMAIL_IMG_WIDTH: &str = "121";
pub const EMAIL_IMG_HEIGHT: &str = "48";
pub const EMAIL_DARK_IMG_WIDTH: &str = "113";
pub const EMAIL_DARK_IMG_HEIGHT: &str = "40";
const EMAIL_PNG_WIDTH: u32 = 363;
const EMAIL_PNG_HEIGHT: u32 = 144;
const EMAIL_DARK_PNG_WIDTH: u32 = 339;
const EMAIL_DARK_PNG_HEIGHT: u32 = 120;
/// The `.email-card` background (`email/messages.rs`) — the light PNG's plate.
pub(crate) const EMAIL_PLATE_RGB: (u8, u8, u8) = (0xFC, 0xFB, 0xF8);
/// The brand pink, identical in both vendored files; the parse's pink anchor.
const PINK_RGB: (u8, u8, u8) = (0xE7, 0x0B, 0x77);

/// The absolute URLs embedded in every outgoing email header image — the
/// permanent public locations of the wordmark PNGs (mirrors
/// `routes::og::customization_png_url`'s use of `ARENA_PUBLIC_BASE_URL`).
pub fn wordmark_png_url() -> String {
    format!(
        "{}{}",
        crate::config::ARENA_PUBLIC_BASE_URL,
        WORDMARK_PNG_PATH
    )
}

pub fn wordmark_dark_png_url() -> String {
    format!(
        "{}{}",
        crate::config::ARENA_PUBLIC_BASE_URL,
        WORDMARK_DARK_PNG_PATH
    )
}

pub struct Wordmark {
    /// `cls-2`: the BATTLE lettering plus the flanking snake heads.
    ink: Vec<tiny_skia::Path>,
    /// `cls-3`: the pink brush SNAKE.
    pink: Vec<tiny_skia::Path>,
    /// This file's own fills (ink lettering + pink brush), read from the SVG.
    pub official_ink: Color,
    pub official_pink: Color,
    /// Union of the painted paths' tight bounds, in viewBox units.
    bounds: Rect,
}

static LIGHT: LazyLock<Option<Wordmark>> =
    LazyLock::new(|| parse(include_str!("../static/brand/logo-light.svg")).ok());
static DARK: LazyLock<Option<Wordmark>> =
    LazyLock::new(|| parse(include_str!("../static/brand/logo-dark.svg")).ok());
static LIGHT_PNG: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| render_email_pngs().0);
static DARK_PNG: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| render_email_pngs().1);

impl Wordmark {
    /// The light variant's artwork (ink `#1c1e21`, pink `#e70b77`).
    pub fn get() -> cja::Result<&'static Wordmark> {
        LIGHT
            .as_ref()
            .ok_or_else(|| eyre!("Failed to parse the light wordmark artwork"))
    }

    /// The dark variant's artwork (ink `#ffffff`, pink `#e70b77`).
    pub fn get_dark() -> cja::Result<&'static Wordmark> {
        DARK.as_ref()
            .ok_or_else(|| eyre!("Failed to parse the dark wordmark artwork"))
    }

    /// The ink box's width when drawn `height` px tall.
    pub fn ink_width(&self, height: f32) -> f32 {
        self.bounds.width() / self.bounds.height() * height
    }

    fn transform_for(&self, x: f32, center_y: f32, height: f32) -> Transform {
        let s = height / self.bounds.height();
        Transform::from_scale(s, s).post_translate(
            x - self.bounds.left() * s,
            center_y - height / 2.0 - self.bounds.top() * s,
        )
    }

    /// Draw the wordmark with its ink box `height` px tall, left edge at `x`,
    /// vertical centre at `center_y`, with the given fills.
    /// `FillRule::Winding` (SVG default nonzero — the eye holes rely on it).
    pub fn draw(
        &self,
        pixmap: &mut Pixmap,
        x: f32,
        center_y: f32,
        height: f32,
        ink: Color,
        pink: Color,
    ) {
        let t = self.transform_for(x, center_y, height);
        for path in &self.ink {
            pixmap.fill_path(path, &solid(ink), FillRule::Winding, t, None);
        }
        for path in &self.pink {
            pixmap.fill_path(path, &solid(pink), FillRule::Winding, t, None);
        }
    }

    /// Same as [`Wordmark::draw`], onto a social-card canvas.
    pub fn draw_on_canvas(
        &self,
        c: &mut Canvas,
        x: f32,
        center_y: f32,
        height: f32,
        ink: Color,
        pink: Color,
    ) {
        let t = self.transform_for(x, center_y, height);
        for path in &self.ink {
            c.fill_path(path, &solid(ink), FillRule::Winding, t);
        }
        for path in &self.pink {
            c.fill_path(path, &solid(pink), FillRule::Winding, t);
        }
    }
}

/// Parse one vendored artwork file (trusted compile-time input): plain
/// `usvg::Options::default()`. The pink layer is recognized by `PINK_RGB`;
/// the ink layer is whatever the *other* solid colour is — read from the
/// file, never hard-coded. Any structural drift in a re-vendor (unexpected
/// shape count, non-identity transform, stroke, gradient, a second ink
/// colour) fails loudly instead of rendering a mangled trademark.
pub(crate) fn parse(source: &str) -> cja::Result<Wordmark> {
    let tree = usvg::Tree::from_str(source, &usvg::Options::default())
        .wrap_err("Failed to parse the vendored wordmark SVG")?;
    let mut art = Collector::default();
    for child in tree.root().children() {
        art.visit(child)?;
    }

    let ink = art.ink.len();
    let pink = art.pink.len();
    let total = ink + pink + art.unpainted;
    if (ink, pink, art.unpainted, total) != (5, 1, 8, 14) {
        return Err(eyre!(
            "Wordmark artwork structure changed: {ink} ink, {pink} pink, {} unpainted, {total} total (expected 5/1/8/14)",
            art.unpainted
        ));
    }
    let Some(official_ink) = art.official_ink else {
        return Err(eyre!("Wordmark artwork has no ink fill"));
    };
    let Some(official_pink) = art.official_pink else {
        return Err(eyre!("Wordmark artwork has no pink fill"));
    };
    let bounds = union_tight_bounds(art.painted_bounds())?;
    let width = bounds.width();
    let height = bounds.height();
    let aspect = width / height;
    if !(690.0..=710.0).contains(&width)
        || !(240.0..=260.0).contains(&height)
        || !(2.7..=2.95).contains(&aspect)
    {
        return Err(eyre!(
            "Wordmark painted bounds drifted: {width:.1}×{height:.1} (aspect {aspect:.3}); expected ≈697×247"
        ));
    }
    Ok(Wordmark {
        ink: art.ink,
        pink: art.pink,
        official_ink,
        official_pink,
        bounds,
    })
}

/// Every path's tight bounds, painted paths only, in document order — the
/// geometry-equality test pairs them up one-to-one.
fn painted_tight_bounds(wordmark: &Wordmark) -> Vec<Rect> {
    wordmark
        .ink
        .iter()
        .chain(wordmark.pink.iter())
        .map(|p| p.compute_tight_bounds().expect("non-empty path"))
        .collect()
}

#[derive(Default)]
struct Collector {
    ink: Vec<tiny_skia::Path>,
    pink: Vec<tiny_skia::Path>,
    painted_bounds: Vec<Rect>,
    unpainted: usize,
    official_ink: Option<Color>,
    official_pink: Option<Color>,
}

impl Collector {
    /// Visit every `Node::Path` unfiltered — usvg keeps `fill:none` paths in
    /// the tree but marks them invisible, so classifying by `fill()` is the
    /// only correct shape (a visibility filter would count 0 unpainted).
    fn visit(&mut self, node: &usvg::Node) -> cja::Result<()> {
        match node {
            usvg::Node::Group(g) => {
                for child in g.children() {
                    self.visit(child)?;
                }
            }
            usvg::Node::Path(p) => {
                if !p.abs_transform().is_identity() {
                    return Err(eyre!(
                        "Wordmark path carries a non-identity transform — the vendored artwork changed"
                    ));
                }
                match p.fill().map(usvg::Fill::paint) {
                    None => {
                        if p.is_visible() {
                            return Err(eyre!(
                                "Unpainted wordmark path is visible — the vendored artwork gained a stroke"
                            ));
                        }
                        self.unpainted += 1;
                    }
                    Some(usvg::Paint::Color(c)) => {
                        if !p.is_visible() {
                            return Err(eyre!(
                                "Painted wordmark path is invisible — the vendored artwork changed"
                            ));
                        }
                        self.painted_bounds.push(
                            p.data()
                                .compute_tight_bounds()
                                .ok_or_else(|| eyre!("Wordmark path has an empty bounds"))?,
                        );
                        let color = Color::from_rgba8(c.red, c.green, c.blue, 255);
                        if (c.red, c.green, c.blue) == PINK_RGB {
                            self.official_pink = Some(color);
                            self.pink.push(p.data().clone());
                        } else {
                            if self.official_ink.is_some_and(|prev| prev != color) {
                                return Err(eyre!(
                                    "Wordmark artwork has two distinct ink colours — re-vendored?"
                                ));
                            }
                            self.official_ink = Some(color);
                            self.ink.push(p.data().clone());
                        }
                    }
                    Some(_) => {
                        return Err(eyre!(
                            "Wordmark path uses a gradient or pattern fill — only flat fills are official"
                        ));
                    }
                }
            }
            _ => return Err(eyre!("Unexpected node in the wordmark SVG")),
        }
        Ok(())
    }

    fn painted_bounds(&self) -> Vec<Rect> {
        self.painted_bounds.clone()
    }
}

fn union_tight_bounds(bounds: Vec<Rect>) -> cja::Result<Rect> {
    let mut iter = bounds.into_iter();
    let first = iter.next().ok_or_else(|| eyre!("No painted paths"))?;
    let (mut l, mut t, mut r, mut b) = (first.left(), first.top(), first.right(), first.bottom());
    for rect in iter {
        l = l.min(rect.left());
        t = t.min(rect.top());
        r = r.max(rect.right());
        b = b.max(rect.bottom());
    }
    Rect::from_ltrb(l, t, r, b).ok_or_else(|| eyre!("Wordmark bounds are not a valid rect"))
}

/// The email header PNGs, rendered once and cached.
///
/// Light: 363×144, opaque `#FCFBF8` plate (the email card colour, so the
/// logo keeps its own light ground even where dark-mode clients force-invert
/// the card), ink 120px tall centred with 12px margins. Dark: 339×120,
/// transparent, ink 120px tall centred.
fn render_email_pngs() -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let light = (|| -> cja::Result<Vec<u8>> {
        let wordmark = Wordmark::get()?;
        let mut pixmap = Pixmap::new(EMAIL_PNG_WIDTH, EMAIL_PNG_HEIGHT)
            .ok_or_else(|| eyre!("Failed to allocate the light wordmark pixmap"))?;
        pixmap.fill(Color::from_rgba8(
            EMAIL_PLATE_RGB.0,
            EMAIL_PLATE_RGB.1,
            EMAIL_PLATE_RGB.2,
            255,
        ));
        fit_and_draw(
            wordmark,
            &mut pixmap,
            12.0,
            12.0,
            (EMAIL_PNG_WIDTH - 24) as f32,
            (EMAIL_PNG_HEIGHT - 24) as f32,
        );
        encode_rgba_png(&pixmap)
    })();
    let dark = (|| -> cja::Result<Vec<u8>> {
        let wordmark = Wordmark::get_dark()?;
        let mut pixmap = Pixmap::new(EMAIL_DARK_PNG_WIDTH, EMAIL_DARK_PNG_HEIGHT)
            .ok_or_else(|| eyre!("Failed to allocate the dark wordmark pixmap"))?;
        fit_and_draw(
            wordmark,
            &mut pixmap,
            0.0,
            0.0,
            EMAIL_DARK_PNG_WIDTH as f32,
            EMAIL_DARK_PNG_HEIGHT as f32,
        );
        encode_rgba_png(&pixmap)
    })();
    (light.ok(), dark.ok())
}

/// Fit the ink box aspect-preserved and centred into `x..x+w`, `y..y+h`.
fn fit_and_draw(wordmark: &Wordmark, pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32) {
    let bounds = wordmark.bounds;
    let scale = (w / bounds.width()).min(h / bounds.height());
    let height = bounds.height() * scale;
    let width = bounds.width() * scale;
    wordmark.draw(
        pixmap,
        x + (w - width) / 2.0,
        y + h / 2.0,
        height,
        wordmark.official_ink,
        wordmark.official_pink,
    );
}

/// Like `og::canvas::encode_png` but RGBA — the email PNGs keep their alpha
/// (the dark variant is transparent; the encoder there would flatten it).
fn encode_rgba_png(pixmap: &Pixmap) -> cja::Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity((pixmap.width() * pixmap.height() * 4) as usize);
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, pixmap.width(), pixmap.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Balanced);
    let mut writer = encoder
        .write_header()
        .wrap_err("Failed to write wordmark PNG header")?;
    writer
        .write_image_data(&rgba)
        .wrap_err("Failed to write wordmark PNG data")?;
    writer.finish().wrap_err("Failed to finish wordmark PNG")?;
    Ok(png)
}

/// The plated light header image for outgoing email (see [`render_email_pngs`]).
pub fn wordmark_png() -> cja::Result<&'static [u8]> {
    LIGHT_PNG
        .as_deref()
        .ok_or_else(|| eyre!("Failed to render the light wordmark PNG"))
}

/// The transparent dark header image for outgoing email (see [`render_email_pngs`]).
pub fn wordmark_dark_png() -> cja::Result<&'static [u8]> {
    DARK_PNG
        .as_deref()
        .ok_or_else(|| eyre!("Failed to render the dark wordmark PNG"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a PNG into its (width, height, RGBA rows).
    fn decode_rgba(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let reader = png::Decoder::new(std::io::Cursor::new(png_bytes.to_vec()))
            .read_info()
            .unwrap();
        let info = reader.info().clone();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        let mut buf = vec![0; reader.output_buffer_size().expect("PNG buffer size")];
        let mut reader = reader;
        let _ = reader.next_frame(&mut buf).unwrap();
        (info.width, info.height, buf)
    }

    fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{what}: {actual} vs expected {expected} (±{tolerance})"
        );
    }

    #[test]
    fn wordmark_parses_official_artwork() {
        let wordmark = parse(include_str!("../static/brand/logo-light.svg")).unwrap();
        assert_eq!(wordmark.ink.len(), 5);
        assert_eq!(wordmark.pink.len(), 1);
        let light_ink = rgb_hex(wordmark.official_ink);
        assert_eq!(light_ink, (0x1c, 0x1e, 0x21));
        assert_eq!(rgb_hex(wordmark.official_pink), (0xe7, 0x0b, 0x77));
        // Drift tripwires: measured from the vendored artwork with
        // `compute_tight_bounds()`; a re-vendor moves these numbers.
        assert_close(wordmark.bounds.left(), 6.34, 1.0, "left");
        assert_close(wordmark.bounds.top(), 6.26, 1.0, "top");
        assert_close(wordmark.bounds.right(), 703.67, 1.0, "right");
        assert_close(wordmark.bounds.bottom(), 253.74, 1.0, "bottom");
        assert_close(wordmark.ink_width(44.0), 124.0, 2.0, "ink_width(44)");
    }

    #[test]
    fn wordmark_dark_parses_official_artwork() {
        let wordmark = parse(include_str!("../static/brand/logo-dark.svg")).unwrap();
        assert_eq!(wordmark.ink.len(), 5);
        assert_eq!(wordmark.pink.len(), 1);
        assert_eq!(rgb_hex(wordmark.official_ink), (0xff, 0xff, 0xff));
        assert_eq!(rgb_hex(wordmark.official_pink), (0xe7, 0x0b, 0x77));
        assert_close(wordmark.bounds.left(), 6.34, 1.0, "left");
        assert_close(wordmark.bounds.top(), 6.26, 1.0, "top");
        assert_close(wordmark.bounds.right(), 703.67, 1.0, "right");
        assert_close(wordmark.bounds.bottom(), 253.74, 1.0, "bottom");
    }

    #[test]
    fn both_files_share_geometry() {
        let light = parse(include_str!("../static/brand/logo-light.svg")).unwrap();
        let dark = parse(include_str!("../static/brand/logo-dark.svg")).unwrap();
        assert_eq!(
            (light.ink.len(), light.pink.len()),
            (dark.ink.len(), dark.pink.len())
        );
        let light_bounds = painted_tight_bounds(&light);
        let dark_bounds = painted_tight_bounds(&dark);
        assert_eq!(light_bounds.len(), dark_bounds.len());
        for (i, (l, d)) in light_bounds.iter().zip(&dark_bounds).enumerate() {
            for (what, (a, b)) in [
                ("left", (l.left(), d.left())),
                ("top", (l.top(), d.top())),
                ("right", (l.right(), d.right())),
                ("bottom", (l.bottom(), d.bottom())),
            ] {
                assert!(
                    (a - b).abs() <= 0.01,
                    "path {i} {what} diverged: {a} vs {b}"
                );
            }
        }
        for (what, (a, b)) in [
            ("left", (light.bounds.left(), dark.bounds.left())),
            ("top", (light.bounds.top(), dark.bounds.top())),
            ("right", (light.bounds.right(), dark.bounds.right())),
            ("bottom", (light.bounds.bottom(), dark.bounds.bottom())),
        ] {
            assert!((a - b).abs() <= 0.01, "union {what} diverged: {a} vs {b}");
        }
        assert_eq!(rgb_hex(light.official_pink), rgb_hex(dark.official_pink));
        assert_ne!(rgb_hex(light.official_ink), rgb_hex(dark.official_ink));
    }

    #[test]
    fn email_png_is_plated_light_artwork() {
        let (width, height, rgba) = decode_rgba(wordmark_png().unwrap());
        assert_eq!((width, height), (EMAIL_PNG_WIDTH, EMAIL_PNG_HEIGHT));
        let pixel = |x: u32, y: u32| {
            let i = (y * width + x) as usize * 4;
            (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
        };
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(pixel(x, y), (0xFC, 0xFB, 0xF8, 0xFF), "corner ({x},{y})");
        }
        assert!(
            rgba.as_chunks::<4>().0.contains(&[0x1c, 0x1e, 0x21, 0xFF]),
            "no ink pixel found"
        );
        assert!(
            rgba.as_chunks::<4>().0.contains(&[0xe7, 0x0b, 0x77, 0xFF]),
            "no pink pixel found"
        );
    }

    #[test]
    fn email_dark_png_is_transparent_dark_artwork() {
        let (width, height, rgba) = decode_rgba(wordmark_dark_png().unwrap());
        assert_eq!(
            (width, height),
            (EMAIL_DARK_PNG_WIDTH, EMAIL_DARK_PNG_HEIGHT)
        );
        let pixel = |x: u32, y: u32| {
            let i = (y * width + x) as usize * 4;
            (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
        };
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
        ] {
            assert_eq!(pixel(x, y).3, 0, "corner ({x},{y}) alpha");
        }
        assert!(
            rgba.as_chunks::<4>().0.contains(&[0xFF, 0xFF, 0xFF, 0xFF]),
            "no white pixel found"
        );
        assert!(
            rgba.as_chunks::<4>().0.contains(&[0xe7, 0x0b, 0x77, 0xFF]),
            "no pink pixel found"
        );
    }

    #[test]
    fn wordmark_draws_at_requested_height() {
        let wordmark = parse(include_str!("../static/brand/logo-light.svg")).unwrap();
        let mut pixmap = Pixmap::new(400, 200).unwrap();
        wordmark.draw(
            &mut pixmap,
            10.0,
            100.0,
            44.0,
            Color::from_rgba8(0x1c, 0x1e, 0x21, 0xFF),
            Color::from_rgba8(0xe7, 0x0b, 0x77, 0xFF),
        );
        let opaque: Vec<(u32, u32)> = (0..pixmap.height())
            .flat_map(|y| (0..pixmap.width()).map(move |x| (x, y)))
            .filter(|&(x, y)| pixmap.pixel(x, y).is_some_and(|p| p.alpha() > 0))
            .collect();
        let min_y = opaque.iter().map(|&(_, y)| y).min().unwrap();
        let max_y = opaque.iter().map(|&(_, y)| y).max().unwrap();
        let min_x = opaque.iter().map(|&(x, _)| x).min().unwrap();
        assert_close((max_y - min_y) as f32 + 1.0, 44.0, 1.0, "drawn height");
        // Respects placement: left edge ≈ 10, vertical centre ≈ 100.
        assert_close(min_x as f32, 10.0, 1.0, "drawn left");
        assert_close(
            (min_y + max_y) as f32 / 2.0 + 0.5,
            100.0,
            1.0,
            "drawn centre",
        );
    }

    fn rgb_hex(c: Color) -> (u8, u8, u8) {
        (
            (c.red() * 255.0).round() as u8,
            (c.green() * 255.0).round() as u8,
            (c.blue() * 255.0).round() as u8,
        )
    }
}
