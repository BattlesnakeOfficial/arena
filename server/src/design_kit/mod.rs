//! Head & Tail design kit: turn an untrusted upload of a Battlesnake head or tail into ONE
//! clean path in a `0 0 100 100` viewBox, plus shape metrics and friendly lints.
//!
//! Pure and AppState-free. The pipeline (see `docs/design-kit.md`):
//!
//! 1. [`sniff`] the format from magic bytes; reject HEIC, GIF, WebP, PSD, ZIP/.procreate
//!    and PDF/.ai with app-specific advice.
//! 2. Apply the per-format byte cap, then decode (PNG via `png`, JPEG via `zune-jpeg`),
//!    checking the header dimensions before any pixel buffer is allocated.
//! 3. Pick the ink with the template-aware rule ([`palette`]).
//! 4. Fit the canvas into a centred square grid, threshold, remove specks and pinholes,
//!    and trace with visioncortex.
//! 5. Optionally apply a one-tap [`Fix`], then emit the path and lint it for both kinds.
//!
//! Security model: user markup is never echoed. [`CleanShape::to_svg`] fills a fixed
//! template whose only variable parts are a fill-rule keyword and a path `d` that we
//! format from numbers (alphabet `MLQCZ0-9 .-`).
//!
//! SVG input arrives in a follow-up (DEV-1539 PR 2); until then it is sniffed and
//! rejected with [`ProcessError::NotYetSupported`].

mod emit;
mod fix;
mod lints;
pub mod palette;
mod raster;
mod raster_in;
mod trace;

pub use lints::{Edge, Lint, Severity};
pub use raster::{METRIC_SIDE, Metrics};

/// Specks and pinholes smaller than this many units on a side (1 unit² in area, i.e.
/// 10 × 10 px on the 1000 px template) are removed before tracing.
pub const SPECK_UNITS: f32 = 1.0;

/// What the upload is meant to be. Both attach to the body on the LEFT edge of the
/// 100x100 square; heads face right and tails point right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Head,
    Tail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

impl FillRule {
    pub fn as_svg(self) -> &'static str {
        match self {
            FillRule::NonZero => "nonzero",
            FillRule::EvenOdd => "evenodd",
        }
    }

    fn to_skia(self) -> tiny_skia::FillRule {
        match self {
            FillRule::NonZero => tiny_skia::FillRule::Winding,
            FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
        }
    }
}

/// How the clean path was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// A raster (PNG/JPEG) traced into splines.
    Traced,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Traced => "traced",
        }
    }
}

/// A format we accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    Png,
    Jpeg,
    /// Recognised, but rejected with [`ProcessError::NotYetSupported`] until PR 2.
    Svg,
}

impl InputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            InputFormat::Png => "png",
            InputFormat::Jpeg => "jpeg",
            InputFormat::Svg => "svg",
        }
    }
}

/// A format we recognise and reject, each with its own advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectedFormat {
    /// HEIC/HEIF/AVIF (an ISO-BMFF `ftyp` box with an image brand).
    Heic,
    Gif,
    Webp,
    /// Photoshop document: most likely the template itself.
    Psd,
    /// ZIP container: a `.procreate` file (or any zip).
    Zip,
    /// PDF, which is also what Illustrator `.ai` files are; PostScript too.
    Pdf,
}

impl RejectedFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectedFormat::Heic => "heic",
            RejectedFormat::Gif => "gif",
            RejectedFormat::Webp => "webp",
            RejectedFormat::Psd => "psd",
            RejectedFormat::Zip => "zip",
            RejectedFormat::Pdf => "pdf",
        }
    }

    fn advice(self) -> &'static str {
        match self {
            RejectedFormat::Heic => {
                "That's a HEIC or AVIF photo, which we can't read. Share or export it as JPEG \
                 or PNG, then upload that."
            }
            RejectedFormat::Gif => "GIFs aren't supported. Export your drawing as PNG.",
            RejectedFormat::Webp => "WebP isn't supported. Export your drawing as PNG or JPEG.",
            RejectedFormat::Psd => {
                "That's a PSD, like the template. Draw on it, then export a PNG: in Procreate, \
                 Actions → Share → PNG."
            }
            RejectedFormat::Zip => {
                "That looks like a Procreate file. In Procreate, tap Actions → Share → PNG, \
                 then upload the PNG."
            }
            RejectedFormat::Pdf => {
                "That's a PDF or Illustrator file. Use File → Export → SVG or PNG, then upload \
                 that."
            }
        }
    }
}

/// A one-tap fix, applied to the clean path before it is emitted and linted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// Mirror horizontally (`x -> 100 - x`).
    Flip,
    /// Scale the drawing up to the edges of the square, keeping its aspect ratio and
    /// anchoring it to the left edge.
    Fit,
}

impl Fix {
    pub fn as_str(self) -> &'static str {
        match self {
            Fix::Flip => "flip",
            Fix::Fit => "fit",
        }
    }
}

/// Hard caps on what we will process.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Byte cap for SVG uploads (checked after sniffing).
    pub max_svg_bytes: usize,
    /// Byte cap for PNG/JPEG uploads (checked after sniffing).
    pub max_raster_bytes: usize,
    /// Largest accepted raster width or height, checked from the header.
    pub max_raster_side: u32,
    /// Rasters smaller than this on their longer side get a `low_resolution` tip.
    pub min_useful_side: u32,
    /// Side of the square grid rasters are traced on (larger images are downscaled).
    pub trace_side: u32,
    /// Most separate traced shapes before the upload counts as too complex.
    pub max_trace_clusters: usize,
    /// Longest accepted output `d`, in bytes.
    pub max_path_d_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_svg_bytes: 512 * 1024,
            max_raster_bytes: 4 * 1024 * 1024,
            max_raster_side: 2048,
            min_useful_side: 128,
            trace_side: 1024,
            max_trace_clusters: 2_000,
            max_path_d_bytes: 64 * 1024,
        }
    }
}

/// Shape lints for each kind. Processing doesn't depend on the kind, so one upload is
/// linted both ways and the client can switch kinds without re-posting.
#[derive(Debug, Clone, PartialEq)]
pub struct KindLints {
    pub head: Vec<Lint>,
    pub tail: Vec<Lint>,
}

impl KindLints {
    pub fn for_kind(&self, kind: AssetKind) -> &[Lint] {
        match kind {
            AssetKind::Head => &self.head,
            AssetKind::Tail => &self.tail,
        }
    }
}

/// The result of processing: one path in 0..100 space, its metrics and lints.
#[derive(Debug, Clone)]
pub struct CleanShape {
    /// Absolute `M`/`L`/`Q`/`C`/`Z` commands; matches `^[MLQCZ0-9 .\-]*$`.
    pub path_d: String,
    pub fill_rule: FillRule,
    pub strategy: Strategy,
    pub input: InputFormat,
    /// Measured on the clean path (after any fix).
    pub metrics: Metrics,
    /// Shape lints per kind.
    pub lints: KindLints,
    /// Kind-independent input facts (tips and info): what we noticed or changed.
    pub info: Vec<Lint>,
}

impl CleanShape {
    /// The only SVG we ever emit:
    /// `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path fill-rule="…" d="…"/></svg>`.
    /// It starts with `<svg` (the board's loader breaks on a prolog or comment).
    pub fn to_svg(&self) -> String {
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><path fill-rule=\"{}\" d=\"{}\"/></svg>",
            self.fill_rule.as_svg(),
            self.path_d
        )
    }

    /// No warn-level lint for this kind: "Passes every check the official heads pass."
    pub fn passes(&self, kind: AssetKind) -> bool {
        self.lints
            .for_kind(kind)
            .iter()
            .all(|l| l.severity() != Severity::Warn)
    }
}

/// Why an upload could not be processed.
///
/// Everything except [`ProcessError::Internal`] is caused by the upload and should be
/// shown to the user (`user_message()`, HTTP 422). `Internal` is a bug (a caught panic or
/// a failed allocation) and should be reported as a server error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProcessError {
    #[error("the file is empty")]
    EmptyFile,
    #[error("unrecognised file format")]
    UnknownFormat,
    #[error("unsupported format: {}", .0.as_str())]
    UnsupportedFormat(RejectedFormat),
    #[error("{} uploads are not supported yet", .0.as_str())]
    NotYetSupported(InputFormat),
    #[error("file is {bytes} bytes; the limit is {max}")]
    TooLarge { bytes: usize, max: usize },
    #[error("image is {width}x{height}; the limit is {max_side} px per side")]
    ImageTooLarge {
        width: u32,
        height: u32,
        max_side: u32,
    },
    #[error("could not read the image: {0}")]
    InvalidImage(String),
    #[error("too complex: {0}")]
    TooComplex(&'static str),
    /// Nothing drawable survived. `info` says why when we know (e.g. guides only).
    #[error("nothing drawable found")]
    Empty { info: Vec<Lint> },
    #[error("internal error: {0}")]
    Internal(&'static str),
}

impl ProcessError {
    /// Stable snake_case identifier for the client.
    pub fn code(&self) -> &'static str {
        match self {
            ProcessError::EmptyFile => "empty_file",
            ProcessError::UnknownFormat => "unknown_format",
            ProcessError::UnsupportedFormat(_) => "unsupported_format",
            ProcessError::NotYetSupported(_) => "not_yet_supported",
            ProcessError::TooLarge { .. } => "too_large",
            ProcessError::ImageTooLarge { .. } => "image_too_large",
            ProcessError::InvalidImage(_) => "invalid_image",
            ProcessError::TooComplex(_) => "too_complex",
            ProcessError::Empty { .. } => "empty",
            ProcessError::Internal(_) => "internal",
        }
    }

    /// A bug rather than a problem with the upload.
    pub fn is_internal(&self) -> bool {
        matches!(self, ProcessError::Internal(_))
    }

    /// Friendly, actionable copy for the artist.
    pub fn user_message(&self) -> String {
        match self {
            ProcessError::EmptyFile => "That file is empty. Export your drawing again.".into(),
            ProcessError::UnknownFormat => {
                "We couldn't tell what kind of file this is. Upload a PNG, JPEG or SVG.".into()
            }
            ProcessError::UnsupportedFormat(f) => f.advice().into(),
            ProcessError::NotYetSupported(_) => {
                "SVG uploads are coming soon. For now, export your drawing as PNG.".into()
            }
            ProcessError::TooLarge { max, .. } => format!(
                "That file is too big (the limit is {}). Export a PNG at 1000 × 1000 px, the \
                 template's size.",
                human_bytes(*max)
            ),
            ProcessError::ImageTooLarge {
                width,
                height,
                max_side,
            } => format!(
                "Your image is {width} × {height} px. Export it at {max_side} px or less on each \
                 side; the template is 1000 × 1000 px."
            ),
            ProcessError::InvalidImage(_) => {
                "We couldn't read that image; it may be damaged or cut off. Export it again as \
                 PNG."
                    .into()
            }
            ProcessError::TooComplex(_) => "That's too detailed to turn into a head or tail. Use \
                 one solid dark colour with a few big cut-outs, and no texture or photo \
                 background."
                .into(),
            ProcessError::Empty { info } if info.contains(&Lint::GuidesVisible) => "We only found \
                 the template guides. Draw in black on the \"Draw here\" layer, hide the guides, \
                 then export again."
                .into(),
            ProcessError::Empty { .. } => {
                "We couldn't find a drawing. Draw in solid black (or any \
                 dark colour) and export as PNG."
                    .into()
            }
            ProcessError::Internal(_) => {
                "Something went wrong on our side. Please try again.".into()
            }
        }
    }
}

fn human_bytes(n: usize) -> String {
    if n >= 1024 * 1024 && n.is_multiple_of(1024 * 1024) {
        format!("{} MB", n / (1024 * 1024))
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}

/// Identify the upload from its magic bytes.
pub fn sniff(bytes: &[u8]) -> Result<InputFormat, ProcessError> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    if bytes.is_empty() {
        return Err(ProcessError::EmptyFile);
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Ok(InputFormat::Png);
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return Ok(InputFormat::Jpeg);
    }
    let rejected = if bytes.get(4..8) == Some(b"ftyp") {
        // ISO-BMFF: HEIF/AVIF images by their major brand (videos share the box).
        const IMAGE_BRANDS: [&[u8]; 12] = [
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1",
            b"msf1", b"avif", b"avis",
        ];
        let brand = bytes.get(8..12).unwrap_or_default();
        IMAGE_BRANDS
            .contains(&brand)
            .then_some(RejectedFormat::Heic)
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(RejectedFormat::Gif)
    } else if starts(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(RejectedFormat::Webp)
    } else if starts(b"8BPS") {
        Some(RejectedFormat::Psd)
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        Some(RejectedFormat::Zip)
    } else if starts(b"%PDF") || starts(b"%!PS") {
        Some(RejectedFormat::Pdf)
    } else {
        None
    };
    if let Some(r) = rejected {
        return Err(ProcessError::UnsupportedFormat(r));
    }
    if looks_like_svg(bytes) {
        return Ok(InputFormat::Svg);
    }
    Err(ProcessError::UnknownFormat)
}

/// Text that starts (after a BOM and whitespace) with `<` and has an `<svg` tag early on.
/// Prologs, comments and doctypes before the root are common in exported SVGs.
fn looks_like_svg(bytes: &[u8]) -> bool {
    let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let start = text
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(text.len());
    let head = &text[start..text.len().min(start + 64 * 1024)];
    head.first() == Some(&b'<') && head.windows(4).any(|w| w == b"<svg")
}

/// Untrusted upload bytes -> clean shape.
///
/// CPU-bound and synchronous (tens of milliseconds in release for a 2048 px PNG); run it
/// off the async runtime, behind a semaphore.
pub fn process_upload(
    bytes: &[u8],
    limits: &Limits,
    fix: Option<Fix>,
) -> Result<CleanShape, ProcessError> {
    let input = sniff(bytes)?;
    let max = match input {
        InputFormat::Svg => limits.max_svg_bytes,
        InputFormat::Png | InputFormat::Jpeg => limits.max_raster_bytes,
    };
    if bytes.len() > max {
        return Err(ProcessError::TooLarge {
            bytes: bytes.len(),
            max,
        });
    }
    let traced = match input {
        InputFormat::Png | InputFormat::Jpeg => raster_in::process(bytes, input, limits)?,
        InputFormat::Svg => return Err(ProcessError::NotYetSupported(input)),
    };
    finish(
        traced.path,
        FillRule::EvenOdd,
        Strategy::Traced,
        input,
        traced.info,
        fix,
        limits,
    )
}

/// Shared tail of every pipeline: path in 0..100 -> fix -> `d` + metrics + lints.
fn finish(
    path: tiny_skia::Path,
    fill_rule: FillRule,
    strategy: Strategy,
    input: InputFormat,
    info: Vec<Lint>,
    fix: Option<Fix>,
    limits: &Limits,
) -> Result<CleanShape, ProcessError> {
    let path = match fix {
        Some(f) => fix::apply(path, f),
        None => path,
    };
    let path_d = emit::path_to_d(&path);
    if path_d.len() > limits.max_path_d_bytes {
        return Err(ProcessError::TooComplex("the outline is too detailed"));
    }
    let metrics = Metrics::of_path(&path, fill_rule)?;
    if metrics.fill_pct < 0.05 {
        return Err(ProcessError::Empty { info });
    }
    let lints = KindLints {
        head: lints::for_kind(&metrics, AssetKind::Head),
        tail: lints::for_kind(&metrics, AssetKind::Tail),
    };
    Ok(CleanShape {
        path_d,
        fill_rule,
        strategy,
        input,
        metrics,
        lints,
        info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_sniffing_tolerates_prologs() {
        assert_eq!(sniff(b"<svg/>"), Ok(InputFormat::Svg));
        assert_eq!(
            sniff(
                b"\xef\xbb\xbf  <?xml version=\"1.0\"?>\n<!-- hi -->\n<svg viewBox=\"0 0 1 1\"/>"
            ),
            Ok(InputFormat::Svg)
        );
        assert_eq!(sniff(b"<html></html>"), Err(ProcessError::UnknownFormat));
        assert_eq!(sniff(b"   "), Err(ProcessError::UnknownFormat));
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(4 * 1024 * 1024), "4 MB");
        assert_eq!(human_bytes(512 * 1024), "512 KB");
    }
}
