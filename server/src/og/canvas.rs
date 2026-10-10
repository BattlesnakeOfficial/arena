//! A 1200×630 drawing surface for social cards, its colours, and PNG output.

use color_eyre::eyre::{Context as _, eyre};
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap, Point,
    RadialGradient, Rect, Shader, SpreadMode, Stroke, Transform,
};

use super::text::Fonts;

/// The size every social platform crops to (1.91:1).
pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

/// The site's dark theme (`[data-app-theme="dark"]` in `static/arena.css`), plus
/// the home page's decorative board panel.
pub mod palette {
    use tiny_skia::Color;

    use super::rgb;

    pub fn paper() -> Color {
        rgb(0x17, 0x11, 0x17)
    }
    pub fn ink() -> Color {
        rgb(0xF4, 0xEF, 0xEA)
    }
    pub fn muted() -> Color {
        rgb(0x9C, 0x8F, 0x99)
    }
    pub fn hairline() -> Color {
        rgb(0x2A, 0x23, 0x2A)
    }
    pub fn card() -> Color {
        rgb(0x1E, 0x18, 0x1D)
    }
    pub fn pink() -> Color {
        rgb(0xFF, 0x3D, 0x8A)
    }
    pub fn board_top() -> Color {
        rgb(0x1A, 0x14, 0x1A)
    }
    pub fn board_bottom() -> Color {
        rgb(0x17, 0x12, 0x17)
    }
    pub fn board_border() -> Color {
        rgb(0x32, 0x2A, 0x31)
    }
    pub fn cell() -> Color {
        rgb(0x24, 0x1C, 0x23)
    }
    /// A grid cell tinted toward pink: danger, not just a lighter square.
    pub fn hazard() -> Color {
        rgb(0x45, 0x1D, 0x31)
    }
    pub fn food() -> Color {
        rgb(0xFF, 0x5F, 0x6D)
    }
    /// `chip_color`'s fallback for snakes without a valid colour.
    pub fn snake_fallback() -> Color {
        rgb(0x88, 0x88, 0x88)
    }
}

pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

/// `color` at `alpha` (0..=1) opacity.
pub fn with_alpha(mut color: Color, alpha: f32) -> Color {
    color.apply_opacity(alpha);
    color
}

/// A strict `#rrggbb` snake colour, like [`crate::customizations::normalize_color`].
pub fn parse_hex(declared: &str) -> Option<Color> {
    let hex = declared.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(rgb(channel(0)?, channel(2)?, channel(4)?))
}

/// A snake's colour for drawing, with the site's gray fallback.
pub fn snake_color(declared: &str) -> Color {
    parse_hex(declared).unwrap_or(palette::snake_fallback())
}

pub struct Canvas {
    pixmap: Pixmap,
    pub(super) fonts: &'static Fonts,
    /// While set, every fill and stroke is clipped to it.
    clip: Option<Mask>,
}

impl Canvas {
    /// A blank card, filled with the page background.
    pub fn new() -> cja::Result<Self> {
        let mut pixmap =
            Pixmap::new(WIDTH, HEIGHT).ok_or_else(|| eyre!("Failed to allocate card pixmap"))?;
        pixmap.fill(palette::paper());
        Ok(Self {
            pixmap,
            fonts: Fonts::get()?,
            clip: None,
        })
    }

    /// Clip drawing to `path` until [`Canvas::unclip`].
    pub fn clip_to(&mut self, path: &Path) {
        self.clip = Mask::new(WIDTH, HEIGHT).map(|mut mask| {
            mask.fill_path(path, FillRule::Winding, true, Transform::identity());
            mask
        });
    }

    pub fn unclip(&mut self) {
        self.clip = None;
    }

    pub fn fill_path(&mut self, path: &Path, paint: &Paint, rule: FillRule, transform: Transform) {
        self.pixmap
            .fill_path(path, paint, rule, transform, self.clip.as_ref());
    }

    pub fn stroke_path(
        &mut self,
        path: &Path,
        paint: &Paint,
        stroke: &Stroke,
        transform: Transform,
    ) {
        self.pixmap
            .stroke_path(path, paint, stroke, transform, self.clip.as_ref());
    }

    pub fn fill(&mut self, path: &Path, color: Color) {
        self.fill_path(
            path,
            &solid(color),
            FillRule::Winding,
            Transform::identity(),
        );
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
            self.pixmap.fill_rect(
                rect,
                &solid(color),
                Transform::identity(),
                self.clip.as_ref(),
            );
        }
    }

    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, color: Color) {
        if let Some(path) = round_rect(x, y, w, h, r) {
            self.fill(&path, color);
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            self.fill(&path, color);
        }
    }

    /// A soft circular glow: `color` at the centre fading to transparent at `r`.
    pub fn glow(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        let stops = vec![
            GradientStop::new(0.0, color),
            GradientStop::new(1.0, with_alpha(color, 0.0)),
        ];
        let center = Point::from_xy(cx, cy);
        let Some(shader) = RadialGradient::new(
            center,
            0.0,
            center,
            r,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) else {
            return;
        };
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            self.fill_path(
                &path,
                &shaded(shader),
                FillRule::Winding,
                Transform::identity(),
            );
        }
    }

    #[cfg(test)]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        self.pixmap.pixel(x, y).map_or([0; 3], |p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue()]
        })
    }

    /// The card as an opaque RGB PNG.
    pub fn into_png(self) -> cja::Result<Vec<u8>> {
        let mut rgb = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
        for pixel in self.pixmap.pixels() {
            let c = pixel.demultiply();
            rgb.extend_from_slice(&[c.red(), c.green(), c.blue()]);
        }
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, WIDTH, HEIGHT);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Balanced);
        let mut writer = encoder
            .write_header()
            .wrap_err("Failed to write card PNG header")?;
        writer
            .write_image_data(&rgb)
            .wrap_err("Failed to write card PNG data")?;
        writer.finish().wrap_err("Failed to finish card PNG")?;
        Ok(png)
    }
}

pub fn solid(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

pub fn shaded(shader: Shader<'static>) -> Paint<'static> {
    Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    }
}

/// A top-to-bottom gradient over `y0..y1`.
pub fn vertical_gradient(y0: f32, y1: f32, top: Color, bottom: Color) -> Option<Shader<'static>> {
    LinearGradient::new(
        Point::from_xy(0.0, y0),
        Point::from_xy(0.0, y1),
        vec![GradientStop::new(0.0, top), GradientStop::new(1.0, bottom)],
        SpreadMode::Pad,
        Transform::identity(),
    )
}

/// A rectangle with corners of radius `r` (clamped to fit).
pub fn round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    // Cubic Bézier control-point distance that best approximates a quarter circle.
    const KAPPA: f32 = 0.552_284_8;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = r * KAPPA;
    let (x1, y1) = (x + w, y + h);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x1 - r, y);
    pb.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x + r, y1);
    pb.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_only_strict_rrggbb() {
        assert_eq!(parse_hex("#ff3d8a"), Some(rgb(0xff, 0x3d, 0x8a)));
        assert_eq!(parse_hex("#FF3D8A"), Some(rgb(0xff, 0x3d, 0x8a)));
        for bad in [
            "", "ff3d8a", "#fff", "#ff3d8a0", "#gg0000", "red", "#ff3d8é",
        ] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
        assert_eq!(snake_color("nope"), palette::snake_fallback());
    }

    #[test]
    fn blank_card_encodes_as_a_1200x630_png() {
        let png = Canvas::new().unwrap().into_png().unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
        assert_eq!(info.color_type, png::ColorType::Rgb);
    }
}
