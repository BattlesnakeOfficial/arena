//! Text for social cards: the site's three typefaces, embedded, shaped with
//! rustybuzz (so kerning matches the browser) and filled as tiny-skia paths.
//!
//! The fonts are static instances of the Google Fonts families `Page` loads
//! (`GOOGLE_FONTS_HREF`), vendored under `og/fonts/` with their OFL licences.
//! Characters a font has no glyph for (emoji, most non-Latin scripts) are
//! dropped rather than drawn as boxes.

use std::sync::LazyLock;

use color_eyre::eyre::eyre;
use rustybuzz::{Face, UnicodeBuffer, ttf_parser};
use tiny_skia::{Color, FillRule, PathBuilder, Transform};

use super::canvas::{Canvas, solid};

/// A typeface and weight from the site's type scale (`--display`, `--body`,
/// `--mono` in `static/arena.css`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// Bricolage Grotesque ExtraBold: headlines and the wordmark.
    Display,
    /// Instrument Sans Medium: body copy.
    Body,
    /// Instrument Sans SemiBold: names in lists.
    Strong,
    /// IBM Plex Mono Medium: kickers and numbers.
    Mono,
}

pub struct Fonts {
    display: Face<'static>,
    body: Face<'static>,
    strong: Face<'static>,
    mono: Face<'static>,
}

static FONTS: LazyLock<Option<Fonts>> = LazyLock::new(|| {
    Some(Fonts {
        display: Face::from_slice(include_bytes!("fonts/BricolageGrotesque-ExtraBold.ttf"), 0)?,
        body: Face::from_slice(include_bytes!("fonts/InstrumentSans-Medium.ttf"), 0)?,
        strong: Face::from_slice(include_bytes!("fonts/InstrumentSans-SemiBold.ttf"), 0)?,
        mono: Face::from_slice(include_bytes!("fonts/IBMPlexMono-Medium.ttf"), 0)?,
    })
});

impl Fonts {
    pub fn get() -> cja::Result<&'static Fonts> {
        FONTS
            .as_ref()
            .ok_or_else(|| eyre!("An embedded social-card font failed to parse"))
    }

    fn face(&self, font: Font) -> &Face<'static> {
        match font {
            Font::Display => &self.display,
            Font::Body => &self.body,
            Font::Strong => &self.strong,
            Font::Mono => &self.mono,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    pub font: Font,
    /// Font size in pixels.
    pub size: f32,
    pub color: Color,
    /// Extra space between glyphs, in em (CSS `letter-spacing`).
    pub tracking: f32,
}

impl TextStyle {
    pub fn new(font: Font, size: f32, color: Color) -> Self {
        Self {
            font,
            size,
            color,
            tracking: 0.0,
        }
    }

    pub fn tracking(self, tracking: f32) -> Self {
        Self { tracking, ..self }
    }

    pub fn size(self, size: f32) -> Self {
        Self { size, ..self }
    }

    pub fn color(self, color: Color) -> Self {
        Self { color, ..self }
    }
}

#[derive(Debug, Clone, Copy)]
struct Glyph {
    id: ttf_parser::GlyphId,
    /// Offset from the line's origin, in pixels (`y` grows upward, as in the font).
    x: f32,
    y: f32,
}

/// One shaped line of text, ready to draw.
#[derive(Debug, Clone)]
pub struct Line {
    glyphs: Vec<Glyph>,
    pub width: f32,
    pub style: TextStyle,
}

/// `text` with the characters `face` can't draw dropped, control characters
/// removed, and runs of whitespace collapsed to single spaces.
fn clean(face: &Face, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else if !c.is_control() && face.glyph_index(c).is_some() {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

struct Outline<'a> {
    pb: &'a mut PathBuilder,
    x: f32,
    y: f32,
    scale: f32,
}

impl Outline<'_> {
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        (self.x + x * self.scale, self.y - y * self.scale)
    }
}

impl ttf_parser::OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.pb.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.pb.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.at(x1, y1);
        let (x, y) = self.at(x, y);
        self.pb.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.at(x1, y1);
        let (x2, y2) = self.at(x2, y2);
        let (x, y) = self.at(x, y);
        self.pb.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.pb.close();
    }
}

const ELLIPSIS: &str = "…";

impl Canvas {
    /// Shape one line of `text` (no wrapping).
    pub fn shape(&self, text: &str, style: TextStyle) -> Line {
        let face = self.fonts.face(style.font);
        let text = clean(face, text);
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(&text);
        let shaped = rustybuzz::shape(face, &[], buffer);
        let scale = style.size / face.units_per_em() as f32;
        let tracking = style.tracking * style.size;

        let mut glyphs = Vec::with_capacity(shaped.len());
        let mut pen = 0.0;
        for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            glyphs.push(Glyph {
                id: ttf_parser::GlyphId(u16::try_from(info.glyph_id).unwrap_or(0)),
                x: pen + pos.x_offset as f32 * scale,
                y: pos.y_offset as f32 * scale,
            });
            pen += pos.x_advance as f32 * scale + tracking;
        }
        // Tracking goes between glyphs, not after the last one.
        let width = if glyphs.is_empty() {
            0.0
        } else {
            pen - tracking
        };
        Line {
            glyphs,
            width,
            style,
        }
    }

    /// Draw `line` with its left end at `x` and its baseline at `baseline`.
    pub fn draw(&mut self, line: &Line, x: f32, baseline: f32) {
        let face = self.fonts.face(line.style.font);
        let scale = line.style.size / face.units_per_em() as f32;
        let mut pb = PathBuilder::new();
        for glyph in &line.glyphs {
            let mut outline = Outline {
                pb: &mut pb,
                x: x + glyph.x,
                y: baseline - glyph.y,
                scale,
            };
            face.outline_glyph(glyph.id, &mut outline);
        }
        if let Some(path) = pb.finish() {
            self.fill_path(
                &path,
                &solid(line.style.color),
                FillRule::Winding,
                Transform::identity(),
            );
        }
    }

    /// Shape and draw `text`; returns its width.
    pub fn text(&mut self, text: &str, style: TextStyle, x: f32, baseline: f32) -> f32 {
        let line = self.shape(text, style);
        self.draw(&line, x, baseline);
        line.width
    }

    /// Shape and draw `text` with its right end at `right`; returns its width.
    pub fn text_right(&mut self, text: &str, style: TextStyle, right: f32, baseline: f32) -> f32 {
        let line = self.shape(text, style);
        self.draw(&line, right - line.width, baseline);
        line.width
    }

    /// Height of a capital letter at `size`, for centring text on shapes.
    pub fn cap_height(&self, font: Font, size: f32) -> f32 {
        let face = self.fonts.face(font);
        let units = face
            .capital_height()
            .map_or_else(|| f32::from(face.ascender()) * 0.7, f32::from);
        units * size / face.units_per_em() as f32
    }

    /// `text` on one line at the largest size from `style.size` down to
    /// `min_size` that fits `max_width`; ellipsized at `min_size` if even
    /// that is too wide.
    pub fn fit_line(&self, text: &str, style: TextStyle, max_width: f32, min_size: f32) -> Line {
        let line = self.shape(text, style);
        if line.width <= max_width {
            return line;
        }
        // Width is linear in size (tracking is in em), so one rescale lands it.
        let size = (style.size * max_width / line.width).max(min_size);
        let style = style.size(size);
        let line = self.shape(text, style);
        if line.width <= max_width + 0.5 {
            return line;
        }
        self.ellipsize(text, style, max_width)
    }

    /// `text` on one line at `style.size`, cut short with an ellipsis if it
    /// doesn't fit `max_width`.
    pub fn ellipsize(&self, text: &str, style: TextStyle, max_width: f32) -> Line {
        let full = self.shape(text, style);
        if full.width <= max_width {
            return full;
        }
        let chars: Vec<char> = clean(self.fonts.face(style.font), text).chars().collect();
        let cut = |n: usize| {
            let kept: String = chars[..n].iter().collect();
            format!("{}{ELLIPSIS}", kept.trim_end())
        };
        // Longest prefix that fits with the ellipsis: binary search on length.
        let (mut lo, mut hi) = (0, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.shape(&cut(mid), style).width <= max_width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        self.shape(&cut(lo), style)
    }

    /// Word-wrap `text` into at most `max_lines` lines of `max_width`,
    /// shrinking from `style.size` toward `min_size` until it fits. At
    /// `min_size` the overflow is ellipsized onto the last line.
    pub fn fit_lines(
        &self,
        text: &str,
        style: TextStyle,
        max_width: f32,
        max_lines: usize,
        min_size: f32,
    ) -> Vec<Line> {
        let mut size = style.size;
        loop {
            let style = style.size(size);
            let lines = self.wrap(text, style, max_width);
            let fits = lines.len() <= max_lines
                && lines
                    .iter()
                    .all(|l| self.shape(l, style).width <= max_width);
            if fits || size <= min_size {
                let max_lines = max_lines.max(1);
                return lines
                    .iter()
                    .enumerate()
                    .take(max_lines)
                    .map(|(i, line)| {
                        let line = if i + 1 == max_lines {
                            lines[i..].join(" ")
                        } else {
                            line.clone()
                        };
                        self.ellipsize(&line, style, max_width)
                    })
                    .collect();
            }
            size = (size * 0.92).max(min_size);
        }
    }

    /// Greedy word wrap; a word wider than `max_width` gets a line of its own.
    fn wrap(&self, text: &str, style: TextStyle, max_width: f32) -> Vec<String> {
        let text = clean(self.fonts.face(style.font), text);
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in text.split(' ') {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if current.is_empty() || self.shape(&candidate, style).width <= max_width {
                current = candidate;
            } else {
                lines.push(std::mem::replace(&mut current, word.to_string()));
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::og::canvas::palette;

    fn canvas() -> Canvas {
        Canvas::new().unwrap()
    }

    fn style(size: f32) -> TextStyle {
        TextStyle::new(Font::Display, size, palette::ink())
    }

    #[test]
    fn every_embedded_font_parses() {
        let fonts = Fonts::get().unwrap();
        for font in [Font::Display, Font::Body, Font::Strong, Font::Mono] {
            assert!(fonts.face(font).glyph_index('A').is_some(), "{font:?}");
            assert!(
                fonts.face(font).glyph_index('…').is_some(),
                "{font:?} has no ellipsis"
            );
        }
    }

    #[test]
    fn unsupported_characters_are_dropped_and_whitespace_collapsed() {
        let fonts = Fonts::get().unwrap();
        let face = fonts.face(Font::Display);
        assert_eq!(clean(face, "  🐍 Snek\t\n  Two\u{0007} "), "Snek Two");
        assert_eq!(clean(face, "🐍🐍"), "");
    }

    #[test]
    fn shaping_kerns_like_a_browser() {
        // "AV" kerns tighter than "A" + "V" laid side by side.
        let c = canvas();
        let pair = c.shape("AV", style(100.0)).width;
        let apart = c.shape("A", style(100.0)).width + c.shape("V", style(100.0)).width;
        assert!(pair < apart - 1.0, "AV {pair} vs {apart}");
    }

    #[test]
    fn tracking_widens_between_glyphs_only() {
        let c = canvas();
        let plain = c.shape("abc", style(100.0)).width;
        let tracked = c.shape("abc", style(100.0).tracking(0.1)).width;
        assert!(
            (tracked - plain - 20.0).abs() < 0.01,
            "{plain} -> {tracked}"
        );
        assert_eq!(c.shape("", style(100.0).tracking(0.1)).width, 0.0);
    }

    #[test]
    fn fit_line_shrinks_then_ellipsizes() {
        let c = canvas();
        let short = c.fit_line("Snek", style(60.0), 1000.0, 30.0);
        assert_eq!(short.style.size, 60.0);

        let wide = c
            .shape("A considerably longer snake name", style(60.0))
            .width;
        let shrunk = c.fit_line(
            "A considerably longer snake name",
            style(60.0),
            wide * 0.75,
            30.0,
        );
        assert!(shrunk.style.size < 60.0 && shrunk.style.size >= 30.0);
        assert!(shrunk.width <= wide * 0.75 + 0.5);

        let cut = c.fit_line(&"Snek".repeat(40), style(60.0), 300.0, 30.0);
        assert_eq!(cut.style.size, 30.0);
        assert!(cut.width <= 300.0, "{}", cut.width);
        assert!(cut.width > 200.0, "cut far too short: {}", cut.width);
    }

    #[test]
    fn ellipsize_handles_nothing_fitting() {
        let c = canvas();
        let line = c.ellipsize("Snek", style(60.0), 1.0);
        // Only the ellipsis is left; it may overflow a 1px box, but never panics.
        assert_eq!(line.glyphs.len(), 1);
    }

    #[test]
    fn fit_lines_wraps_shrinks_and_caps_line_count() {
        let c = canvas();
        let lines = c.fit_lines("Snek Alpha vs Snek Beta", style(64.0), 420.0, 2, 40.0);
        assert!(lines.len() <= 2);
        assert!(lines.iter().all(|l| l.width <= 420.0));

        let many = "word ".repeat(80);
        let capped = c.fit_lines(&many, style(64.0), 420.0, 3, 40.0);
        assert_eq!(capped.len(), 3);
        assert_eq!(capped[0].style.size, 40.0);
        assert!(capped.iter().all(|l| l.width <= 420.0));

        let one_long_word = c.fit_lines(&"x".repeat(200), style(64.0), 420.0, 2, 40.0);
        assert_eq!(one_long_word.len(), 1);
        assert!(one_long_word[0].width <= 420.0);

        assert!(c.fit_lines("🐍", style(64.0), 420.0, 2, 40.0).is_empty());
    }
}
