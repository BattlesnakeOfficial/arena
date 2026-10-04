//! Head & Tail design kit: turn an untrusted upload of a Battlesnake head or tail into ONE
//! clean path in a `0 0 100 100` viewBox, plus shape metrics and friendly lints.
//!
//! Pure and AppState-free. The pipeline (see `docs/design-kit.md`):
//!
//! 1. [`sniff`] the format from magic bytes; reject HEIC, GIF, WebP, PSD, ZIP/.procreate,
//!    PDF/.ai, compressed (.svgz) and UTF-16 SVGs with app-specific advice.
//! 2. Apply the per-format byte cap.
//! 3. Rasters: decode (PNG via `png`, JPEG via `zune-jpeg`), checking the header
//!    dimensions before any pixel buffer is allocated; pick the ink with the
//!    template-aware rule ([`palette`]); fit the canvas into a centred square grid,
//!    threshold, remove specks and pinholes, and trace with visioncortex.
//! 4. SVGs: bound the document (depth, entities, nodes, references, CSS) before usvg
//!    converts it, keep only the drawing (the template filter), paint a bounded "truth"
//!    raster, and emit the vector geometry as is when it reproduces the truth, or trace
//!    the truth otherwise.
//! 5. Optionally apply one-tap [`Fix`]es, then emit the path and lint it for both kinds.
//!
//! Security model: user markup is never echoed. [`CleanShape::to_svg`] fills a fixed
//! template whose only variable parts are a fill-rule keyword and a path `d` that we
//! format from numbers (alphabet `MLQCZ0-9 .-`).
//!
//! **Stack**: processing a hostile SVG within the limits can recurse about 4,000 levels
//! deep inside usvg, which overflows a default 2 MiB thread stack and aborts the process.
//! Run uploads with [`process_on_big_stack`] (a dedicated thread with a 64 MiB stack),
//! and in production inside a short-lived child process with memory and CPU limits (the
//! studio, PR 3): a failed allocation, or an abort nothing here foresaw, then ends only
//! that process.

mod emit;
mod fix;
mod lints;
mod paint;
pub mod palette;
mod raster;
mod raster_in;
pub mod refs;
mod svg_in;
mod svg_scan;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// How the clean path was produced. Serialized as [`Strategy::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// A raster (PNG/JPEG) traced into splines.
    Traced,
    /// SVG geometry emitted as is: it renders the same as the drawing.
    VectorExact,
    /// An SVG rendered to the trace grid and traced (overlapping shapes of opposite
    /// winding, white cut-outs, clips, opacity, ...).
    Retraced,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Traced => "traced",
            Strategy::VectorExact => "vector_exact",
            Strategy::Retraced => "retraced",
        }
    }
}

/// A format we accept. Serialized as [`InputFormat::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputFormat {
    Png,
    Jpeg,
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
    /// Gzip: a compressed SVG (`.svgz`), as Inkscape and Illustrator can save.
    Svgz,
    /// UTF-16 text: an SVG saved with UTF-16 encoding (an Illustrator option).
    Utf16,
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
            RejectedFormat::Svgz => "svgz",
            RejectedFormat::Utf16 => "utf16",
        }
    }

    /// What to tell the artist (the 422 message for this format).
    pub fn advice(self) -> &'static str {
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
            RejectedFormat::Svgz => {
                "That's a compressed SVG (.svgz). Save it as a plain SVG instead (Inkscape: Save \
                 As → Plain SVG; Illustrator: File → Export → Export As → SVG), then upload that."
            }
            RejectedFormat::Utf16 => {
                "That SVG is saved as UTF-16 text, which we can't read. Save it again as UTF-8 \
                 (in Illustrator's SVG Options, set Encoding to UTF-8), then upload it."
            }
        }
    }
}

/// A one-tap fix, applied to the clean path before it is emitted and linted.
///
/// Fixes form a set: [`process_upload`] applies Flip before Fit whatever order they are
/// given in (fitting first would move a flipped head away from the neck edge), so the
/// studio can send every fix the artist has tapped so far, e.g. `?fix=flip&fix=fit`.
/// Serialized as [`Fix::as_str`]; [`Fix::parse`] reads it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

    /// The fix named by [`Fix::as_str`].
    pub fn parse(name: &str) -> Option<Fix> {
        [Fix::Flip, Fix::Fit]
            .into_iter()
            .find(|f| f.as_str() == name)
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
    /// Most outline on the trace grid, in pixel edges (ink next to background) per pixel
    /// of grid side. Bounds the tracer's work and keeps it clear of visioncortex's
    /// internal limits; the most detailed official asset needs about 20 (21 roughened).
    pub max_trace_edges_per_side: usize,
    /// Most total area of the traced shapes' and holes' bounding boxes, in multiples of
    /// the trace grid's area (the tracer rescans each box). The official assets need at
    /// most 2.2.
    pub max_trace_box_cover: usize,
    /// Longest accepted output `d`, in bytes.
    pub max_path_d_bytes: usize,
    /// Most XML nodes (elements, text, comments) in an SVG.
    pub max_svg_nodes: u32,
    /// Deepest element nesting in an SVG.
    pub max_svg_depth: usize,
    /// Most `<use>` elements.
    pub max_svg_uses: usize,
    /// Most clipPath, mask, pattern, marker, symbol, gradient and filter definitions.
    pub max_svg_defs: usize,
    /// Most elements that references may add on top of `max_svg_nodes`: every `<use>`,
    /// objectBoundingBox pattern fill and marker vertex copies its target's content.
    pub max_svg_expansion: u64,
    /// Most path segments usvg may make, counted before it runs: every copy (`<use>`,
    /// patterns, markers) and every arc split into the cubics usvg makes of it. Bounds
    /// usvg's memory before the painter's own budget (`max_svg_segments`) can see it.
    pub max_svg_expanded_segments: u64,
    /// Deepest nesting once references are followed (a chain of patterns, masks or
    /// `<use>` nests each target's content inside the referencing element). Bounds
    /// usvg's recursion; see the stack note on [`process_upload`].
    pub max_svg_nesting: usize,
    /// CSS budget: an upper bound on simplecss's steps, parsing the sheets and `style`
    /// attributes and matching every rule against every element and `<use>` copy (see
    /// `svg_scan.rs`).
    pub max_css_work: u64,
    /// Most path segments painted (stroke outlines count 5 each).
    pub max_svg_segments: usize,
    /// Most vertical travel of the painted outlines, in units (a full-height edge is
    /// 100): the rasteriser's work grows with it, and 5000 segments that cross the
    /// whole drawing took 350 ms in release. The official assets need at most 1,835.
    pub max_svg_travel: u32,
    /// Most clipped groups painted. Each holds a mask (one byte per pixel of the raster,
    /// 1 MiB at the trace grid size) while its content is painted, so this bounds the
    /// masks alive at once.
    pub max_svg_clips: usize,
    /// Most dashes in dashed strokes.
    pub max_svg_dashes: usize,
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
            max_trace_edges_per_side: 64,
            max_trace_box_cover: 8,
            max_path_d_bytes: 64 * 1024,
            max_svg_nodes: 20_000,
            max_svg_depth: 64,
            max_svg_uses: 500,
            max_svg_defs: 64,
            max_svg_expansion: 20_000,
            max_svg_expanded_segments: 500_000,
            max_svg_nesting: 4096,
            max_css_work: 20_000_000,
            max_svg_segments: 5_000,
            max_svg_travel: 40_000,
            max_svg_clips: 16,
            max_svg_dashes: 20_000,
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
///
/// The fields are private so that the only way to get one is [`process_upload`]: the
/// `d` it carries is always formatted from numbers by this module, which is what makes
/// [`CleanShape::to_svg`] safe to serve.
#[derive(Debug, Clone)]
pub struct CleanShape {
    path_d: String,
    fill_rule: FillRule,
    strategy: Strategy,
    input: InputFormat,
    metrics: Metrics,
    lints: KindLints,
    info: Vec<Lint>,
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

    /// Absolute `M`/`L`/`Q`/`C`/`Z` commands; matches `^[MLQCZ0-9 .\-]*$`.
    pub fn path_d(&self) -> &str {
        &self.path_d
    }

    pub fn fill_rule(&self) -> FillRule {
        self.fill_rule
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    pub fn input(&self) -> InputFormat {
        self.input
    }

    /// Measured on the clean path (after any fixes).
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Shape lints per kind.
    pub fn lints(&self) -> &KindLints {
        &self.lints
    }

    /// Kind-independent input facts (tips and info): what we noticed or changed.
    pub fn info(&self) -> &[Lint] {
        &self.info
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
/// shown to the user (`user_message()`, HTTP 422). `Internal` is a bug (a caught tracer,
/// usvg or painter panic, a processing thread that couldn't start, or an internal
/// invariant that didn't hold) and should be reported as a server error. A decoder panic
/// is caught and reported as `InvalidImage`.
///
/// [`process_upload`] does not catch everything: a panic elsewhere in our own code
/// unwinds out of it ([`process_on_big_stack`] catches that as `Internal`), and a failed
/// allocation aborts the process. Run it in an isolated process with a memory limit.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProcessError {
    #[error("the file is empty")]
    EmptyFile,
    #[error("unrecognised file format")]
    UnknownFormat,
    #[error("unsupported format: {}", .0.as_str())]
    UnsupportedFormat(RejectedFormat),
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
    /// Not XML we accept: malformed, or declaring entities.
    #[error("the SVG is not valid XML: {0}")]
    InvalidXml(String),
    /// XML, but not an SVG we can read (not UTF-8, wrong root, bad size, ...).
    #[error("could not read the SVG: {0}")]
    InvalidSvg(String),
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
            ProcessError::TooLarge { .. } => "too_large",
            ProcessError::ImageTooLarge { .. } => "image_too_large",
            ProcessError::InvalidImage(_) => "invalid_image",
            ProcessError::InvalidXml(_) => "invalid_xml",
            ProcessError::InvalidSvg(_) => "invalid_svg",
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
            ProcessError::InvalidXml(detail) if detail == svg_in::ENTITIES_REJECTED => "That SVG \
                 declares XML entities, which we don't accept. Export it again as a plain SVG \
                 (in Illustrator: File → Export → Export As → SVG), or export a PNG."
                .into(),
            ProcessError::InvalidXml(_) => "That SVG isn't valid XML, so we couldn't read it. \
                 Export it again from your drawing app, or export a PNG."
                .into(),
            ProcessError::InvalidSvg(detail) if detail.starts_with(svg_in::NOT_UTF8) => {
                "That SVG isn't saved as UTF-8 text, which we need. Save it again with the \
                 encoding set to UTF-8 (in Illustrator's SVG Options, set Encoding to UTF-8), \
                 then upload it."
                    .into()
            }
            ProcessError::InvalidSvg(_) => "We couldn't read that SVG. Export it again as a \
                 plain SVG, or export a PNG."
                .into(),
            ProcessError::TooComplex(_) => "That's too detailed to turn into a head or tail. Use \
                 one solid dark colour with a few big cut-outs, and no texture or photo \
                 background. In a vector app, flatten effects, symbols and patterns before \
                 exporting."
                .into(),
            // When we know why nothing was left, say so. Template guides are in every
            // export from the template, so they explain it only when nothing else does.
            ProcessError::Empty { info } if info.contains(&Lint::OutsideDrawHereIgnored) => {
                "We only use your \"Draw here\" layer, and nothing in it showed up. Move your \
                 drawing into \"Draw here\", then export again."
                    .into()
            }
            // The text may be hidden, or a label beside a drawing that isn't ink: don't
            // tell the artist to outline it unless the drawing is text.
            ProcessError::Empty { info } if info.contains(&Lint::TextIgnored) => "We couldn't \
                 find a drawing. Text isn't supported: if your drawing is text, convert it to \
                 outlines (Type → Create Outlines, or Path → Object to Path). Otherwise, draw \
                 in solid black. Then export again."
                .into(),
            ProcessError::Empty { info }
                if info.iter().any(|l| matches!(l, Lint::ImageIgnored { .. })) =>
            {
                "We couldn't find a drawing: we don't read pictures embedded in an SVG. Upload \
                 your drawing as a PNG instead, or draw it with vector shapes."
                    .into()
            }
            ProcessError::Empty { info } if info.contains(&Lint::GuidesVisible) => "We only found \
                 the template guides. Draw in black on the \"Draw here\" layer, hide the Guides \
                 and Reference layers, then export again."
                .into(),
            ProcessError::Empty { .. } => "We couldn't find a drawing. Draw in solid black and \
                 export again. Light colours, and colours close to the template's blue and pink \
                 guides, are ignored."
                .into(),
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

/// What a [`Signature`] identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    Accepted(InputFormat),
    Rejected(RejectedFormat),
}

/// A magic-byte signature: for every `(offset, alternatives)` part, the bytes at
/// `offset` start with one of the alternatives.
#[derive(Debug, Clone, Copy)]
pub struct Signature {
    pub parts: &'static [(usize, &'static [&'static [u8]])],
    pub sniffed: Sniffed,
}

impl Signature {
    pub fn matches(&self, bytes: &[u8]) -> bool {
        self.parts.iter().all(|(offset, alternatives)| {
            let rest = bytes.get(*offset..).unwrap_or_default();
            alternatives.iter().any(|magic| rest.starts_with(magic))
        })
    }
}

/// ISO-BMFF major brands of HEIF/AVIF images (videos share the `ftyp` box).
const HEIF_BRANDS: &[&[u8]] = &[
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    b"avif", b"avis",
];

/// Every magic-byte signature [`sniff`] knows, first match wins. The studio page renders
/// the rejected ones for its instant in-browser check, so there is one table.
pub const SIGNATURES: &[Signature] = &[
    Signature {
        parts: &[(0, &[b"\x89PNG\r\n\x1a\n"])],
        sniffed: Sniffed::Accepted(InputFormat::Png),
    },
    Signature {
        parts: &[(0, &[&[0xff, 0xd8, 0xff]])],
        sniffed: Sniffed::Accepted(InputFormat::Jpeg),
    },
    Signature {
        parts: &[(4, &[b"ftyp"]), (8, HEIF_BRANDS)],
        sniffed: Sniffed::Rejected(RejectedFormat::Heic),
    },
    Signature {
        parts: &[(0, &[b"GIF87a", b"GIF89a"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Gif),
    },
    Signature {
        parts: &[(0, &[b"RIFF"]), (8, &[b"WEBP"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Webp),
    },
    Signature {
        parts: &[(0, &[b"8BPS"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Psd),
    },
    Signature {
        parts: &[(0, &[b"PK\x03\x04", b"PK\x05\x06"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Zip),
    },
    Signature {
        parts: &[(0, &[b"%PDF", b"%!PS"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Pdf),
    },
    Signature {
        parts: &[(0, &[&[0x1f, 0x8b]])],
        sniffed: Sniffed::Rejected(RejectedFormat::Svgz),
    },
    Signature {
        // A UTF-16 byte order mark, or `<` as UTF-16 without one.
        parts: &[(0, &[&[0xff, 0xfe], &[0xfe, 0xff], b"<\0", b"\0<"])],
        sniffed: Sniffed::Rejected(RejectedFormat::Utf16),
    },
];

/// Identify the upload from its magic bytes ([`SIGNATURES`]), else as SVG text.
pub fn sniff(bytes: &[u8]) -> Result<InputFormat, ProcessError> {
    if bytes.is_empty() {
        return Err(ProcessError::EmptyFile);
    }
    match SIGNATURES
        .iter()
        .find(|s| s.matches(bytes))
        .map(|s| s.sniffed)
    {
        Some(Sniffed::Accepted(format)) => Ok(format),
        Some(Sniffed::Rejected(r)) => Err(ProcessError::UnsupportedFormat(r)),
        None if looks_like_svg(bytes) => Ok(InputFormat::Svg),
        None => Err(ProcessError::UnknownFormat),
    }
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

/// Untrusted upload bytes -> clean shape, with `fixes` applied (a set; see [`Fix`]).
///
/// CPU-bound and synchronous (tens of milliseconds in release for a 2048 px PNG or an
/// official SVG). In production, run untrusted uploads in an isolated child process with
/// memory and CPU limits (see [`ProcessError`]), behind a semaphore.
///
/// **Needs a 64 MiB stack for untrusted input.** An SVG within every limit can still make
/// usvg recurse about [`Limits::max_svg_nesting`] levels deep (a chain of patterns or
/// masks, each wrapping nested groups): the worst accepted inputs abort on 2 and 4 MiB
/// stacks and pass on 8 MiB in release (measured), and need more in a debug build. On a default 2 MiB thread (Tokio workers and `spawn_blocking` threads
/// included) that overflows the stack, which aborts the whole process. Production code
/// should call [`process_on_big_stack`] (inside that child process); calling this
/// directly is fine for trusted input such as tests of the official assets.
pub fn process_upload(
    bytes: &[u8],
    limits: &Limits,
    fixes: &[Fix],
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
    match input {
        InputFormat::Png | InputFormat::Jpeg => {
            let traced = raster_in::process(bytes, input, limits)?;
            finish(
                traced.path,
                FillRule::EvenOdd,
                Strategy::Traced,
                input,
                traced.info,
                fixes,
                limits,
            )
        }
        InputFormat::Svg => {
            // usvg and tiny-skia are not ours; a panic on some odd input is a bug, not a
            // reason to take the caller down.
            let shape = std::panic::catch_unwind(|| svg_in::process(bytes, limits))
                .map_err(|_| ProcessError::Internal("the SVG reader panicked"))??;
            finish(
                shape.path,
                shape.fill_rule,
                shape.strategy,
                input,
                shape.info,
                fixes,
                limits,
            )
        }
    }
}

/// Stack for [`process_on_big_stack`]'s thread. The deepest SVG the limits allow needs
/// between 4 and 8 MiB in release; this leaves room for debug builds. Only touched pages
/// are committed, so an ordinary upload uses a few hundred KB of it.
pub const PROCESS_STACK_BYTES: usize = 64 << 20;

/// [`process_upload`] on a dedicated, named thread with a [`PROCESS_STACK_BYTES`] stack.
///
/// `permit` (e.g. an `OwnedSemaphorePermit`) is moved into the thread and dropped only
/// when the work has finished, before the result is sent, so a caller that stops waiting
/// (a timeout) doesn't release capacity while the CPU is still busy. A panic is reported
/// as [`ProcessError::Internal`]; so is failing to start the thread.
pub fn process_on_big_stack(
    bytes: Vec<u8>,
    limits: &Limits,
    fixes: &[Fix],
    permit: impl Send + 'static,
) -> tokio::sync::oneshot::Receiver<Result<CleanShape, ProcessError>> {
    let limits = limits.clone();
    let fixes = fixes.to_vec();
    spawn_processing(
        PROCESS_STACK_BYTES,
        move || process_upload(&bytes, &limits, &fixes),
        permit,
    )
}

/// Run `work` on a new thread with a `stack_bytes` stack, holding `permit` until it ends.
fn spawn_processing<W>(
    stack_bytes: usize,
    work: W,
    permit: impl Send + 'static,
) -> tokio::sync::oneshot::Receiver<Result<CleanShape, ProcessError>>
where
    W: FnOnce() -> Result<CleanShape, ProcessError> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("design-kit".into())
        .stack_size(stack_bytes)
        .spawn(move || {
            // Nothing the work touched outlives a panic: its result is replaced.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or(Err(ProcessError::Internal("processing panicked")));
            drop(permit);
            // The receiver may be gone (the caller timed out); nothing to do then.
            let _ = tx.send(result);
        });
    match spawned {
        Ok(_detached) => rx,
        Err(_) => {
            // The closure (and with it the permit and the sender) was dropped.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(Err(ProcessError::Internal(
                "could not start the processing thread",
            )));
            rx
        }
    }
}

/// Shared tail of every pipeline: path in 0..100 -> fix -> `d` + metrics + lints.
fn finish(
    path: tiny_skia::Path,
    fill_rule: FillRule,
    strategy: Strategy,
    input: InputFormat,
    info: Vec<Lint>,
    fixes: &[Fix],
    limits: &Limits,
) -> Result<CleanShape, ProcessError> {
    let mut path = path;
    for f in [Fix::Flip, Fix::Fit] {
        if fixes.contains(&f) {
            path = fix::apply(path, f);
        }
    }
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
    fn a_thread_that_cannot_start_is_an_internal_error() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Permit(Arc<AtomicBool>);
        impl Drop for Permit {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        // No system can map a 1 PiB stack.
        let rx = spawn_processing(
            1 << 50,
            || process_upload(b"<svg/>", &Limits::default(), &[]),
            Permit(released.clone()),
        );
        let result = rx.blocking_recv().expect("a result is sent");
        assert_eq!(
            result.err(),
            Some(ProcessError::Internal(
                "could not start the processing thread"
            ))
        );
        assert!(released.load(Ordering::SeqCst), "the permit is released");
    }

    #[test]
    fn the_permit_is_held_until_the_work_ends_even_when_the_caller_stops_waiting() {
        use std::sync::mpsc;
        use std::time::Duration;
        /// Reports its drop.
        struct Permit(mpsc::Sender<()>);
        impl Drop for Permit {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let wait = Duration::from_secs(10);

        // Work that runs until we let it finish.
        let (released_tx, released) = mpsc::channel();
        let (started_tx, started) = mpsc::channel();
        let (finish_tx, finish) = mpsc::channel::<()>();
        let rx = spawn_processing(
            1 << 20,
            move || {
                let _ = started_tx.send(());
                let _ = finish.recv();
                Err(ProcessError::EmptyFile)
            },
            Permit(released_tx),
        );
        started.recv_timeout(wait).expect("the work starts");
        assert!(released.try_recv().is_err(), "held while the work runs");
        // The caller times out and stops waiting; the CPU is still busy.
        drop(rx);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            released.try_recv().is_err(),
            "held after the caller stops waiting"
        );
        finish_tx.send(()).expect("the work is waiting");
        released
            .recv_timeout(wait)
            .expect("released when the work ends");

        // A caller that waits gets the result after the permit is gone.
        let (released_tx, released) = mpsc::channel();
        let rx = spawn_processing(
            1 << 20,
            || Err(ProcessError::EmptyFile),
            Permit(released_tx),
        );
        assert_eq!(
            rx.blocking_recv().expect("a result").err(),
            Some(ProcessError::EmptyFile)
        );
        assert!(
            released.try_recv().is_ok(),
            "released before the result is sent"
        );
    }

    #[test]
    fn compressed_and_utf16_svgs_get_their_own_advice() {
        assert_eq!(
            sniff(&[0x1f, 0x8b, 0x08, 0x00]),
            Err(ProcessError::UnsupportedFormat(RejectedFormat::Svgz))
        );
        let utf16: Vec<u8> = "\u{feff}<svg/>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            sniff(&utf16),
            Err(ProcessError::UnsupportedFormat(RejectedFormat::Utf16))
        );
        let utf16be: Vec<u8> = "<svg/>".encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(
            sniff(&utf16be),
            Err(ProcessError::UnsupportedFormat(RejectedFormat::Utf16))
        );
        for f in [RejectedFormat::Svgz, RejectedFormat::Utf16] {
            let msg = ProcessError::UnsupportedFormat(f).user_message();
            assert!(msg.contains("SVG"), "{msg}");
        }
        assert!(
            !ProcessError::InvalidSvg("x".into())
                .user_message()
                .contains("svgz")
        );
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(4 * 1024 * 1024), "4 MB");
        assert_eq!(human_bytes(512 * 1024), "512 KB");
    }
}
